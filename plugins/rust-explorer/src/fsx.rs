//! Fast file system access for the window.
//! - One folder: FindFirstFileExW with basic info and large fetch (no 8.3 names, bigger buffers), no extra call per file.
//! - A whole folder tree (the filter with "Subfolders" on): several threads crawl it into a compact in-memory index (one
//!   string arena for the names, 32 bytes per entry), and every key press scans that index, which takes milliseconds.
//!   Results show while the crawl is still running.
use std::{collections::VecDeque, os::windows::ffi::OsStrExt, path::{Path, PathBuf}, ptr, sync::{Arc, Condvar, Mutex, RwLock, atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed}}, time::Instant};
use windows_sys::Win32::{Foundation::{ERROR_FILE_NOT_FOUND, FILETIME, GetLastError, INVALID_HANDLE_VALUE}, Storage::FileSystem::*};

pub const DIR: u32 = 0x10;
pub const HIDDEN: u32 = 0x2;
const REPARSE: u32 = 0x400; // junctions and symbolic links: never followed by the crawl (loops, the same files twice)

/// One entry of a folder as Windows reports it while listing (nothing else is read).
pub struct Found<'a> { pub name: &'a [u16], pub attr: u32, pub size: u64, pub time: u64 }

/// `dir\*` for FindFirstFileExW; past MAX_PATH the `\\?\` form, which has no length limit.
fn pattern(dir: &Path) -> Vec<u16> {
    let mut p: Vec<u16> = dir.as_os_str().encode_wide().collect();
    if p.len() > 240 && !p.starts_with(&[92, 92, 63, 92]) {
        p = if p.starts_with(&[92, 92]) { r"\\?\UNC".encode_utf16().chain(p[1..].iter().copied()).collect() } else { r"\\?\".encode_utf16().chain(p).collect() };
    }
    if p.last() != Some(&92) { p.push(92); }
    p.extend([42, 0]);
    p
}

fn ticks(t: FILETIME) -> u64 { (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64 }

/// Calls `f` for every entry of `dir` ("." and ".." left out). false = the folder cannot be read (gone, no access).
pub fn each(dir: &Path, mut f: impl FnMut(Found)) -> bool {
    let pat = pattern(dir);
    unsafe {
        let mut fd: WIN32_FIND_DATAW = std::mem::zeroed();
        let h = FindFirstFileExW(pat.as_ptr(), FindExInfoBasic, &mut fd as *mut _ as *mut _, FindExSearchNameMatch, ptr::null(), FIND_FIRST_EX_LARGE_FETCH);
        if h == INVALID_HANDLE_VALUE { return GetLastError() == ERROR_FILE_NOT_FOUND; } // an empty drive has no entries at all
        loop {
            let n = fd.cFileName.iter().position(|&c| c == 0).unwrap_or(fd.cFileName.len());
            let name = &fd.cFileName[..n];
            if name != [46] && name != [46, 46] {
                f(Found { name, attr: fd.dwFileAttributes, size: (fd.nFileSizeHigh as u64) << 32 | fd.nFileSizeLow as u64, time: ticks(fd.ftLastWriteTime) });
            }
            if FindNextFileW(h, &mut fd) == 0 { break; }
        }
        FindClose(h);
    }
    true
}

/// Case-insensitive "name contains t" (`t` already lower case). ASCII names, nearly all of them, are compared byte by
/// byte without allocating.
pub fn contains_ci(name: &str, t: &str) -> bool {
    if t.is_empty() { return true; }
    if !(name.is_ascii() && t.is_ascii()) { return name.to_lowercase().contains(t); }
    let (h, n) = (name.as_bytes(), t.as_bytes());
    if n.len() > h.len() { return false; }
    'at: for i in 0..=h.len() - n.len() {
        if h[i].to_ascii_lowercase() != n[0] { continue; }
        for j in 1..n.len() {
            if h[i + j].to_ascii_lowercase() != n[j] { continue 'at; }
        }
        return true;
    }
    false
}

// ---------- the crawl ----------

const E_DIR: u8 = 1;
const E_HIDDEN: u8 = 2; // hidden itself
const E_UNDER: u8 = 4; // inside a hidden folder (AppData, .git): left out with the hidden ones

struct Ent { off: u32, len: u16, flags: u8, parent: u32, size: u64, time: u64 }

/// Everything found below the root so far. `dirs[d]` is the entry of folder number d (0 = the root itself).
pub struct Tree { names: String, ents: Vec<Ent>, dirs: Vec<u32> }

impl Tree {
    fn name(&self, e: &Ent) -> &str { &self.names[e.off as usize..e.off as usize + e.len as usize] }
    pub fn len(&self) -> usize { self.ents.len() }
    /// (folder, size, modified) of entry `i`.
    pub fn meta(&self, i: usize) -> (bool, u64, u64) { let e = &self.ents[i]; (e.flags & E_DIR != 0, e.size, e.time) }
    /// Full path of entry `i`.
    pub fn path(&self, root: &Path, i: usize) -> PathBuf {
        let mut parts = vec![self.name(&self.ents[i])];
        let mut d = self.ents[i].parent;
        while d != 0 {
            let e = &self.ents[self.dirs[d as usize] as usize];
            parts.push(self.name(e));
            d = e.parent;
        }
        let mut p = root.to_path_buf();
        for s in parts.iter().rev() { p.push(s); }
        p
    }
    /// Entries whose name holds every term (lower case); hidden ones and what lies in hidden folders only with `hidden`.
    /// Returns up to `max` entry numbers and the total count.
    pub fn find(&self, terms: &[String], hidden: bool, max: usize) -> (Vec<usize>, usize) {
        let (mut out, mut total) = (vec![], 0);
        for (i, e) in self.ents.iter().enumerate() {
            if !hidden && e.flags & (E_HIDDEN | E_UNDER) != 0 { continue; }
            let n = self.name(e);
            if terms.iter().all(|t| contains_ci(n, t)) {
                total += 1;
                if out.len() < max { out.push(i); }
            }
        }
        (out, total)
    }
}

/// A crawl of one folder tree, running or finished. Dropping the last handle does not stop it; `stop` does.
pub struct Crawl {
    pub root: PathBuf,
    pub tree: RwLock<Tree>,
    pub started: Instant,
    done: AtomicBool,
    stop: AtomicBool,
    work: (Mutex<(VecDeque<(u32, PathBuf, bool)>, usize)>, Condvar), // (folders waiting, folders being listed)
}

impl Crawl {
    pub fn is_done(&self) -> bool { self.done.load(Relaxed) }
    pub fn stop(&self) {
        self.stop.store(true, Relaxed);
        self.work.1.notify_all();
    }

    /// Starts crawling `root` on a few threads. `notify` is called now and then while entries come in (at most every
    /// 150 ms) and once at the end.
    pub fn start(root: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Arc<Crawl> {
        let c = Arc::new(Crawl {
            root: root.clone(),
            tree: RwLock::new(Tree { names: String::new(), ents: vec![], dirs: vec![u32::MAX] }),
            started: Instant::now(),
            done: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            work: (Mutex::new((VecDeque::from([(0, root, false)]), 0)), Condvar::new()),
        });
        let notify = Arc::new(notify);
        let n = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8);
        let alive = Arc::new(AtomicUsize::new(n));
        let last = Arc::new(AtomicU64::new(0));
        for _ in 0..n {
            let (c, notify, alive, last) = (c.clone(), notify.clone(), alive.clone(), last.clone());
            std::thread::spawn(move || {
                while let Some((id, path, under)) = c.next_job() {
                    let mut batch: Vec<(String, u32, u64, u64)> = vec![];
                    each(&path, |f| batch.push((String::from_utf16_lossy(f.name), f.attr, f.size, f.time)));
                    let mut subs = vec![];
                    {
                        let mut t = c.tree.write().unwrap_or_else(|e| e.into_inner());
                        for (name, attr, size, time) in batch {
                            let i = t.ents.len() as u32;
                            let off = t.names.len() as u32;
                            t.names.push_str(&name);
                            let hid = attr & HIDDEN != 0;
                            let flags = (attr & DIR != 0) as u8 * E_DIR | hid as u8 * E_HIDDEN | under as u8 * E_UNDER;
                            t.ents.push(Ent { off, len: name.len() as u16, flags, parent: id, size, time });
                            if attr & DIR != 0 && attr & REPARSE == 0 {
                                let d = t.dirs.len() as u32;
                                t.dirs.push(i);
                                subs.push((d, path.join(&name), under || hid));
                            }
                        }
                    }
                    let (m, cv) = &c.work;
                    let mut q = m.lock().unwrap_or_else(|e| e.into_inner());
                    q.0.extend(subs);
                    q.1 -= 1;
                    drop(q);
                    cv.notify_all();
                    let ms = c.started.elapsed().as_millis() as u64;
                    if ms.saturating_sub(last.load(Relaxed)) >= 150 { last.store(ms, Relaxed); notify(); }
                }
                c.work.1.notify_all();
                if alive.fetch_sub(1, Relaxed) == 1 {
                    c.done.store(true, Relaxed);
                    notify();
                }
            });
        }
        c
    }

    /// The next folder to list; None when the crawl is over (nothing waiting and nobody listing) or stopped.
    fn next_job(&self) -> Option<(u32, PathBuf, bool)> {
        let (m, cv) = &self.work;
        let mut q = m.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.stop.load(Relaxed) { return None; }
            if let Some(j) = q.0.pop_front() { q.1 += 1; return Some(j); }
            if q.1 == 0 { return None; }
            q = cv.wait(q).unwrap_or_else(|e| e.into_inner());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rx-fsx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(r"sub\deep")).unwrap();
        for f in ["Report.txt", r"sub\notes.md", r"sub\deep\REPORT-2.pdf"] { std::fs::write(d.join(f), "abc").unwrap(); }
        d
    }

    #[test]
    fn case_insensitive_contains() {
        assert!(contains_ci("Annual Report.PDF", "report"));
        assert!(contains_ci("x", ""));
        assert!(!contains_ci("rep", "report"));
        assert!(contains_ci("ŞEHİR planı.txt", "planı"));
    }

    #[test]
    fn lists_one_folder_with_sizes_and_kinds() {
        let d = temp("each");
        let mut seen = vec![];
        assert!(each(&d, |f| seen.push((String::from_utf16_lossy(f.name), f.attr & DIR != 0, f.size))));
        seen.sort();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(seen, [("Report.txt".to_string(), false, 3), ("sub".to_string(), true, 0)]);
        assert!(!each(Path::new(r"C:\no\such\folder\here"), |_| {}));
    }

    #[test]
    fn long_paths_get_the_unlimited_form() {
        let long = PathBuf::from(format!(r"C:\{}", "a".repeat(250)));
        assert!(String::from_utf16_lossy(&pattern(&long)).starts_with(r"\\?\C:\aaa"));
        let unc = PathBuf::from(format!(r"\\srv\share\{}", "b".repeat(250)));
        assert!(String::from_utf16_lossy(&pattern(&unc)).starts_with(r"\\?\UNC\srv\share\bbb"));
        assert_eq!(String::from_utf16_lossy(&pattern(Path::new(r"C:\"))), "C:\\*\0");
    }

    /// Timing on a real disk: `cargo test -p rust-explorer --release crawl_speed -- --ignored --nocapture` (RX_ROOT=folder).
    #[test]
    #[ignore]
    fn crawl_speed() {
        let root = PathBuf::from(std::env::var("RX_ROOT").unwrap_or_else(|_| "C:\\".into()));
        let c = Crawl::start(root.clone(), || {});
        while !c.is_done() { std::thread::sleep(std::time::Duration::from_millis(2)); }
        let crawl = c.started.elapsed();
        let t = c.tree.read().unwrap();
        let t0 = Instant::now();
        let (_, hits) = t.find(&["report".into()], false, 50_000);
        let q1 = t0.elapsed();
        let t0 = Instant::now();
        let (_, all) = t.find(&["e".into()], true, 50_000);
        let q2 = t0.elapsed();
        println!("{}: {} entries crawled in {crawl:?}; \"report\" {hits} hits in {q1:?}; \"e\" {all} hits in {q2:?}; names {} MB", root.display(), t.len(), t.names.len() >> 20);
    }

    #[test]
    fn crawl_finds_names_at_any_depth() {
        let d = temp("crawl");
        let c = Crawl::start(d.clone(), || {});
        let t0 = Instant::now();
        while !c.is_done() && t0.elapsed().as_secs() < 10 { std::thread::sleep(std::time::Duration::from_millis(5)); }
        assert!(c.is_done());
        let t = c.tree.read().unwrap();
        let (hits, total) = t.find(&["report".into()], false, 10);
        let mut paths: Vec<PathBuf> = hits.iter().map(|&i| t.path(&d, i)).collect();
        paths.sort();
        assert_eq!(total, 2);
        assert_eq!(paths, [d.join("Report.txt"), d.join(r"sub\deep\REPORT-2.pdf")]);
        assert_eq!(t.len(), 5); // Report.txt, sub, sub\notes.md, sub\deep, sub\deep\REPORT-2.pdf
        assert_eq!(t.find(&[], false, 2), (vec![0, 1], 5)); // capped list, full count
        drop(t);
        let _ = std::fs::remove_dir_all(&d);
    }
}
