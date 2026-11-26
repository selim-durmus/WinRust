//! What is inside each folder of the listing (bytes, subfolders and files, all the way down), counted in the background,
//! like File Pilot's Folders / Files / Size columns. Totals show while they grow.
//!
//! - Counting: a few threads share one queue of folders. Each thread starts on a folder of its own, so small folders are
//!   done at once; once every folder has a thread, the idle ones help the biggest (C:\Users alone used to take one thread
//!   8.5 s, the whole of C:\ now takes about 3.5 s).
//! - Every folder walked on the way keeps its own totals, so opening a subfolder of a counted one shows its totals at
//!   once, and counting a parent reuses the subfolders already known.
//! - Each local fixed drive is watched for changes (one ReadDirectoryChangesW on its root, subtree): a change re-lists
//!   just the folder it happened in and moves the difference up through the counted folders above it. Totals on a
//!   watched drive therefore stay right for the whole session; elsewhere they are trusted for two minutes.
//!
//! Folders on network drives are left alone.
#![allow(unsafe_op_in_unsafe_fn)]
use std::{
    collections::{HashMap, HashSet},
    hash::{BuildHasherDefault, Hasher},
    path::{Path, PathBuf},
    sync::{Condvar, LazyLock, Mutex, MutexGuard, OnceLock, atomic::{AtomicU32, AtomicU64, Ordering::Relaxed}},
    time::{Duration, Instant},
};

pub const WM_SIZES: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 10; // some totals changed: repaint

/// Totals of one folder. `done` false = still counting (what is shown is a lower bound).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Sum { pub bytes: u64, pub dirs: u32, pub files: u32, pub done: bool }

impl Sum {
    /// Adds subfolder `c`: the folder itself and everything below it.
    fn add_child(&mut self, c: &Sum) {
        self.bytes += c.bytes;
        self.files += c.files;
        self.dirs += 1 + c.dirs;
    }
}

const KEEP: u32 = 120; // seconds a total is trusted on a drive that is not watched
const WORKERS: usize = 4; // more threads list no faster: the file system is the limit

// ---------- what is known: every counted folder ----------

/// What lies directly in a folder: its files and their bytes, and the links to folders (counted, not walked).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Own { bytes: u64, files: u32, links: u32 }

#[derive(Clone, Copy, Debug)]
struct Node { sum: Sum, parent: u64, at: u32, drive: u8 }

/// The keys are hashes already.
#[derive(Default)]
struct IdHash(u64);
impl Hasher for IdHash {
    fn finish(&self) -> u64 { self.0 }
    fn write(&mut self, b: &[u8]) { for &x in b { self.0 = self.0.rotate_left(8) ^ x as u64; } }
    fn write_u64(&mut self, n: u64) { self.0 = n; }
}
type Map = HashMap<u64, Node, BuildHasherDefault<IdHash>>;

static STORE: LazyLock<Mutex<Map>> = LazyLock::new(|| Mutex::new(Map::default()));

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> { m.lock().unwrap_or_else(|e| e.into_inner()) }

/// The key of `name` in the folder with key `parent` (names compared without case, like Windows).
fn mix(parent: u64, name: &str) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    h.write_u64(parent);
    if name.is_ascii() { h.write(name.to_ascii_lowercase().as_bytes()) } else { h.write(name.to_lowercase().as_bytes()) }
    h.write_u8(0xFF);
    h.finish()
}

const SEED: u64 = 0x5EED_F01D;

/// The key of folder `p` (built part by part, the same way the walk builds its subfolders' keys).
fn key(p: &Path) -> u64 { p.to_string_lossy().split(['\\', '/']).filter(|s| !s.is_empty()).fold(SEED, mix) }

/// Drive number of `p` (0 = A:), 255 when it has none.
fn drive(p: &Path) -> u8 {
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() { b[0].to_ascii_uppercase() - b'A' } else { 255 }
}

static START: OnceLock<Instant> = OnceLock::new();
fn now() -> u32 { START.get_or_init(Instant::now).elapsed().as_secs() as u32 }

static WATCHING: AtomicU32 = AtomicU32::new(0); // drives whose changes are followed (bit = drive)
static STARTED: AtomicU32 = AtomicU32::new(0); // drives a watch was tried on

fn watched(d: u8) -> bool { d < 32 && WATCHING.load(Relaxed) & (1 << d) != 0 }

/// Can this total be shown as it is (no need to count again)?
fn fresh(n: &Node) -> bool { n.sum.done && (watched(n.drive) || now().saturating_sub(n.at) < KEEP) }

/// The totals of folder `p` so far (None = not counted, or not yet started).
pub fn get(p: &Path) -> Option<Sum> { lock(&STORE).get(&key(p)).map(|n| n.sum) }

// ---------- counting ----------

/// One folder of a walk: what lies in it, what its finished subfolders add up to, how many are still being counted.
struct WNode { key: u64, parent: u32, own: Own, acc: Sum, wait: u32 }

/// Counting one folder (asked for by the listing) down to the bottom.
struct Walk {
    id: u64,
    root: PathBuf,
    root_parent: u64,
    drive: u8,
    nodes: Vec<WNode>,
    todo: Vec<(u32, u64, PathBuf)>, // folders still to list: (node, key, path)
    busy: u32,                      // folders of it being listed right now
    seen: Sum,                      // everything found so far, shown while counting
    shown: Instant,
}

const ROOT: u32 = u32::MAX;

impl Walk {
    fn new(id: u64, root: &Path) -> Walk {
        let k = key(root);
        Walk {
            id,
            root: root.to_path_buf(),
            root_parent: root.parent().map_or(0, key),
            drive: drive(root),
            nodes: vec![WNode { key: k, parent: ROOT, own: Own::default(), acc: Sum::default(), wait: 1 }],
            todo: vec![(0, k, root.to_path_buf())],
            busy: 0,
            seen: Sum::default(),
            shown: Instant::now(),
        }
    }

    /// Folder `n` was listed: `own` lies in it directly, `kids` are its subfolders (key, path). Subfolders known already
    /// count at once; the others are queued. Returns the folders this finished (every one whose subfolders are all
    /// counted), for the store, and whether the whole walk is done.
    fn listed(&mut self, n: u32, own: Own, kids: Vec<(u64, PathBuf)>, store: &Map) -> (Vec<(u64, Node)>, bool) {
        let at = now();
        let n = n as usize;
        self.nodes[n].own = own;
        self.seen.files += own.files;
        self.seen.bytes += own.bytes;
        self.seen.dirs += own.links + kids.len() as u32;
        for (k, p) in kids {
            if let Some(c) = store.get(&k).filter(|c| fresh(c)) {
                self.nodes[n].acc.add_child(&c.sum);
                self.seen.files += c.sum.files;
                self.seen.bytes += c.sum.bytes;
                self.seen.dirs += c.sum.dirs;
            } else {
                let i = self.nodes.len() as u32;
                self.nodes.push(WNode { key: k, parent: n as u32, own: Own::default(), acc: Sum::default(), wait: 1 });
                self.todo.push((i, k, p));
                self.nodes[n].wait += 1;
            }
        }
        self.nodes[n].wait -= 1; // its own listing
        // the folders finished now, from this one up
        let (mut fin, mut i) = (vec![], n);
        while self.nodes[i].wait == 0 {
            let nd = &self.nodes[i];
            let mut sum = nd.acc;
            sum.bytes += nd.own.bytes;
            sum.files += nd.own.files;
            sum.dirs += nd.own.links;
            sum.done = true;
            let parent = if nd.parent == ROOT { self.root_parent } else { self.nodes[nd.parent as usize].key };
            fin.push((nd.key, Node { sum, parent, at, drive: self.drive }));
            if nd.parent == ROOT { return (fin, true); }
            let p = nd.parent as usize;
            self.nodes[p].acc.add_child(&sum);
            self.nodes[p].wait -= 1;
            i = p;
        }
        (fin, false)
    }
}

/// Lists folder `p`: what lies in it directly, and its subfolders (links to folders are counted, not followed).
fn list(p: &Path, k: u64) -> (Own, Vec<(u64, PathBuf)>) {
    let (mut own, mut kids) = (Own::default(), vec![]);
    crate::fsx::each(p, |f| {
        if f.attr & crate::fsx::DIR != 0 {
            if f.attr & 0x400 != 0 { own.links += 1; } else {
                let name = String::from_utf16_lossy(f.name);
                kids.push((mix(k, &name), p.join(name)));
            }
        } else {
            own.files += 1;
            own.bytes += f.size;
        }
    });
    (own, kids)
}

/// Counts `root` on this thread (a folder that appeared on a watched drive), keeping every folder's totals.
fn walk_here(root: &Path) -> Option<Sum> {
    let mut w = Walk::new(0, root);
    while let Some((n, k, p)) = w.todo.pop() {
        let (own, kids) = list(&p, k);
        let mut st = lock(&STORE);
        let (fin, done) = w.listed(n, own, kids, &st);
        st.extend(fin);
        if done { break; }
    }
    get(root)
}

struct Sched { walks: Vec<Walk>, next: u64 }
static SCHED: LazyLock<(Mutex<Sched>, Condvar)> = LazyLock::new(|| (Mutex::new(Sched { walks: vec![], next: 1 }), Condvar::new()));
static MAIN: AtomicU64 = AtomicU64::new(0); // the window told when totals change
static LAST_NOTE: AtomicU64 = AtomicU64::new(0);

/// Counts `dirs` (the folders of the listing just shown), dropping the counts still running for the listing before
/// (what they finished is kept, and used again).
pub fn ask(dirs: Vec<PathBuf>) {
    let (m, cv) = &*SCHED;
    let mut s = lock(m);
    s.walks.clear();
    add(&mut s, dirs);
    cv.notify_all();
}

/// Counts `dirs` too, after what is already queued (folders opened in the tree), without dropping anything.
pub fn more(dirs: Vec<PathBuf>) {
    let (m, cv) = &*SCHED;
    let mut s = lock(m);
    add(&mut s, dirs);
    cv.notify_all();
}

fn add(s: &mut Sched, dirs: Vec<PathBuf>) {
    for d in dirs {
        if remote(&d) || s.walks.iter().any(|w| w.root == d) { continue; }
        if lock(&STORE).get(&key(&d)).is_some_and(fresh) { continue; }
        watch(drive(&d));
        let id = s.next;
        s.next += 1;
        s.walks.push(Walk::new(id, &d));
    }
}

/// The next folder for a counting thread: one of the walk it is on; else a walk nobody is on yet (in the order asked);
/// else one of the walk with the most folders waiting.
fn take(s: &mut Sched, mine: &mut u64) -> Option<(u64, u32, u64, PathBuf)> {
    let i = s.walks.iter().position(|w| w.id == *mine && !w.todo.is_empty())
        .or_else(|| s.walks.iter().position(|w| w.busy == 0 && !w.todo.is_empty()))
        .or_else(|| s.walks.iter().enumerate().filter(|(_, w)| !w.todo.is_empty()).max_by_key(|(_, w)| w.todo.len()).map(|(i, _)| i))?;
    let w = &mut s.walks[i];
    let (n, k, p) = w.todo.pop()?;
    w.busy += 1;
    *mine = w.id;
    Some((w.id, n, k, p))
}

/// Starts the counting threads; `main` gets WM_SIZES (at most every 200 ms) while totals change.
pub fn start(main: isize) {
    MAIN.store(main as u64, Relaxed);
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_err() { return; }
    for _ in 0..WORKERS { std::thread::spawn(worker); }
}

fn worker() {
    let (m, cv) = &*SCHED;
    let mut mine = 0;
    loop {
        let (id, n, k, p) = {
            let mut s = lock(m);
            loop {
                if let Some(j) = take(&mut s, &mut mine) { break j; }
                s = cv.wait(s).unwrap_or_else(|e| e.into_inner());
            }
        };
        let (own, kids) = list(&p, k);
        let mut s = lock(m);
        let Some(i) = s.walks.iter().position(|w| w.id == id) else { continue }; // dropped meanwhile (another listing)
        let w = &mut s.walks[i];
        w.busy -= 1;
        let mut st = lock(&STORE);
        let (fin, done) = w.listed(n, own, kids, &st);
        st.extend(fin);
        if done {
            drop(st);
            s.walks.remove(i);
            note();
        } else {
            if w.shown.elapsed() > Duration::from_millis(150) {
                // the total so far, shown while it grows
                w.shown = Instant::now();
                st.insert(w.nodes[0].key, Node { sum: w.seen, parent: w.root_parent, at: now(), drive: w.drive });
                drop(st);
                note();
            }
            if !w.todo.is_empty() { cv.notify_all(); }
        }
    }
}

/// Network folders are not counted: walking a share would be slow and load the server.
fn remote(p: &Path) -> bool {
    let s = p.to_string_lossy();
    if s.starts_with(r"\\") { return true; }
    let root: Vec<u16> = s.chars().take(3).collect::<String>().encode_utf16().chain([0]).collect();
    unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) == 4 }
}

fn note() {
    let t = Instant::now().duration_since(*START.get_or_init(Instant::now)).as_millis() as u64;
    if t.saturating_sub(LAST_NOTE.load(Relaxed)) < 200 { return; }
    LAST_NOTE.store(t, Relaxed);
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(MAIN.load(Relaxed) as _, WM_SIZES, 0, 0) };
}

// ---------- keeping the totals right: the drives' changes ----------

/// Follows the changes of drive `d` (fixed drives only: a handle on a USB stick's root would keep it from being ejected).
fn watch(d: u8) {
    if d >= 26 || STARTED.fetch_or(1 << d, Relaxed) & (1 << d) != 0 { return; }
    let root: Vec<u16> = format!("{}:\\", (b'A' + d) as char).encode_utf16().chain([0]).collect();
    if unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) } != 3 { return; }
    std::thread::spawn(move || unsafe { watch_drive(d) });
}

unsafe fn watch_drive(d: u8) {
    use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*, System::{IO::*, Threading::*}};
    let base = PathBuf::from(format!("{}:\\", (b'A' + d) as char));
    let root: Vec<u16> = format!("{}:\\", (b'A' + d) as char).encode_utf16().chain([0]).collect();
    let h = CreateFileW(root.as_ptr(), FILE_LIST_DIRECTORY, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED, std::ptr::null_mut());
    if h == INVALID_HANDLE_VALUE { return; }
    let ev = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
    let mut buf = vec![0u64; 8192]; // 64 KB, the most a watch can take
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SIZE;
    let (mut dirty, mut dead): (HashSet<PathBuf>, HashSet<u64>) = (HashSet::new(), HashSet::new());
    let mut since: Option<Instant> = None; // the first change not handled yet
    WATCHING.fetch_or(1 << d, Relaxed);
    'watch: loop {
        let mut ov: OVERLAPPED = std::mem::zeroed();
        ov.hEvent = ev;
        ResetEvent(ev);
        if ReadDirectoryChangesW(h, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, 1, filter, std::ptr::null_mut(), &mut ov, None) == 0 { break; }
        // changes are handled together, 300 ms after the first of them (a copy makes thousands)
        loop {
            let wait = since.map_or(INFINITE, |t| 300u32.saturating_sub(t.elapsed().as_millis() as u32));
            if WaitForSingleObject(ev, wait) == WAIT_OBJECT_0 { break; }
            handle(&mut dirty, &mut dead);
            since = None;
        }
        let mut got = 0u32;
        let ok = GetOverlappedResult(h, &ov, &mut got, 0) != 0;
        if !ok && GetLastError() != ERROR_NOTIFY_ENUM_DIR { break 'watch; }
        if got == 0 {
            // more changes than the buffer holds: this drive's totals are counted again when they are next shown
            forget(d);
            (dirty, dead) = (HashSet::new(), HashSet::new());
            note();
            continue;
        }
        let base_ptr = buf.as_ptr() as *const u8;
        let mut off = 0usize;
        loop {
            let e = base_ptr.add(off);
            let next = *(e as *const u32);
            let action = *(e.add(4) as *const u32);
            let len = *(e.add(8) as *const u32) as usize / 2;
            let name = String::from_utf16_lossy(std::slice::from_raw_parts(e.add(12) as *const u16, len));
            let full = base.join(&name);
            if action == FILE_ACTION_REMOVED || action == FILE_ACTION_RENAMED_OLD_NAME { dead.insert(key(&full)); }
            if let Some(p) = full.parent() { dirty.insert(p.to_path_buf()); }
            if next == 0 { break; }
            off += next as usize;
        }
        since.get_or_insert_with(Instant::now);
    }
    WATCHING.fetch_and(!(1 << d), Relaxed); // its totals are trusted for two minutes again
    CloseHandle(ev);
    CloseHandle(h);
}

/// The changes gathered: folders gone are forgotten (with everything below them), the folders things changed in are
/// listed again.
fn handle(dirty: &mut HashSet<PathBuf>, dead: &mut HashSet<u64>) {
    if !dead.is_empty() { purge(std::mem::take(dead)); }
    let mut any = false;
    for p in dirty.drain() { any |= refresh(&p); }
    if any { note(); }
}

/// Forgets folders `dead` and everything below them (a folder of the same name made later is counted afresh).
fn purge(dead: HashSet<u64>) {
    let mut st = lock(&STORE);
    let mut dead: HashSet<u64> = dead.into_iter().filter(|k| st.remove(k).is_some()).collect();
    while !dead.is_empty() {
        let below: Vec<u64> = st.iter().filter(|(_, n)| dead.contains(&n.parent)).map(|(k, _)| *k).collect();
        for k in &below { st.remove(k); }
        dead = below.into_iter().collect();
    }
}

/// Forgets every total of drive `d`.
fn forget(d: u8) { lock(&STORE).retain(|_, n| n.drive != d); }

/// Something changed in counted folder `p`: it is listed again (a new subfolder counted), and the difference goes up
/// through every counted folder above it. true = a total changed.
fn refresh(p: &Path) -> bool {
    let k = key(p);
    let Some(old) = lock(&STORE).get(&k).copied().filter(|n| n.sum.done) else { return false };
    if !p.is_dir() { return false; }
    let (own, kids) = list(p, k);
    let new: Vec<PathBuf> = { let st = lock(&STORE); kids.iter().filter(|(c, _)| !st.get(c).is_some_and(|n| n.sum.done)).map(|(_, q)| q.clone()).collect() };
    for q in new { walk_here(&q); }
    let mut st = lock(&STORE);
    let mut sum = Sum { bytes: own.bytes, files: own.files, dirs: own.links, done: true };
    for (c, _) in &kids { if let Some(n) = st.get(c) { sum.add_child(&n.sum); } }
    st.insert(k, Node { sum, at: now(), ..old });
    let (db, df, dd) = (sum.bytes as i64 - old.sum.bytes as i64, sum.files as i64 - old.sum.files as i64, sum.dirs as i64 - old.sum.dirs as i64);
    if (db, df, dd) == (0, 0, 0) { return false; }
    let mut a = old.parent;
    for _ in 0..4096 {
        let Some(n) = st.get_mut(&a) else { break };
        n.sum.bytes = (n.sum.bytes as i64 + db).max(0) as u64;
        n.sum.files = (n.sum.files as i64 + df).max(0) as u32;
        n.sum.dirs = (n.sum.dirs as i64 + dd).max(0) as u32;
        a = n.parent;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rx-sizes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(r"a\b")).unwrap();
        std::fs::write(d.join("x.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(d.join(r"a\b\y.bin"), vec![0u8; 24]).unwrap();
        d
    }

    /// Timing on a real disk: `cargo test -p rust-explorer --release size_speed -- --ignored --nocapture` (RX_ROOT=folder).
    #[test]
    #[ignore]
    fn size_speed() {
        let root = PathBuf::from(std::env::var("RX_ROOT").unwrap_or_else(|_| "C:\\".into()));
        let mut dirs = vec![];
        crate::fsx::each(&root, |f| if f.attr & crate::fsx::DIR != 0 { dirs.push(root.join(String::from_utf16_lossy(f.name))); });
        start(0);
        let t0 = Instant::now();
        ask(dirs.clone());
        let mut left: Vec<PathBuf> = dirs.clone();
        let mut done_at = vec![];
        while !left.is_empty() && t0.elapsed().as_secs() < 120 {
            left.retain(|d| if get(d).is_some_and(|s| s.done) { done_at.push((t0.elapsed().as_millis(), d.clone())); false } else { true });
            std::thread::sleep(Duration::from_millis(1));
        }
        let all = t0.elapsed();
        let half = done_at.get(done_at.len() / 2).map_or(0, |x| x.0);
        println!("{}: {} folders all counted in {all:?}, half of them by {half} ms; last: {:?}", root.display(), dirs.len(), done_at.last());
        let t1 = Instant::now();
        ask(dirs.clone());
        let again = dirs.iter().all(|d| get(d).is_some_and(|s| s.done));
        println!("asked again: all there at once = {again} ({:?})", t1.elapsed());
        let deep = std::env::var("USERPROFILE").map(PathBuf::from).unwrap_or_default();
        println!("{} known without counting: {:?}", deep.display(), get(&deep));
        let n = lock(&STORE).len();
        println!("folders kept: {n} (~{} MB)", n * (std::mem::size_of::<(u64, Node)>() + 8) >> 20);
    }

    #[test]
    fn keys_ignore_case_and_trailing_slashes() {
        assert_eq!(key(Path::new(r"C:\Users\Tutto")), key(Path::new(r"c:\users\TUTTO\")));
        assert_eq!(key(Path::new(r"C:\Users\Tutto")), mix(key(Path::new(r"C:\Users")), "tutto"));
        assert_ne!(key(Path::new(r"C:\ab\c")), key(Path::new(r"C:\a\bc")));
        assert_eq!(drive(Path::new(r"d:\x")), 3);
    }

    #[test]
    fn counts_everything_below_a_folder_and_keeps_each_subfolder() {
        let d = temp("walk");
        let s = walk_here(&d).unwrap();
        assert_eq!(s, Sum { bytes: 1024, dirs: 2, files: 2, done: true });
        assert_eq!(get(&d.join("a")).unwrap(), Sum { bytes: 24, dirs: 1, files: 1, done: true });
        assert_eq!(get(&d.join(r"a\b")).unwrap(), Sum { bytes: 24, dirs: 0, files: 1, done: true });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_counting_threads_get_the_same_totals() {
        let d = temp("threads");
        start(0);
        more(vec![d.clone()]);
        let t0 = Instant::now();
        while !get(&d).is_some_and(|s| s.done) && t0.elapsed().as_secs() < 10 { std::thread::sleep(Duration::from_millis(5)); }
        assert_eq!(get(&d).unwrap(), Sum { bytes: 1024, dirs: 2, files: 2, done: true });
        assert_eq!(get(&d.join(r"a\b")).unwrap().bytes, 24);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_drive_watch_keeps_totals_right_by_itself() {
        let d = temp("watch");
        start(0);
        more(vec![d.clone()]);
        let wait_for = |want: u64| {
            let t0 = Instant::now();
            while get(&d).is_none_or(|s| !s.done || s.bytes != want) && t0.elapsed().as_secs() < 10 { std::thread::sleep(Duration::from_millis(20)); }
            get(&d).map(|s| s.bytes)
        };
        assert_eq!(wait_for(1024), Some(1024));
        std::fs::write(d.join(r"a\b\more.bin"), vec![0u8; 500]).unwrap();
        assert_eq!(wait_for(1524), Some(1524)); // nobody asked: the watch saw it
        assert_eq!(get(&d.join("a")).unwrap().files, 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_change_moves_up_through_the_folders_above() {
        let d = temp("change");
        walk_here(&d).unwrap();
        // a file grows deep down, a folder with a file appears in the middle
        std::fs::write(d.join(r"a\b\y.bin"), vec![0u8; 124]).unwrap();
        std::fs::create_dir_all(d.join(r"a\new")).unwrap();
        std::fs::write(d.join(r"a\new\z.bin"), vec![0u8; 10]).unwrap();
        // (the drive's watch may get there first: the totals are what counts)
        refresh(&d.join(r"a\b"));
        refresh(&d.join("a"));
        assert_eq!(get(&d.join("a")).unwrap(), Sum { bytes: 134, dirs: 2, files: 2, done: true });
        assert_eq!(get(&d).unwrap(), Sum { bytes: 1134, dirs: 3, files: 3, done: true });
        // a folder goes: it and what was below it are forgotten, the totals above drop
        std::fs::remove_dir_all(d.join(r"a\b")).unwrap();
        purge([key(&d.join(r"a\b"))].into());
        assert!(get(&d.join(r"a\b")).is_none());
        refresh(&d.join("a"));
        assert_eq!(get(&d).unwrap(), Sum { bytes: 1010, dirs: 2, files: 2, done: true });
        let _ = std::fs::remove_dir_all(&d);
    }
}
