//! What is inside each folder of the listing (bytes, subfolders and files, all the way down), counted in the background
//! by a few threads, like File Pilot's Folders / Files / Size columns. Totals show while they grow; a finished one is kept
//! for two minutes, so going back to a folder does not count it again. Folders on network drives are left alone.
use std::{collections::{HashMap, VecDeque}, path::{Path, PathBuf}, sync::{Condvar, Mutex, OnceLock, atomic::{AtomicU32, AtomicU64, Ordering::Relaxed}}, time::{Duration, Instant}};

pub const WM_SIZES: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 10; // some totals changed: repaint

/// Totals of one folder. `done` false = still counting (what is shown is a lower bound).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Sum { pub bytes: u64, pub dirs: u32, pub files: u32, pub done: bool }

const KEEP: Duration = Duration::from_secs(120);
const WORKERS: usize = 4;

struct Shared {
    cache: Mutex<HashMap<String, (Sum, Instant)>>,
    queue: (Mutex<VecDeque<(u32, PathBuf)>>, Condvar),
}
static SHARED: OnceLock<Shared> = OnceLock::new();
static ERA: AtomicU32 = AtomicU32::new(0); // a new listing: folders still queued or being counted for the old one are dropped
static MAIN: AtomicU64 = AtomicU64::new(0); // the window told when totals change
static LAST_NOTE: AtomicU64 = AtomicU64::new(0);

fn key(p: &Path) -> String { p.to_string_lossy().trim_end_matches('\\').to_lowercase() }

/// The totals of folder `p` so far (None = not counted, or not yet started).
pub fn get(p: &Path) -> Option<Sum> {
    SHARED.get()?.cache.lock().ok()?.get(&key(p)).map(|e| e.0)
}

/// Counts `dirs` (the folders of the listing just shown), dropping whatever was asked for before.
pub fn ask(dirs: Vec<PathBuf>) {
    let Some(sh) = SHARED.get() else { return };
    let era = ERA.fetch_add(1, Relaxed) + 1;
    let fresh = |p: &Path| sh.cache.lock().ok().and_then(|c| c.get(&key(p)).map(|(s, t)| s.done && t.elapsed() < KEEP)).unwrap_or(false);
    let todo: Vec<PathBuf> = dirs.into_iter().filter(|d| !remote(d) && !fresh(d)).collect();
    let (m, cv) = &sh.queue;
    let mut q = m.lock().unwrap_or_else(|e| e.into_inner());
    q.clear();
    q.extend(todo.into_iter().map(|d| (era, d)));
    cv.notify_all();
}

/// Counts `dirs` too, after what is already queued (folders opened in the tree), without dropping anything.
pub fn more(dirs: Vec<PathBuf>) {
    let Some(sh) = SHARED.get() else { return };
    let era = ERA.load(Relaxed);
    let fresh = |p: &Path| sh.cache.lock().ok().and_then(|c| c.get(&key(p)).map(|(s, t)| s.done && t.elapsed() < KEEP)).unwrap_or(false);
    let (m, cv) = &sh.queue;
    let mut q = m.lock().unwrap_or_else(|e| e.into_inner());
    q.extend(dirs.into_iter().filter(|d| !remote(d) && !fresh(d)).map(|d| (era, d)));
    cv.notify_all();
}

/// Starts the counting threads; `main` gets WM_SIZES (at most every 200 ms) while totals change.
pub fn start(main: isize) {
    MAIN.store(main as u64, Relaxed);
    if SHARED.set(Shared { cache: Mutex::new(HashMap::new()), queue: (Mutex::new(VecDeque::new()), Condvar::new()) }).is_err() { return; }
    for _ in 0..WORKERS {
        std::thread::spawn(|| loop {
            let Some(sh) = SHARED.get() else { return };
            let job = {
                let (m, cv) = &sh.queue;
                let mut q = m.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(j) = q.pop_front() { break j; }
                    q = cv.wait(q).unwrap_or_else(|e| e.into_inner());
                }
            };
            count(sh, job.0, &job.1);
        });
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
    let now = Instant::now().duration_since(*START.get_or_init(Instant::now)).as_millis() as u64;
    if now.saturating_sub(LAST_NOTE.load(Relaxed)) < 200 { return; }
    LAST_NOTE.store(now, Relaxed);
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(MAIN.load(Relaxed) as _, WM_SIZES, 0, 0) };
}
static START: OnceLock<Instant> = OnceLock::new();

/// Walks `root` (junctions and links are counted, not followed), publishing the totals as they grow.
fn count(sh: &Shared, era: u32, root: &Path) {
    let k = key(root);
    let mut sum = Sum::default();
    let mut stack = vec![root.to_path_buf()];
    let mut last = Instant::now();
    while let Some(d) = stack.pop() {
        if ERA.load(Relaxed) != era { return; } // a new listing: this one is not wanted any more
        crate::fsx::each(&d, |f| {
            if f.attr & crate::fsx::DIR != 0 {
                sum.dirs += 1;
                if f.attr & 0x400 == 0 { stack.push(d.join(String::from_utf16_lossy(f.name))); }
            } else {
                sum.files += 1;
                sum.bytes += f.size;
            }
        });
        if last.elapsed() > Duration::from_millis(150) {
            last = Instant::now();
            if let Ok(mut c) = sh.cache.lock() { c.insert(k.clone(), (sum, Instant::now())); }
            note();
        }
    }
    sum.done = true;
    if let Ok(mut c) = sh.cache.lock() { c.insert(k, (sum, Instant::now())); }
    note();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_everything_below_a_folder() {
        let d = std::env::temp_dir().join(format!("rx-sizes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(r"a\b")).unwrap();
        std::fs::write(d.join("x.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(d.join(r"a\b\y.bin"), vec![0u8; 24]).unwrap();
        let _ = SHARED.set(Shared { cache: Mutex::new(HashMap::new()), queue: (Mutex::new(VecDeque::new()), Condvar::new()) });
        let era = ERA.load(Relaxed);
        count(SHARED.get().unwrap(), era, &d);
        let s = get(&d).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(s, Sum { bytes: 1024, dirs: 2, files: 2, done: true });
    }
}
