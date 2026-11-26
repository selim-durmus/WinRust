// rust-explorer [path]  — file manager window (Explorer replacement).
// Virtual list (LVS_OWNERDATA, only visible rows are asked for text/icon), system icons, tabs,
// IFileOperation (recycle bin / undo / elevation prompts), real shell context menu with submenus, OLE drag and drop.
// Keys: Enter open, Backspace up, F2 rename, Del recycle, Ctrl+C/X/V copy/cut/paste, Ctrl+N new folder,
// Ctrl+T/W/Tab tabs, Ctrl+M thumbnails, Ctrl+L/F4 address bar, Ctrl+F or "? terms" in the bar = instant search through the rs service, right click / Menu key = shell context menu.
// Look: topbar's palette, painted tabs / toolbar / address pill and status line (src/ui.rs), dark title bar and list.
// Clipboard is the system one (Ctrl+C/X/V interoperate with Explorer). Also: Alt+Left/Right back/forward, Alt+Up up,
// Alt+Enter properties, F5 refresh, Ctrl+H hidden files, click a column header to sort, right click on empty space = folder menu.
// Speed: a folder is listed on a worker thread with FindFirstFileExW (fsx.rs) and shown at once when that takes under
// 60 ms (nearly always), else when it arrives; a watcher refreshes the folder when something in it changes (selection kept).
// Filter (floating over the list's bottom left corner): typing in the list or Ctrl+F filters the folder as you type; Ctrl+E ("Subfolders") makes it
// search the whole tree below, crawled once by several threads and then filtered in memory on every key press.
// Up/Down in the filter move through the list, Enter opens, Esc clears. Ctrl+Shift+F or "? terms" in the address bar =
// the whole PC through the rust-search service.
// Side pane: recents, bookmarks (Ctrl+D or the star), drives, places; the last tabs come back at start (prefs.rs).
// Settings: the gear (or Ctrl+,) opens a page with the options (opts.rs).
// shortcuts: icons are extension-based (exe/lnk/ico/url/scr per file) and synchronous (thumbnails: one background thread,
// 96px, no disk cache), silent rename errors, "? " search blocks the UI while it waits for the service, the service pipe
// is admin-only so run as admin, drops always land in the current folder (not on a folder row), tabs remember only the
// folder (no selection/scroll), the subfolder crawl does not follow changes on disk (it is redone after a minute).
// rust-explorer --desktop  - the desktop for rs-shell: borderless bottom-most color-keyed window with the Desktop folders
//   (user + public) as big icons over the wallpaper it paints itself (without explorer nothing else does): reads the BMP
//   that rs-shell converts into %LOCALAPPDATA%\rs-shell\wallpaper.bmp, "fill" style, scaled once at start.
//   shortcuts: wallpaper is loaded once (changing it needs a restart), icon labels are always white, no background menu /
//   drag-to-position / auto-arrange, items from the public desktop can't be renamed.
#![windows_subsystem = "windows"]
use std::{cell::{Cell, RefCell}, env, fs, os::windows::ffi::OsStrExt, path::{Path, PathBuf}, ptr, sync::{Arc, atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed}, mpsc}, time::Duration};
use windows_sys::Win32::{Foundation::*, Graphics::Gdi::{BLACK_BRUSH, GetStockObject, InvalidateRect}, System::LibraryLoader::GetModuleHandleW, UI::{Controls::*, Input::KeyboardAndMouse::{GetFocus, GetKeyState, SetFocus, VK_CONTROL, VK_SHIFT}, Shell::*, WindowsAndMessaging::*}};

mod fsx; // fast listing and the subfolder crawl
mod glist; // the GPU-drawn list (smooth scrolling), being built
mod menu; // our own right-click menu
mod opts; // the settings page
mod prefs; // settings, session, recents and bookmarks on disk
mod sizes; // what is inside each folder, counted in the background
mod thumbs; // pictures of the icon view, drawn at any zoom
mod ui; // the look: palette, painted tabs / toolbar / address pill / status line, dark theme

static DESKTOP: AtomicBool = AtomicBool::new(false);
fn desk() -> bool { DESKTOP.load(Relaxed) }
const CLR_NONE: isize = 0xFFFF_FFFF; // list background / text background: let the parent's wallpaper show through
thread_local! { static WALL: std::cell::Cell<(isize, i32, i32)> = const { std::cell::Cell::new((0, 0, 0)) }; } // (HBITMAP, w, h)
/// What the bar reserves at the top and bottom of the screen: the work area topbar sets when there is no explorer.
fn work_strips() -> (i32, i32) {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    if unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut _ as *mut _, 0) } == 0 { return (0, 0); }
    (r.top.max(0), (unsafe { GetSystemMetrics(SM_CYSCREEN) } - r.bottom).max(0))
}
/// Margins of the desktop's icon list, from the bar's real size: its reserved strip (any height, top or bottom).
/// The dock style reserves nothing but floats over the bottom edge, so it gets a fixed gap while topbar runs.
fn desk_top() -> i32 { work_strips().0 + ui::sc(8) }
fn desk_bottom() -> i32 {
    let reserved = work_strips().1;
    if reserved > 0 { return reserved + ui::sc(4); }
    let bar_up = unsafe { !FindWindowW(w("topbar").as_ptr(), ptr::null()).is_null() };
    if bar_up && winrust_common::style::style() == winrust_common::style::Style::Dock { ui::sc(52) } else { ui::sc(8) }
}
static THUMB_PX: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0); // picture size of the icon view (physical px), see ZOOM
const WM_GO: u32 = WM_APP + 1; // address bar: Enter (wp 13) / Esc (wp 27)
static EDIT_OLD: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

#[derive(Clone)]
struct Row { name: String, dir: bool, size: u64, time: u64, icon: i32, sub: (u32, u32), depth: u16, open: bool } // time: FILETIME ticks; icon: system image list index, -1 not asked yet; sub: (folders, files) below a folder, filled from sizes.rs before sorting; depth / open: the tree view

/// Back / forward trail of visited folders (one for the whole window, not per tab).
#[derive(Default)]
struct Hist { v: Vec<PathBuf>, pos: usize }
impl Hist {
    fn visit(&mut self, p: PathBuf) {
        if self.v.get(self.pos) == Some(&p) { return; }
        self.v.truncate(self.pos + usize::from(!self.v.is_empty())); // a new visit drops the forward trail
        self.v.push(p);
        self.pos = self.v.len() - 1;
    }
    fn back(&mut self) -> Option<PathBuf> { if self.pos > 0 { self.pos -= 1; Some(self.v[self.pos].clone()) } else { None } }
    fn forward(&mut self) -> Option<PathBuf> { if self.pos + 1 < self.v.len() { self.pos += 1; Some(self.v[self.pos].clone()) } else { None } }
}

// rows: what the list shows (the folder, filtered); all: the whole folder (or the "? " search hits) the filter works on
struct State { list: HWND, tab: HWND, edit: HWND, dir: PathBuf, rows: Vec<Row>, all: Vec<Row>, thumbs: bool, view: View, tabs: Vec<PathBuf>, active: usize, hist: Hist }
thread_local! {
    static ST: RefCell<State> = RefCell::new(State { list: 0 as _, tab: 0 as _, edit: 0 as _, dir: PathBuf::new(), rows: vec![], all: vec![], thumbs: false, view: View::Details, tabs: vec![], active: 0, hist: Hist { v: vec![], pos: 0 } });
    static PREFS: RefCell<prefs::Prefs> = RefCell::new(prefs::Prefs::default());
    static RECENT: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    static MARKS: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    static FILT: RefCell<Filt> = RefCell::new(Filt::default());
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
    static CRAWL: RefCell<Option<Arc<fsx::Crawl>>> = const { RefCell::new(None) };
    static SAVES_SESSION: Cell<bool> = const { Cell::new(false) }; // started without a folder: this window's tabs come back next time
}
static SORT_COL: AtomicUsize = AtomicUsize::new(0); // 0 name, 1 modified, 2 type, 3 size
static SORT_ASC: AtomicBool = AtomicBool::new(true);
static SHOW_HIDDEN: AtomicBool = AtomicBool::new(false); // Ctrl+H
static FOLDERS_FIRST: AtomicBool = AtomicBool::new(true);
static NAV: AtomicBool = AtomicBool::new(false); // loading from the back / forward trail: do not record it again
const WM_NAV: u32 = WM_APP + 3; // wp: 1 back, 2 forward, 3 properties, 4 up (sent from the message loop for Alt+key)
const WM_LISTED: u32 = WM_APP + 5; // a slow folder listing is ready: wp = its load number
const WM_CRAWL: u32 = WM_APP + 6; // the subfolder crawl found more (or finished)
const WM_CHANGED: u32 = WM_APP + 7; // something changed in the shown folder: wp = watcher number
const WM_FILTKEY: u32 = WM_APP + 8; // a key in the filter box the window acts on: wp = virtual key
static LOAD_GEN: AtomicU32 = AtomicU32::new(0);
static WATCH_GEN: AtomicU32 = AtomicU32::new(0);
static FILT_OLD: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
const MAX_HITS: usize = 50_000; // subfolder hits shown at most (all are counted)

/// The filter box and what it last found (for the status line).
#[derive(Default)]
struct Filt { edit: isize, text: String, sub: bool, total: usize, capped: bool, scanning: bool, scanned: usize, flipped: Option<String> }

/// A folder listing still running on its thread.
struct Pending { seq: u32, rx: mpsc::Receiver<Option<Vec<Row>>>, dir: PathBuf, record: bool }

fn row(name: String, m: Option<&fs::Metadata>) -> Row {
    use std::os::windows::fs::MetadataExt;
    Row { name, dir: m.is_some_and(|m| m.is_dir()), size: m.map_or(0, |m| m.len()), time: m.map_or(0, |m| m.last_write_time()), icon: -1, sub: (0, 0), depth: 0, open: false }
}

/// Folders first (unless turned off in the settings); inside each group by the column (0 name, 1 modified, 2 type,
/// 3 folders, 4 files, 5 size), ties by name. The lower-case names are made once per row, not twice per comparison.
fn sort_rows(rows: &mut Vec<Row>, col: usize, asc: bool) {
    let ff = FOLDERS_FIRST.load(Relaxed);
    let mut keys: Vec<(bool, String, String, usize)> = rows.iter().enumerate().map(|(i, r)| {
        let name = r.name.rsplit('\\').next().unwrap_or("").to_lowercase();
        (ff && !r.dir, if col == 2 { type_text(&r.name, r.dir) } else { String::new() }, name, i)
    }).collect();
    keys.sort_by(|a, b| {
        let (ra, rb) = (&rows[a.3], &rows[b.3]);
        let o = match col { 1 => ra.time.cmp(&rb.time), 2 => a.1.cmp(&b.1), 3 => ra.sub.0.cmp(&rb.sub.0), 4 => ra.sub.1.cmp(&rb.sub.1), 5 => ra.size.cmp(&rb.size), _ => a.2.cmp(&b.2) };
        a.0.cmp(&b.0).then(if asc { o } else { o.reverse() }).then_with(|| a.2.cmp(&b.2))
    });
    let mut old: Vec<Option<Row>> = std::mem::take(rows).into_iter().map(Some).collect();
    *rows = keys.iter().filter_map(|k| old[k.3].take()).collect();
}

/// Explorer's "Type" column in short form: "File folder", "TXT file", "File".
fn type_text(name: &str, dir: bool) -> String {
    if dir { return "File folder".into(); }
    let file = name.rsplit('\\').next().unwrap_or(name);
    match file.rsplit_once('.') { Some((stem, e)) if !stem.is_empty() && !e.is_empty() => format!("{} file", e.to_uppercase()), _ => "File".into() }
}

/// Explorer shows file sizes in KB; the bigger ones read better in MB / GB.
fn size_text(b: u64) -> String {
    const MB: u64 = 1 << 20;
    if b < MB { format!("{} KB", b.div_ceil(1024)) } else if b < 1 << 30 { format!("{:.1} MB", b as f64 / MB as f64) } else { format!("{:.2} GB", b as f64 / (1u64 << 30) as f64) }
}

fn date_text(ft: u64) -> String {
    use windows_sys::Win32::{Foundation::{FILETIME, SYSTEMTIME}, System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime}};
    if ft == 0 { return String::new(); }
    unsafe {
        let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
        let (mut u, mut l): (SYSTEMTIME, SYSTEMTIME) = (std::mem::zeroed(), std::mem::zeroed());
        if FileTimeToSystemTime(&f, &mut u) == 0 || SystemTimeToTzSpecificLocalTime(ptr::null(), &u, &mut l) == 0 { return String::new(); }
        format!("{:04}-{:02}-{:02} {:02}:{:02}", l.wYear, l.wMonth, l.wDay, l.wHour, l.wMinute)
    }
}

/// Our own tools installed by the launcher (photoview, wiztree): the exe in Program Files, None when not installed.
fn tool(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env::var_os("ProgramFiles")?).join(name).join(format!("{name}.exe"));
    p.is_file().then_some(p)
}

/// Same list as photoview's EXTS.
fn is_pic(p: &std::path::Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| ["jpg", "jpeg", "jfif", "png", "bmp", "gif", "tif", "tiff", "webp", "heic", "avif"].contains(&e.to_ascii_lowercase().as_str()))
}

fn properties(path: &PathBuf) {
    let (f, v) = (w(&path.to_string_lossy()), w("properties"));
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = 0x0C; // SEE_MASK_INVOKEIDLIST
    sei.lpVerb = v.as_ptr(); sei.lpFile = f.as_ptr(); sei.nShow = SW_SHOW;
    unsafe { ShellExecuteExW(&mut sei) };
}

/// New folder / new empty file with a free name ("New folder", "New folder (2)", ...). Once the folder is listed again
/// it is selected and its name is open for typing (RENAME_NEW).
fn new_entry(dir: &PathBuf, base: &str, folder: bool) {
    let (stem, ext) = match base.rsplit_once('.') { Some((s, e)) if !folder => (s.to_string(), format!(".{e}")), _ => (base.to_string(), String::new()) };
    let path = (1..).map(|n| dir.join(if n == 1 { base.to_string() } else { format!("{stem} ({n}){ext}") })).find(|p| !p.exists());
    if let Some(p) = path {
        if if folder { fs::create_dir(&p) } else { fs::File::create(&p).map(drop) }.is_ok() {
            sh::push_undo(sh::Undo::Created(p.clone()));
            if !desk() { RENAME_NEW.set(p.file_name().map(|n| n.to_string_lossy().into_owned())); }
        }
    }
}

thread_local! {
    /// The name of a folder or file just made here: the next listing of this folder selects it and starts renaming it.
    static RENAME_NEW: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Pastes what is on the system clipboard (files copied or cut here or in any other file manager).
fn paste(h: HWND, dir: &PathBuf) {
    let Some((paths, cut)) = sh::clip_get() else { return };
    let _ = sh::fileop(h, if cut { sh::Op::Move } else { sh::Op::Copy }, &paths, dir);
    if cut { sh::clip_clear(); }
}

/// Right click on empty space: the folder's own menu.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn bg_menu(h: HWND) {
    use menu::Item;
    let disk = tool("wiztree");
    let mut items = vec![];
    // what Ctrl+Z would take back, on top while there is something
    if let Some(u) = sh::undo_label() { items.extend([Item::new(7, "\u{E7A7}", u, "Ctrl+Z"), Item::sep()]); }
    items.extend([
        Item::new(1, "\u{E8F4}", "New folder", "Ctrl+N"), Item::new(2, "\u{E8A5}", "New text file", ""), Item::sep(),
        Item::new(3, "\u{E77F}", "Paste", "Ctrl+V"), Item::new(4, "\u{E72C}", "Refresh", "F5"),
    ]);
    if disk.is_some() { items.push(Item::new(6, "\u{EDA2}", "Disk usage", "")); }
    items.extend([Item::sep(), Item::new(5, "\u{E946}", "Properties", "Alt+Enter")]);
    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    let id = menu::track(h, items, (pt.x, pt.y)) as usize;
    let dir = cur_dir();
    match id {
        6 => { // wiztree takes the drive letter and asks for admin rights itself
            let drive: String = dir.to_string_lossy().chars().take(2).collect();
            if let Some(exe) = disk { let _ = std::process::Command::new(exe).arg(drive).spawn(); }
        }
        1 => new_entry(&dir, "New folder", true),
        2 => new_entry(&dir, "New text document.txt", false),
        3 => paste(h, &dir),
        5 => properties(&dir),
        7 => { sh::undo(h as _); }
        _ => {}
    }
    if id != 0 { reload(h); }
}

/// Back / forward along the visited-folder trail.
fn nav(h: HWND, back: bool) {
    let to = ST.with_borrow_mut(|st| if back { st.hist.back() } else { st.hist.forward() });
    if let Some(p) = to {
        NAV.store(true, Relaxed);
        load(h, p);
        NAV.store(false, Relaxed);
    }
}

fn w(s: &str) -> Vec<u16> { s.encode_utf16().chain([0]).collect() }

fn put(h: HWND, msg: u32, wp: usize, lp: isize) -> isize { unsafe { SendMessageW(h, msg, wp, lp) } }

fn wstr(p: *const u16) -> String {
    let mut n = 0;
    while unsafe { *p.add(n) } != 0 { n += 1; }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

pub fn cur_dir() -> PathBuf { ST.with_borrow(|st| st.dir.clone()) }

/// System image-list index. Extension-based (no disk access) except for files that carry their own icon.
fn icon_of(p: &PathBuf, dir: bool) -> i32 {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let own = !dir && matches!(ext.as_str(), "exe" | "lnk" | "ico" | "url" | "scr");
    let mut fi: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let attr = if dir { 0x10 } else { 0x80 }; // DIRECTORY / NORMAL
    let flags = SHGFI_SYSICONINDEX | SHGFI_SMALLICON | if own { 0 } else { SHGFI_USEFILEATTRIBUTES };
    unsafe { SHGetFileInfoW(w(&p.to_string_lossy()).as_ptr(), attr, &mut fi, size_of::<SHFILEINFOW>() as u32, flags) };
    fi.iIcon
}

/// The tabs are painted by the chrome window from State.tabs: "setting a tab's text" is just a repaint.
fn tab_text(tab: HWND, _i: usize, _text: &str, _insert: bool) {
    if !tab.is_null() { unsafe { InvalidateRect(tab, ptr::null(), 0) }; }
}

/// The text of a desktop row. Rows from the public desktop carry absolute paths: only the file name is shown, and shortcuts
/// lose their ".lnk" / ".url" like in Explorer. Other windows show the row as it is.
fn shown_name(name: &str, desktop: bool) -> String {
    if !desktop { return name.to_string(); }
    let file = std::path::Path::new(name).file_name().map_or_else(|| name.to_string(), |n| n.to_string_lossy().into_owned());
    let lower = file.to_lowercase();
    if lower.ends_with(".lnk") || lower.ends_with(".url") { file[..file.len() - 4].to_string() } else { file }
}

fn label(dir: &PathBuf) -> String {
    dir.file_name().map_or_else(|| dir.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned())
}

/// Rows of `dir`, sorted. Desktop mode merges the public desktop (absolute names) and hides hidden files.
/// None = `dir` itself is unreadable. Runs on worker threads too (only atomics are read).
fn rows_of(dir: &PathBuf) -> Option<Vec<Row>> {
    let mut dirs = vec![dir.clone()];
    if desk() { dirs.extend(sh::desktop_dirs().into_iter().skip(1)); }
    let hidden = !desk() && SHOW_HIDDEN.load(Relaxed);
    let mut rows = vec![];
    for (n, d) in dirs.iter().enumerate() {
        let ok = fsx::each(d, |f| {
            if !hidden && f.attr & fsx::HIDDEN != 0 { return; }
            let name = String::from_utf16_lossy(f.name);
            let name = if n == 0 { name } else { d.join(name).to_string_lossy().into_owned() };
            rows.push(Row { name, dir: f.attr & fsx::DIR != 0, size: f.size, time: f.time, icon: -1, sub: (0, 0), depth: 0, open: false });
        });
        if !ok && n == 0 { return None; }
    }
    sort_rows(&mut rows, SORT_COL.load(Relaxed), SORT_ASC.load(Relaxed));
    Some(rows)
}

/// Shows `dir`. It is listed on a worker thread: a quick folder (nearly all of them) is on screen before this returns,
/// so there is no blank frame; a slow one (network share, sleeping disk) leaves the old listing up and comes through
/// WM_LISTED, unless another folder was asked for meanwhile. An unreadable dir (access denied, gone) changes nothing.
/// Never call with ST borrowed: the list sends GETDISPINFO back into the window proc.
fn load(h: HWND, dir: PathBuf) {
    let record = !NAV.load(Relaxed);
    if desk() {
        if let Some(rows) = rows_of(&dir) { commit(h, dir, rows, record); }
        return;
    }
    PENDING.set(None);
    let seq = LOAD_GEN.fetch_add(1, Relaxed) + 1;
    let (tx, rx) = mpsc::channel();
    let (d, hw) = (dir.clone(), h as isize);
    std::thread::spawn(move || {
        let _ = tx.send(rows_of(&d));
        unsafe { PostMessageW(hw as HWND, WM_LISTED, seq as usize, 0) };
    });
    match rx.recv_timeout(Duration::from_millis(60)) {
        Ok(Some(rows)) => commit(h, dir, rows, record),
        Ok(None) => {}
        Err(_) => {
            PENDING.set(Some(Pending { seq, rx, dir, record }));
            unsafe { ui::status_dirty(h) }; // "Loading..."
        }
    }
}

/// Puts a finished listing on screen. The same folder again (F5, a change on disk, after a file operation) keeps the
/// selection and the scroll position; another folder clears the filter and becomes the newest recent place.
fn commit(h: HWND, dir: PathBuf, rows: Vec<Row>, record: bool) {
    let same = ST.with_borrow(|st| st.dir == dir) && !ui::searching();
    let (mut keep, mut top) = if same && !desk() { kept() } else { (vec![], 0) };
    let fresh = if same { RENAME_NEW.take() } else { RENAME_NEW.set(None); None };
    if let Some(n) = &fresh { (keep, top) = (vec![n.clone()], 0); } // the new item alone is selected, and in view
    // up (or back) out of a folder: the one you were in is selected and in view, so you see where you came from
    let prev = ST.with_borrow(|st| st.dir.clone());
    if !same && !desk() {
        if let Some(first) = prev.strip_prefix(&dir).ok().and_then(|rel| rel.components().next()) {
            keep = vec![first.as_os_str().to_string_lossy().into_owned()];
        }
    }
    if !desk() { unsafe { SetWindowTextW(h, w(&dir.to_string_lossy()).as_ptr()) }; }
    let (tab, edit, active) = ST.with_borrow_mut(|st| {
        if let Some(t) = st.tabs.get_mut(st.active) { *t = dir.clone(); }
        st.dir = dir.clone();
        st.all = rows;
        if record { st.hist.visit(dir.clone()); }
        (st.tab, st.edit, st.active)
    });
    unsafe { SetWindowTextW(edit, w(&dir.to_string_lossy()).as_ptr()) };
    ui::set_search(None); // a folder again: the pill shows its path
    tab_text(tab, active, &label(&dir), false);
    ui::nav_dirty(); // the pane highlights where we are
    if desk() {
        let (list, n) = ST.with_borrow_mut(|st| { st.rows = st.all.clone(); (st.list, st.rows.len()) });
        put(list, LVM_DELETEALLITEMS, 0, 0);
        desk::fill(list, n);
        return;
    }
    if same { tree_refresh(); } else { tree_forget(); }
    if !same {
        // what is inside each folder: counted in the background (a recent count is reused). Not on a refresh of the
        // same folder: that would restart counts still running, and a busy folder would never finish
        if PREFS.with_borrow(|p| p.folder_sizes) { sizes::ask(ST.with_borrow(|st| st.all.iter().filter(|r| r.dir).map(|r| dir.join(&r.name)).collect())); }
        thumbs::clear(); // the pictures of the folder left behind
        filter_clear();
        watch(h, &dir);
        remember(dir.clone());
        save_session();
    }
    show(h, &keep, top);
    if let Some(n) = fresh {
        let (list, i) = ST.with_borrow(|st| (st.list, st.rows.iter().position(|r| r.name == n)));
        if let Some(i) = i { unsafe { SetFocus(list) }; edit_label(list, i); }
    }
}

/// Names of the selected rows and the first row in view, to put them back after the rows are replaced.
fn kept() -> (Vec<String>, isize) {
    ST.with_borrow(|st| {
        let (mut sel, mut i) = (vec![], -1isize);
        loop {
            i = put(st.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
            let Some(r) = usize::try_from(i).ok().and_then(|i| st.rows.get(i)) else { break };
            sel.push(r.name.clone());
        }
        (sel, put(st.list, LVM_GETTOPINDEX, 0, 0))
    })
}

/// Fills the list from `all` through the filter: the folder itself, the rows whose name has every word of the filter,
/// or with "Subfolders" on the hits from the whole tree below. `keep` names are selected again, `top` scrolled back to.
/// With a filter the first row is selected, so Enter in the filter box opens it.
fn show(h: HWND, keep: &[String], top: isize) {
    let (text, sub) = FILT.with_borrow(|f| (f.text.clone(), f.sub));
    let terms: Vec<String> = text.to_lowercase().split_whitespace().map(String::from).collect();
    let (mut total, mut capped, mut scanning, mut scanned, mut flipped) = (0, false, false, 0, None);
    let rows: Vec<Row> = if terms.is_empty() {
        ST.with_borrow(|st| if st.view == View::Tree && !ui::searching() { tree_rows(&st.dir, &st.all) } else { st.all.clone() })
    } else if !sub || ui::searching() {
        let pick = |terms: &[String]| ST.with_borrow(|st| st.all.iter().filter(|r| { let n = r.name.rsplit('\\').next().unwrap_or(""); terms.iter().all(|t| fsx::contains_ci(n, t)) }).cloned().collect::<Vec<_>>());
        let mut v = pick(&terms);
        if v.is_empty() {
            // typed in the other keyboard layout: show what was meant
            let alt: Vec<String> = terms.iter().map(|t| flip_layout(t)).collect();
            if alt != terms { v = pick(&alt); if !v.is_empty() { flipped = Some(alt.join(" ")); } }
        }
        total = v.len();
        v
    } else {
        let c = crawl_for(h, &cur_dir());
        let t = c.tree.read().unwrap_or_else(|e| e.into_inner());
        let hidden = SHOW_HIDDEN.load(Relaxed);
        let (mut hits, mut n) = t.find(&terms, hidden, MAX_HITS);
        if n == 0 {
            let alt: Vec<String> = terms.iter().map(|t| flip_layout(t)).collect();
            if alt != terms { let (a, k) = t.find(&alt, hidden, MAX_HITS); if k > 0 { (hits, n) = (a, k); flipped = Some(alt.join(" ")); } }
        }
        (total, capped, scanning, scanned) = (n, n > hits.len(), !c.is_done(), t.len());
        let mut v: Vec<Row> = hits.iter().map(|&i| {
            let (dir, size, time) = t.meta(i);
            Row { name: t.path(&c.root, i).to_string_lossy().into_owned(), dir, size, time, icon: -1, sub: (0, 0), depth: 0, open: false }
        }).collect();
        drop(t);
        sort_rows(&mut v, SORT_COL.load(Relaxed), SORT_ASC.load(Relaxed));
        v
    };
    FILT.with_borrow_mut(|f| { f.total = total; f.capped = capped; f.scanning = scanning; f.scanned = scanned; f.flipped = flipped; });
    let n = rows.len();
    let list = ST.with_borrow_mut(|st| { st.rows = rows; st.list });
    // the old rows go and the new ones come in one frame: drawing is off in between (a frame with no rows showed
    // "This folder is empty" for an instant). A couple of empty rows at the end leave room to right-click the folder.
    put(list, WM_SETREDRAW, 0, 0);
    put(list, LVM_DELETEALLITEMS, 0, 0);
    put(list, LVM_SETITEMCOUNT, if n > 0 && !desk() { n + PAD_ROWS } else { n }, LVSICF_NOSCROLL as isize);
    let set = |i: usize, state: u32| {
        let mut it: LVITEMW = unsafe { std::mem::zeroed() };
        it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
        it.state = state;
        put(list, LVM_SETITEMSTATE, i, &it as *const _ as isize);
    };
    if !keep.is_empty() {
        let idx: Vec<usize> = ST.with_borrow(|st| st.rows.iter().enumerate().filter(|(_, r)| keep.contains(&r.name)).map(|(i, _)| i).collect());
        for (k, &i) in idx.iter().enumerate() { set(i, LVIS_SELECTED | if k == 0 { LVIS_FOCUSED } else { 0 }); }
    } else if !terms.is_empty() && n > 0 {
        set(0, LVIS_SELECTED | LVIS_FOCUSED);
    }
    if top > 0 && (top as usize) < n {
        // back to where the view was: scroll by whole rows
        let mut r = RECT { left: LVIR_BOUNDS as i32, ..unsafe { std::mem::zeroed() } };
        put(list, LVM_GETITEMRECT, 0, &mut r as *mut _ as isize);
        put(list, LVM_SCROLL, 0, top * (r.bottom - r.top) as isize);
    } else if !keep.is_empty() {
        let f = put(list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize);
        if f >= 0 { put(list, LVM_ENSUREVISIBLE, f as usize, 0); }
    } else if !terms.is_empty() && n > 0 {
        // (not on a plain listing: the list is still 0 px tall at start, and that scrolled the first row out of view)
        put(list, LVM_ENSUREVISIBLE, 0, 0);
    }
    put(list, WM_SETREDRAW, 1, 0);
    unsafe { InvalidateRect(list, ptr::null(), 0) };
    unsafe { ui::status_dirty(h) };
    glist::reset();
    ui::sync_lists();
}

/// Empty rows after the last one: room to right-click the folder itself (new folder, paste) in a full list.
pub const PAD_ROWS: usize = 2;

/// The crawl of `dir` for the subfolder filter: the running or recent one (under a minute old), else a new one.
fn crawl_for(h: HWND, dir: &Path) -> Arc<fsx::Crawl> {
    CRAWL.with_borrow_mut(|c| {
        if let Some(x) = c.as_ref().filter(|x| prefs::same(&x.root, dir) && (!x.is_done() || x.started.elapsed() < Duration::from_secs(60))) { return x.clone(); }
        if let Some(old) = c.take() { old.stop(); }
        let hw = h as isize;
        let n = fsx::Crawl::start(dir.to_path_buf(), move || unsafe { PostMessageW(hw as HWND, WM_CRAWL, 0, 0); });
        *c = Some(n.clone());
        n
    })
}

/// Refreshes the shown folder when something in it changes (a download finishing, a file saved by another program).
/// A burst of changes (a copy, an archive being unpacked) is one refresh. Each new folder starts a new watcher; the old
/// one notices it is no longer wanted within half a second and ends.
fn watch(h: HWND, dir: &Path) {
    use windows_sys::Win32::{Storage::FileSystem::*, System::Threading::WaitForSingleObject};
    let g = WATCH_GEN.fetch_add(1, Relaxed) + 1;
    let (hw, p): (isize, Vec<u16>) = (h as isize, dir.as_os_str().encode_wide().chain([0]).collect());
    std::thread::spawn(move || unsafe {
        let flags = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE;
        let n = FindFirstChangeNotificationW(p.as_ptr(), 0, flags);
        if n == INVALID_HANDLE_VALUE { return; }
        while WATCH_GEN.load(Relaxed) == g {
            if WaitForSingleObject(n, 500) != 0 { continue; } // WAIT_OBJECT_0 = something changed
            for _ in 0..8 {
                // wait for the burst to calm down (at most 2 s: a file being written keeps changing)
                FindNextChangeNotification(n);
                std::thread::sleep(Duration::from_millis(250));
                if WaitForSingleObject(n, 0) != 0 { break; }
            }
            if WATCH_GEN.load(Relaxed) == g { PostMessageW(hw as HWND, WM_CHANGED, g as usize, 0); }
        }
        FindCloseChangeNotification(n);
    });
}

/// Asks the rust-search service of drive `d` (see plugins/rust-search): terms in, one path per line out.
/// None = no service for that drive or no access (its pipe is admin-only unless started with --allow).
fn ask(d: char, q: &str) -> Option<String> {
    use std::{fs::OpenOptions, io::{Read, Write}};
    let mut p = OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\rs-search-{d}")).ok()?;
    p.write_all(q.as_bytes()).ok()?;
    let (mut out, mut chunk) = (Vec::new(), [0u8; 1 << 16]);
    // the server ends its reply by hanging up, which surfaces as a read error: treat any error as EOF
    while let Ok(n @ 1..) = p.read(&mut chunk) { out.extend_from_slice(&chunk[..n]); }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Fills the list with search hits from every drive's service. Rows hold absolute paths, so joining them
/// onto the current dir still yields the right file. Blocks the UI for the duration of the query (fast).
/// Never call with ST borrowed.
fn search(h: HWND, q: &str) {
    let mut rows = vec![];
    let mut more = String::new();
    let mut indexed = false; // some drive had a rust-search service
    for d in 'A'..='Z' {
        let Some(reply) = ask(d, q) else { continue };
        indexed = true;
        for line in reply.lines() {
            if line.starts_with("... ") { more = format!(", {}", line.trim_start_matches("... ")); continue; }
            let m = fs::metadata(line).ok();
            rows.push(row(line.to_string(), m.as_ref()));
        }
    }
    if !indexed {
        // no rust-search installed: look through the current folder and below instead
        let (local, cut, flipped) = local_search(&cur_dir(), q);
        if flipped { more = format!(" for \u{201C}{}\u{201D}", flip_layout(q)); } // typed in the wrong layout: shown what was meant
        rows = local;
        if cut { more += ", stopped early: narrow the search"; }
    }
    let n = rows.len();
    let where_ = if indexed { "" } else { " in this folder" };
    let title = if n == 0 { format!("search: {q} - nothing{where_}") } else { format!("search: {q} - {n} hits{where_}{more}") };
    ui::set_search(Some(q.to_string()));
    unsafe { SetWindowTextW(h, w(&title).as_ptr()) };
    ST.with_borrow_mut(|st| st.all = rows);
    filter_clear();
    show(h, &[], 0);
}

// ---------- the filter box ----------

fn filter_edit() -> HWND { FILT.with_borrow(|f| f.edit) as HWND }
pub fn filtering() -> bool { FILT.with_borrow(|f| !f.text.trim().is_empty()) }

/// Empties the filter box without filtering again (a new folder is coming anyway).
fn filter_clear() {
    let e = filter_edit();
    FILT.with_borrow_mut(|f| f.text.clear());
    if !e.is_null() { unsafe { SetWindowTextW(e, w("").as_ptr()) }; }
}

/// The filter box got new text (EN_CHANGE).
fn filter_changed(h: HWND) {
    let e = filter_edit();
    let mut buf = vec![0u16; unsafe { GetWindowTextLengthW(e) } as usize + 1];
    let n = unsafe { GetWindowTextW(e, buf.as_mut_ptr(), buf.len() as i32) } as usize;
    let text = String::from_utf16_lossy(&buf[..n]);
    if FILT.with_borrow(|f| f.text == text) { return; }
    FILT.with_borrow_mut(|f| f.text = text);
    show(h, &[], 0);
}

/// Into the filter box (Ctrl+F, the search button, or a typed character `ch` that then goes in at the end).
fn filter_focus(ch: Option<u16>) {
    let e = filter_edit();
    if e.is_null() { return; }
    unsafe {
        SetFocus(e);
        let len = GetWindowTextLengthW(e) as usize;
        if let Some(c) = ch { put(e, EM_SETSEL, len, len as isize); put(e, WM_CHAR, c as usize, 0); } else { put(e, EM_SETSEL, 0, -1); }
    }
}

/// "Subfolders" on / off (Ctrl+E or the button). On starts crawling right away, so the first word typed already finds.
fn toggle_sub(h: HWND) {
    let on = FILT.with_borrow_mut(|f| { f.sub = !f.sub; f.sub });
    set_prefs(h, |p| p.subfolders = on, false);
    filter_cue();
    if on { crawl_for(h, &cur_dir()); }
    if filtering() { show(h, &[], 0); } else { unsafe { ui::status_dirty(h) }; }
}

fn filter_cue() {
    let (e, sub) = FILT.with_borrow(|f| (f.edit as HWND, f.sub));
    let cue = if sub { "Filter all below\u{2026}" } else { "Filter\u{2026}" }; // the glyph next to it says the rest
    if !e.is_null() { put(e, 0x1501, 1, w(cue).as_ptr() as isize); } // EM_SETCUEBANNER, shown while focused too
}

/// Filter box subclass: the keys that act on the list go to the window; Up / Down / Page keys move the list's selection
/// while typing goes on.
unsafe extern "system" fn filter_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
        if m == WM_KEYDOWN {
            match wp {
                0x26 | 0x28 | 0x21 | 0x22 => { SendMessageW(ST.with_borrow(|st| st.list), WM_KEYDOWN, wp, lp); return 0; }
                0x0D | 0x1B => { PostMessageW(GetAncestor(h, GA_ROOT), WM_FILTKEY, wp, 0); return 0; }
                0x45 | 0x46 | 0x44 if ctrl => { PostMessageW(GetAncestor(h, GA_ROOT), WM_FILTKEY, wp, 0); return 0; } // Ctrl+E / F / D
                _ => {}
            }
        }
        // Enter / Esc / Ctrl+letter would beep or insert control characters
        if m == WM_CHAR && (matches!(wp, 0x0D | 0x1B | 0x7F) || (wp < 0x20 && ctrl && !matches!(wp, 0x01 | 0x03 | 0x16 | 0x18 | 0x1A | 0x08))) { return 0; }
        CallWindowProcW(std::mem::transmute::<isize, WNDPROC>(FILT_OLD.load(Relaxed)), h, m, wp, lp)
    }
}

/// The status line: how many rows, what is selected, what the filter found, and whether something is still coming.
pub fn status_text() -> String {
    if PENDING.with_borrow(|p| p.is_some()) { return "Loading\u{2026}".into(); }
    let (n, all, sel, bytes) = ST.with_borrow(|st| {
        let (mut k, mut b, mut i) = (0usize, 0u64, -1isize);
        loop {
            i = put(st.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
            let Some(r) = usize::try_from(i).ok().and_then(|i| st.rows.get(i)) else { break };
            k += 1;
            if !r.dir { b += r.size; }
        }
        (st.rows.len(), st.all.len(), k, b)
    });
    let items = |k: usize| format!("{} item{}", thousands(k), if k == 1 { "" } else { "s" });
    let f = FILT.with_borrow(|f| (f.sub, f.total, f.capped, f.scanning, f.scanned, f.flipped.clone()));
    let (sub, total, capped, scanning, scanned, flipped) = f;
    let mut s = if !filtering() {
        items(n)
    } else if sub && !ui::searching() {
        let mut s = format!("{} found below", thousands(total));
        if capped { s += &format!(", first {} shown", thousands(n)); }
        if scanning { s += &format!("  \u{00B7}  searching\u{2026} {} checked", thousands(scanned)); }
        s
    } else {
        format!("{} of {}", thousands(n), items(all))
    };
    if let Some(q) = flipped { s += &format!(" for \u{201C}{q}\u{201D}"); }
    if sel > 0 { s += &format!("  \u{00B7}  {sel} selected"); }
    if bytes > 0 { s += &format!("  \u{00B7}  {}", size_text(bytes)); }
    s
}

fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 { out.push(','); }
        out.push(c);
    }
    out
}

// ---------- remembered things: settings, tabs, recent places, bookmarks ----------

/// Changes the settings, saves them and (with `reload`) shows the folder again under them.
fn set_prefs(h: HWND, f: impl FnOnce(&mut prefs::Prefs), reload_now: bool) {
    let p = PREFS.with_borrow_mut(|p| { f(p); p.clone() });
    prefs::save(&p);
    SHOW_HIDDEN.store(p.hidden, Relaxed);
    FOLDERS_FIRST.store(p.folders_first, Relaxed);
    sizes::HDD_MODE.store(p.hdd_mode, Relaxed);
    FILT.with_borrow_mut(|f| f.sub = p.subfolders);
    filter_cue();
    ui::nav_rebuild();
    ui::sync_lists();
    if reload_now { reload(h); }
    // folder totals just turned on: counted for the folder shown (ones already known are not counted again)
    if p.folder_sizes && !desk() { let d = cur_dir(); sizes::ask(ST.with_borrow(|st| st.all.iter().filter(|r| r.dir).map(|r| d.join(&r.name)).collect())); }
}

fn prefs_now() -> prefs::Prefs { PREFS.with_borrow(|p| p.clone()) }

fn save_session() {
    if desk() || !SAVES_SESSION.get() { return; }
    let (tabs, active) = ST.with_borrow(|st| (st.tabs.clone(), st.active));
    prefs::write("session.txt", &prefs::render_session(&tabs, active));
}

/// `p` (a folder shown or a file opened) becomes the newest recent place. The file is read again first, so several
/// windows add to one list.
fn remember(p: PathBuf) {
    if desk() || p.as_os_str().is_empty() { return; }
    let mut r = prefs::read_paths("recent.txt");
    prefs::push_front(&mut r, p, 20);
    prefs::write_paths("recent.txt", &r);
    RECENT.set(r);
    ui::nav_rebuild();
}

fn forget_recent(p: Option<&Path>) {
    let mut r = prefs::read_paths("recent.txt");
    match p { Some(p) => r.retain(|x| !prefs::same(x, p)), None => r.clear() }
    prefs::write_paths("recent.txt", &r);
    RECENT.set(r);
    ui::nav_rebuild();
}

pub fn marked(p: &Path) -> bool { MARKS.with_borrow(|m| m.iter().any(|x| prefs::same(x, p))) }

/// Bookmarks `p`, or removes it when it already is one (Ctrl+D, the star).
fn toggle_mark(h: HWND, p: PathBuf) {
    let mut m = prefs::read_paths("bookmarks.txt");
    match m.iter().position(|x| prefs::same(x, &p)) { Some(i) => { m.remove(i); } None => m.push(p) }
    prefs::write_paths("bookmarks.txt", &m);
    MARKS.set(m);
    ui::nav_rebuild();
    unsafe { InvalidateRect(ST.with_borrow(|st| st.tab), ptr::null(), 0) }; // the star
    let _ = h;
}

/// A click on a column title: sorts by it (again: the other way round). Both lists.
fn sort_click(h: HWND, c: usize) {
    let asc = if SORT_COL.load(Relaxed) == c { !SORT_ASC.load(Relaxed) } else { true };
    let list = ST.with_borrow(|st| st.list);
    SORT_COL.store(c, Relaxed);
    SORT_ASC.store(asc, Relaxed);
    // the whole header again: the column sorted before must lose its mark (only the clicked one was redrawn)
    unsafe { InvalidateRect(put(ST.with_borrow(|st| st.list), LVM_GETHEADER, 0, 0) as HWND, ptr::null(), 0) };
    if !desk() { set_prefs(h, |p| { p.sort_col = c; p.sort_asc = asc; }, false); }
    let n = ST.with_borrow_mut(|st| {
        // folder totals counted so far take part in the sort
        let d = st.dir.clone();
        for r in st.rows.iter_mut().chain(st.all.iter_mut()).filter(|r| r.dir) {
            if let Some(s) = sizes::get(&d.join(&r.name)) { r.size = s.bytes; r.sub = (s.dirs, s.files); }
        }
        sort_rows(&mut st.rows, c, asc);
        sort_rows(&mut st.all, c, asc);
        if st.view == View::Tree { tree_refresh(); } // each open folder sorted on its own (rows_of sorts)
        st.rows.len()
    });
    put(list, LVM_REDRAWITEMS, 0, n as isize - 1);
    glist::reset();
}

/// Opens row `i`: a folder here, a picture in photoview, anything else with its program.
fn open_row(h: HWND, i: usize) {
    let hit = ST.with_borrow(|st| st.rows.get(i).map(|r| (st.dir.join(&r.name), r.dir)));
    let Some((p, is_dir)) = hit else { return };
    if is_dir && desk() { let _ = std::process::Command::new(env::current_exe().unwrap_or_default()).arg(p).spawn(); }
    else if is_dir { load(h, p) }
    else { open_file(&p) }
}

fn open_file(p: &Path) {
    remember(p.to_path_buf());
    if let Some(exe) = tool("photoview").filter(|_| is_pic(p)) { let _ = std::process::Command::new(exe).arg(p).spawn(); }
    else { unsafe { ShellExecuteW(0 as _, ptr::null(), w(&p.to_string_lossy()).as_ptr(), ptr::null(), ptr::null(), SW_SHOWNORMAL) }; }
}

/// A shell place that is not a folder on disk (the Recycle Bin): the shell opens it, and it is no recent place.
fn open_shell_place(p: &Path) {
    unsafe { ShellExecuteW(0 as _, ptr::null(), w(&p.to_string_lossy()).as_ptr(), ptr::null(), ptr::null(), SW_SHOWNORMAL) };
}

/// Right click on a place in the side pane.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn nav_menu(h: HWND, i: usize) {
    use menu::Item;
    // on a place: its own items first; anywhere in the pane: fold or unfold the whole pane
    let place = ui::nav_item(i);
    let mut items = vec![];
    if let Some((p, sec)) = &place {
        items.extend([Item::new(1, "\u{E8A7}", "Open in new tab", ""), Item::new(2, if marked(p) { "\u{E735}" } else { "\u{E734}" }, if marked(p) { "Remove bookmark" } else { "Bookmark" }, "")]);
        if *sec == ui::SEC_RECENT { items.extend([Item::new(3, "\u{E711}", "Remove from recents", ""), Item::new(4, "\u{E74D}", "Clear recents", "")]); }
        items.extend([Item::new(5, "\u{E946}", "Properties", ""), Item::sep()]);
    }
    items.extend([Item::new(6, "\u{E70D}", "Expand all", ""), Item::new(7, "\u{E70E}", "Collapse all", "")]);
    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    let id = menu::track(h, items, (pt.x, pt.y));
    match (id, place) {
        (6, _) => set_prefs(h, |p| p.collapsed.clear(), false), // every section open
        (7, _) => { ui::nav_close_all(); set_prefs(h, |p| p.collapsed = vec![ui::SEC_RECENT, ui::SEC_MARKS, ui::SEC_DRIVES, ui::SEC_PLACES], false); }
        (1, Some((p, _))) => open_in_new_tab(h, p),
        (2, Some((p, _))) => toggle_mark(h, p),
        (3, Some((p, _))) => forget_recent(Some(&p)),
        (4, _) => forget_recent(None),
        (5, Some((p, _))) => properties(&p),
        _ => {}
    }
}

/// The folder row under screen point (x, y) in the list: (row, its path). None over files, the empty rows, or outside.
pub fn folder_row_at(x: i32, y: i32) -> Option<(usize, PathBuf)> {
    if glist::active() { return glist::folder_at(x, y); }
    let list = ST.with(|c| c.try_borrow().map(|s| s.list).unwrap_or(ptr::null_mut()));
    let mut p = POINT { x, y };
    unsafe { windows_sys::Win32::Graphics::Gdi::ScreenToClient(list, &mut p) };
    let mut ht: LVHITTESTINFO = unsafe { std::mem::zeroed() };
    ht.pt = p;
    let i = usize::try_from(put(list, LVM_HITTEST, 0, &mut ht as *mut _ as isize)).ok()?;
    ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).filter(|r| r.dir).map(|r| (i, s.dir.join(&r.name)))))
}

/// Does what the file menu left to the window: rename in the list, WinRust's pins, the bookmark.
fn menu_choice(h: HWND, c: sh::Choice, paths: &[PathBuf]) {
    match c {
        sh::Choice::Rename => rename_selected(),
        sh::Choice::Delete => { let _ = sh::fileop(h, sh::Op::Delete, paths, &cur_dir()); }
        sh::Choice::PinStart => paths.iter().for_each(|p| bar_pin(0x5753_5450, "--pin-start", p)),
        sh::Choice::PinBar => paths.iter().for_each(|p| bar_pin(0x5753_5442, "--pin-bar", p)),
        sh::Choice::Bookmark => if let Some(p) = paths.first() { toggle_mark(h, p.clone()) },
        sh::Choice::Done => {}
    }
}

/// Hands `path` to the running bar (topbar) to pin: `code` says where (Start panel or taskbar). With no bar running,
/// topbar itself is started with `arg`, which writes it down for the bar's next start.
fn bar_pin(code: usize, arg: &str, path: &Path) {
    #[repr(C)]
    struct Cds { dw: usize, cb: u32, p: *const u8 }
    let wpath: Vec<u16> = path.as_os_str().encode_wide().collect();
    let cds = Cds { dw: code, cb: (wpath.len() * 2) as u32, p: wpath.as_ptr() as *const u8 };
    unsafe {
        let bar = FindWindowW(w("topbar").as_ptr(), ptr::null());
        if !bar.is_null() {
            // a bar that does not know the request (older than this file manager) answers 0: nothing else is tried,
            // starting topbar.exe with an argument it does not know would start a second bar
            let mut r = 0usize;
            SendMessageTimeoutW(bar, 0x4A, 0, &cds as *const _ as isize, SMTO_ABORTIFHUNG, 3000, &mut r);
            return;
        }
    }
    // no bar running: topbar writes the Start entry down for its next start (every topbar knows --pin-start)
    let exe = PathBuf::from(env::var_os("ProgramFiles").unwrap_or_default()).join(r"topbar	opbar.exe");
    if arg == "--pin-start" && exe.is_file() { let _ = std::process::Command::new(exe).arg(arg).arg(path).spawn(); }
}

/// Ctrl+A: every item (not the empty rows at the end).
fn select_all(list: HWND) {
    let n = ST.with_borrow(|st| st.rows.len());
    let it = |on: bool| LVITEMW { stateMask: LVIS_SELECTED, state: if on { LVIS_SELECTED } else { 0 }, ..unsafe { std::mem::zeroed() } };
    put(list, LVM_SETITEMSTATE, usize::MAX, &it(true) as *const _ as isize);
    for k in n..n + PAD_ROWS { put(list, LVM_SETITEMSTATE, k, &it(false) as *const _ as isize); }
}

/// Ctrl+Shift+C: the selected paths on the clipboard as text, one per line (quoted like Explorer's "Copy as path").
fn copy_paths(h: HWND, paths: &[PathBuf]) {
    let text: String = paths.iter().map(|p| format!("\"{}\"", p.display())).collect::<Vec<_>>().join("
");
    let _ = sh::clip_text(h as _, &text);
}

/// Renames row `i` to `new` (either list's rename box): next to the old name (a subfolder hit is renamed in its own
/// folder), noted for Ctrl+Z, the folder listed again once the box is gone. Separators are refused, so a rename can
/// never move the item.
fn rename_row(h: HWND, i: usize, new: &str) {
    if new.is_empty() || new.contains(['\\', '/', ':']) { return; }
    ST.with_borrow(|st| if let Some(r) = st.rows.get(i) { let old = st.dir.join(&r.name); let to = old.with_file_name(new); if old != to && fs::rename(&old, &to).is_ok() { sh::push_undo(sh::Undo::Renamed(to, old)); } });
    unsafe { PostMessageW(h, WM_APP, 0, 0) };
}

/// Starts renaming the focused row in the list (F2, or "Rename" from the menu).
fn rename_selected() {
    let list = ST.with_borrow(|st| st.list);
    let i = put(list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize);
    if i >= 0 { edit_label(list, i as usize); }
}

/// Opens row `i`'s name for typing; for a file only the part before the extension is selected, as in Explorer.
fn edit_label(list: HWND, i: usize) {
    if glist::active() { unsafe { glist::begin_rename(i) }; return; }
    let e = put(list, LVM_EDITLABELW, i, 0) as HWND;
    if e.is_null() || ST.with_borrow(|st| st.rows.get(i).is_none_or(|r| r.dir)) { return; }
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(e, buf.as_mut_ptr(), 512) }.max(0) as usize;
    if let Some(dot) = buf[..n].iter().rposition(|&c| c == '.' as u16).filter(|&d| d > 0) { put(e, EM_SETSEL, 0, dot as isize); }
}

fn open_in_new_tab(h: HWND, p: PathBuf) {
    if !p.is_dir() { open_file(&p); return; }
    tab_new();
    load(h, p);
}

/// The same keys typed in the other layout (QWERTY <-> ЙЦУКЕН): "сфкпщ" -> "cargo", "ljrevtyns" -> "документы".
fn flip_layout(q: &str) -> String {
    let en: Vec<char> = "qwertyuiop[]asdfghjkl;'zxcvbnm,.`QWERTYUIOP{}ASDFGHJKL:\"ZXCVBNM<>~".chars().collect();
    let ru: Vec<char> = "йцукенгшщзхъфывапролджэячсмитьбюёЙЦУКЕНГШЩЗХЪФЫВАПРОЛДЖЭЯЧСМИТЬБЮЁ".chars().collect();
    q.chars().map(|c| en.iter().position(|&x| x == c).map(|i| ru[i]).or_else(|| ru.iter().position(|&x| x == c).map(|i| en[i])).unwrap_or(c)).collect()
}

/// Files and folders under `dir` whose name contains `q` (any case), as rows with absolute names. Walks breadth-first and
/// stops after LOCAL_MAX hits or LOCAL_MS so a search from C:\ cannot hang the window. The same walk also matches `q` typed
/// in the other keyboard layout; those hits are used only when `q` itself found nothing.
/// Returns (rows, stopped early, rows are for the other layout).
/// shortcut: blocks the UI while it walks (up to LOCAL_MS); move to a thread if deeper searches are wanted.
fn local_search(dir: &Path, q: &str) -> (Vec<Row>, bool, bool) {
    const LOCAL_MAX: usize = 500;
    const LOCAL_MS: u128 = 1500;
    let (q, t0) = (q.to_lowercase(), std::time::Instant::now());
    let alt = flip_layout(&q);
    let (mut out, mut other, mut queue) = (vec![], vec![], std::collections::VecDeque::from([dir.to_path_buf()]));
    let mut cut = false;
    'walk: while let Some(d) = queue.pop_front() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            if out.len() >= LOCAL_MAX || t0.elapsed().as_millis() > LOCAL_MS { cut = true; break 'walk; }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && !ft.is_symlink() { queue.push_back(e.path()); } // no junction loops
            let name = e.file_name().to_string_lossy().to_lowercase();
            let hit = |v: &mut Vec<Row>| v.push(row(e.path().to_string_lossy().into_owned(), e.metadata().ok().as_ref()));
            if name.contains(&q) { hit(&mut out); } else if alt != q && other.len() < LOCAL_MAX && name.contains(&alt) { hit(&mut other); }
        }
    }
    if out.is_empty() && !other.is_empty() { (other, cut, true) } else { (out, cut, false) }
}

fn reload(h: HWND) { load(h, cur_dir()); }

/// (list, current dir, full paths of the selected rows)
fn selected() -> (HWND, PathBuf, Vec<PathBuf>) {
    ST.with_borrow(|st| {
        let (mut sel, mut i) = (vec![], -1isize);
        loop {
            i = put(st.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
            let Some(r) = usize::try_from(i).ok().and_then(|i| st.rows.get(i)) else { break };
            sel.push(st.dir.join(&r.name));
        }
        (st.list, st.dir.clone(), sel)
    })
}

fn col(list: HWND, i: i32, text: &str, width: i32, right: bool) {
    let mut t = w(text);
    let mut c: LVCOLUMNW = unsafe { std::mem::zeroed() };
    c.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT; c.pszText = t.as_mut_ptr(); c.cx = width;
    c.fmt = if right { LVCFMT_RIGHT } else { LVCFMT_LEFT };
    put(list, LVM_INSERTCOLUMNW, i as usize, &c as *const _ as isize);
}

/// Opens a new tab on the current folder.
fn tab_new() {
    let tab = ST.with_borrow_mut(|st| { st.tabs.push(st.dir.clone()); st.active = st.tabs.len() - 1; st.tab });
    tab_text(tab, 0, "", true);
    save_session();
}

fn tab_select(h: HWND, i: usize) {
    let dir = ST.with_borrow_mut(|st| { st.active = i; st.tabs[i].clone() });
    load(h, dir);
    save_session();
}

fn tab_close(h: HWND) { let a = ST.with_borrow(|st| st.active); tab_close_at(h, a); }

/// Closes tab `i` (the last one stays). The listing changes only when the active tab was the one closed.
fn tab_close_at(h: HWND, i: usize) {
    let (tab, was_active) = ST.with_borrow_mut(|st| {
        if st.tabs.len() < 2 || i >= st.tabs.len() { return (st.tab, None); }
        st.tabs.remove(i);
        let before = st.active;
        st.active = if i < before { before - 1 } else { before.min(st.tabs.len() - 1) };
        (st.tab, Some(i == before))
    });
    match was_active {
        Some(true) => { let a = ST.with_borrow(|st| st.active); tab_select(h, a); }
        Some(false) => tab_text(tab, 0, "", false),
        None => {}
    }
    save_session();
}

/// How the list shows the folder. The zoom (Ctrl+wheel) walks them in this order, like File Pilot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View { Details, Tree, List, Icons }

/// Zoom steps, smallest first: details (rows never grow), the tree, the list, then pictures growing in small steps
/// from medium through large to extra large icons (logical pixels). Ctrl+wheel and Ctrl+Plus / Minus walk them one at a
/// time; the view button and Ctrl+M jump between details and the pictures; nothing is reloaded at any step.
const ZOOM: [(View, i32); 22] = [
    (View::Details, ROW_PX), (View::Tree, ROW_PX), (View::List, ROW_PX),
    (View::Icons, 48), (View::Icons, 52), (View::Icons, 56), (View::Icons, 62), (View::Icons, 68), (View::Icons, 74),
    (View::Icons, 80), (View::Icons, 88), (View::Icons, 96), (View::Icons, 104), (View::Icons, 114), (View::Icons, 124),
    (View::Icons, 136), (View::Icons, 150), (View::Icons, 166), (View::Icons, 184), (View::Icons, 208), (View::Icons, 232),
    (View::Icons, 256),
];
const ZOOM_DETAILS: usize = 0;
const ROW_PX: i32 = 23; // details and tree rows: compact, the size they keep
const ZOOM_ICONS: usize = 11; // 96 px pictures
thread_local! {
    static ZOOM_AT: Cell<usize> = const { Cell::new(ZOOM_DETAILS) };
    static ZOOM_LAST_ICONS: Cell<usize> = const { Cell::new(ZOOM_ICONS) }; // for the toggle
    static SHOWN: Cell<f32> = const { Cell::new(0.0) }; // picture size on screen now (logical px), gliding to the step's
}
const ZOOM_TIMER: usize = 9;
const PREFS_TIMER: usize = 12; // the zoom step, saved a moment after the last change

/// Name of the view at the current zoom, for the status line: Details, Tree, List, M / L / XL Icons.
pub fn view_name() -> &'static str {
    match ZOOM[ZOOM_AT.get()] {
        (View::Details, _) => "Details",
        (View::Tree, _) => "Tree",
        (View::List, _) => "List",
        (View::Icons, s) if s < 96 => "M Icons",
        (View::Icons, s) if s < 184 => "L Icons",
        _ => "XL Icons",
    }
}

/// Shows the list at zoom step `i`. Between two picture sizes the size glides over a few frames (zoom_tick); a change of
/// view jumps. `refresh` = not the first layout (save, repaint).
fn set_zoom(h: HWND, i: usize, refresh: bool) {
    let i = i.min(ZOOM.len() - 1);
    let (view, size) = ZOOM[i];
    // another view: what is on screen now fades into it (growing zooming in, shrinking zooming out)
    if refresh && ST.with_borrow(|st| st.view) != view && glist::active() {
        glist::snapshot(if i > ZOOM_AT.get() { 1.0 } else { -1.0 });
    }
    ZOOM_AT.set(i);
    if view == View::Icons { ZOOM_LAST_ICONS.set(i); }
    let (list, was) = ST.with_borrow(|st| (st.list, st.view));
    if !refresh || was != view || SHOWN.get() == 0.0 {
        unsafe { KillTimer(h, ZOOM_TIMER) };
        ST.with_borrow_mut(|st| { st.view = view; st.thumbs = view == View::Icons; });
        SHOWN.set(size as f32);
        apply_size(list, view, size as f32);
        put(list, LVM_SETVIEW, match view { View::Icons => LV_VIEW_ICON, View::List => LV_VIEW_LIST, _ => LV_VIEW_DETAILS } as usize, 0);
        if view == View::List { put(list, LVM_SETCOLUMNWIDTH, 0, ui::sc(300) as isize); } // the width of each name
        if refresh {
            // the tree has rows of its own (the open folders' contents): into it or out of it, the rows are made again
            if was == View::Tree || view == View::Tree { let (keep, top) = kept(); show(h, &keep, top); }
            after_resize(list);
        }
        glist::zoom(false);
    } else if glist::active() {
        // the GPU list glides the pictures itself, at the screen's rate; the system list behind takes the size at once
        SHOWN.set(size as f32);
        apply_size(list, view, size as f32);
        glist::zoom(true);
    } else {
        unsafe { SetTimer(h, ZOOM_TIMER, 15, None) };
    }
    if refresh {
        PREFS.with_borrow_mut(|p| p.zoom = i);
        unsafe { SetTimer(h, PREFS_TIMER, 600, None) }; // saved once the wheel rests, not at every notch
        unsafe { ui::status_dirty(h) };
        ui::zoom_pop_show(); // the slider shows where the zoom is, like File Pilot's
    }
}

/// The picture size of the zoom step now (logical px), for the GPU list's glide.
pub fn zoom_target() -> f32 { ZOOM[ZOOM_AT.get()].1 as f32 }

/// (zoom step, number of steps), for the status corner and the slider.
pub fn zoom_at() -> (usize, usize) { (ZOOM_AT.get(), ZOOM.len()) }

/// One frame of the picture-size glide: a good part of the way to the step's size, until it is there.
fn zoom_tick(h: HWND) {
    let (view, target) = ZOOM[ZOOM_AT.get()];
    let target = target as f32;
    let mut s = SHOWN.get();
    s += (target - s) * 0.45;
    if (target - s).abs() < 0.75 {
        s = target;
        unsafe { KillTimer(h, ZOOM_TIMER) };
    }
    SHOWN.set(s);
    let list = ST.with_borrow(|st| st.list);
    apply_size(list, view, s);
    after_resize(list);
}

/// Lays the list out for `view` at size `s` (logical px; only the pictures have a size of their own). Nothing is listed
/// or asked for again; the rows are drawn by hand.
fn apply_size(list: HWND, view: View, s: f32) {
    match view {
        View::Icons => {
            let px = (s * ui::sc(100) as f32 / 100.0).round() as i32;
            if px != THUMB_PX.load(Relaxed) {
                // an empty image list of the picture size only makes the list lay its cells out that big: the pictures
                // themselves are drawn by hand (thumbs.rs), scaled
                put(list, LVM_SETIMAGELIST, LVSIL_NORMAL as usize, ui::sized_list(1, px, px));
                THUMB_PX.store(px, Relaxed);
            }
            let cx = (px + ui::sc(24)).max(ui::sc(92)); // narrow pictures still leave room for the name
            put(list, LVM_SETICONSPACING, 0, ((px + ui::sc(52)) as isize) << 16 | cx as isize);
            ui::row_height(list, false);
        }
        View::List => ui::row_height(list, false),
        View::Details | View::Tree => {
            ui::set_row(s.round() as i32);
            ui::row_height(list, true); // also has the rows measured again
        }
    }
}

/// After a size change: the focused row stays in view, the columns fit, one repaint (no erase: no flash).
fn after_resize(list: HWND) {
    if matches!(ST.with_borrow(|st| st.view), View::Details | View::Tree) { unsafe { ui::fit_columns(list) }; }
    let f = put(list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize);
    if f >= 0 { put(list, LVM_ENSUREVISIBLE, f as usize, 0); }
    unsafe { InvalidateRect(list, ptr::null(), 0) };
}

fn zoom_by(h: HWND, d: i32) {
    let i = (ZOOM_AT.get() as i32 + d).clamp(0, ZOOM.len() as i32 - 1) as usize;
    if i != ZOOM_AT.get() { set_zoom(h, i, true); }
}

/// Details <-> pictures (at the size last used).
fn toggle_view(h: HWND) {
    set_zoom(h, if ZOOM[ZOOM_AT.get()].0 == View::Icons { ZOOM_DETAILS } else { ZOOM_LAST_ICONS.get() }, true);
}

// ---------- the tree view: folders opened in place ----------

thread_local! {
    static OPEN: RefCell<std::collections::HashSet<String>> = RefCell::new(std::collections::HashSet::new()); // open folders (lower-case full paths)
    static KIDS: RefCell<std::collections::HashMap<String, Vec<Row>>> = RefCell::new(std::collections::HashMap::new()); // their rows
}

fn tkey(p: &Path) -> String { p.to_string_lossy().trim_end_matches('\\').to_lowercase() }

/// The rows of the tree: each row of `top`, and under an open folder its own rows, one level deeper. Names stay relative
/// to the shown folder ("sub\file"), so everything that joins a row's name onto the folder keeps working.
fn tree_rows(dir: &Path, top: &[Row]) -> Vec<Row> {
    fn walk(out: &mut Vec<Row>, dir: &Path, prefix: &str, rows: &[Row], depth: u16) {
        for r in rows {
            let mut x = r.clone();
            x.depth = depth;
            if !prefix.is_empty() { x.name = format!("{prefix}\\{}", r.name); }
            let k = tkey(&dir.join(&x.name));
            x.open = r.dir && OPEN.with_borrow(|o| o.contains(&k));
            let (open, name) = (x.open, x.name.clone());
            out.push(x);
            if open {
                let kids = KIDS.with_borrow(|m| m.get(&k).cloned()).unwrap_or_default();
                walk(out, dir, &name, &kids, depth + 1);
            }
        }
    }
    let mut out = Vec::with_capacity(top.len());
    walk(&mut out, dir, "", top, 0);
    out
}

/// Opens or closes folder row `i` of the tree (closing also closes everything open inside it).
fn tree_toggle(h: HWND, i: usize) {
    let Some((full, is_dir)) = ST.with_borrow(|st| st.rows.get(i).map(|r| (st.dir.join(&r.name), r.dir))) else { return };
    if !is_dir { return; }
    let k = tkey(&full);
    if OPEN.with_borrow(|o| o.contains(&k)) {
        let inside = format!("{k}\\");
        OPEN.with_borrow_mut(|o| o.retain(|x| x != &k && !x.starts_with(&inside)));
    } else {
        let Some(kids) = rows_of(&full) else { return }; // unreadable: stays closed
        if PREFS.with_borrow(|p| p.folder_sizes) { sizes::more(kids.iter().filter(|r| r.dir).map(|r| full.join(&r.name)).collect()); }
        KIDS.with_borrow_mut(|m| m.insert(k.clone(), kids));
        OPEN.with_borrow_mut(|o| o.insert(k));
    }
    let (keep, top) = kept();
    show(h, &keep, top);
}

/// The open folders read again (the shown folder changed on disk, or the sort changed).
fn tree_refresh() {
    let open: Vec<String> = OPEN.with_borrow(|o| o.iter().cloned().collect());
    for k in open {
        match rows_of(&PathBuf::from(&k)) {
            Some(rows) => KIDS.with_borrow_mut(|m| m.insert(k, rows)),
            None => { OPEN.with_borrow_mut(|o| o.remove(&k)); None }
        };
    }
}

fn tree_forget() {
    OPEN.with_borrow_mut(|o| o.clear());
    KIDS.with_borrow_mut(|m| m.clear());
}

/// Address box in search mode ("? terms").
fn search_focus() {
    let edit = ST.with_borrow(|st| st.edit);
    unsafe {
        SetWindowTextW(edit, w("? ").as_ptr());
        SetFocus(edit);
    }
    put(edit, EM_SETSEL, 2, 2);
}

/// Wallpaper for the desktop window.
mod wall {
    use windows_sys::Win32::{Graphics::Gdi::*, UI::WindowsAndMessaging::{IMAGE_BITMAP, LR_LOADFROMFILE, LoadImageW}};

    /// The BMP scaled ("fill": cropped to the screen's aspect) to cw x ch. 0 = no wallpaper file.
    pub fn load(cw: i32, ch: i32) -> isize {
        let Ok(base) = std::env::var("LOCALAPPDATA") else { return 0 };
        let path: Vec<u16> = format!(r"{base}\rs-shell\wallpaper.bmp").encode_utf16().chain([0]).collect();
        unsafe {
            let src = LoadImageW(std::ptr::null_mut(), path.as_ptr(), IMAGE_BITMAP, 0, 0, LR_LOADFROMFILE);
            if src.is_null() { return 0; }
            let mut bm: BITMAP = std::mem::zeroed();
            GetObjectW(src, size_of::<BITMAP>() as i32, &mut bm as *mut _ as _);
            let screen = GetDC(std::ptr::null_mut());
            let dst = CreateCompatibleBitmap(screen, cw, ch);
            let (sdc, ddc) = (CreateCompatibleDC(screen), CreateCompatibleDC(screen));
            let (o1, o2) = (SelectObject(sdc, src), SelectObject(ddc, dst));
            SetStretchBltMode(ddc, HALFTONE);
            let (sw, sh) = if bm.bmWidth * ch > bm.bmHeight * cw { (bm.bmHeight * cw / ch, bm.bmHeight) } else { (bm.bmWidth, bm.bmWidth * ch / cw) };
            StretchBlt(ddc, 0, 0, cw, ch, sdc, (bm.bmWidth - sw) / 2, (bm.bmHeight - sh) / 2, sw, sh, SRCCOPY);
            SelectObject(sdc, o1); SelectObject(ddc, o2);
            DeleteDC(sdc); DeleteDC(ddc); ReleaseDC(std::ptr::null_mut(), screen); DeleteObject(src);
            dst as isize
        }
    }

    pub fn paint(dc: HDC, bmp: isize, cw: i32, ch: i32) {
        unsafe {
            let m = CreateCompatibleDC(dc);
            let old = SelectObject(m, bmp as _);
            BitBlt(dc, 0, 0, cw, ch, m, 0, 0, SRCCOPY);
            SelectObject(m, old);
            DeleteDC(m);
        }
    }
}

/// Shell integration through COM (windows crate): file operations, context menu, drag and drop.
mod sh {
    use std::{cell::RefCell, ffi::c_void, os::windows::ffi::OsStrExt, path::PathBuf};
    use windows::{core::*, Win32::{Foundation::*, System::{Com::*, DataExchange::RegisterClipboardFormatW, Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock}, Ole::*, SystemServices::MODIFIERKEYS_FLAGS}, UI::{Shell::{Common::ITEMIDLIST, *}, WindowsAndMessaging::*}}};

    #[derive(Clone, Copy, PartialEq)]
    pub enum Op { Copy, Move, Delete, Purge } // Purge = delete for good (Shift+Delete), the shell asks first

    /// What Ctrl+Z puts back, newest last. Each entry is one action (a whole paste, drop or delete).
    pub enum Undo {
        Moved(Vec<(PathBuf, PathBuf)>), // (where it is now, where it was)
        Copied(Vec<PathBuf>),           // the copies made
        Recycled(Vec<PathBuf>),         // where the items sit inside the Recycle Bin ($Recycle.Bin\...\$R...)
        Renamed(PathBuf, PathBuf),      // (new path, old path)
        Created(PathBuf),               // a new folder / file
    }

    thread_local! { static UNDO: RefCell<Vec<Undo>> = const { RefCell::new(Vec::new()) }; }

    /// The menu's name for what Ctrl+Z would take back, if anything.
    pub fn undo_label() -> Option<&'static str> {
        UNDO.with_borrow(|v| v.last().map(|u| match u {
            Undo::Moved(_) => "Undo move", Undo::Copied(_) => "Undo copy", Undo::Recycled(_) => "Undo delete",
            Undo::Renamed(..) => "Undo rename", Undo::Created(_) => "Undo new item",
        }))
    }

    pub fn push_undo(u: Undo) {
        UNDO.with_borrow_mut(|v| { v.push(u); if v.len() > 50 { v.remove(0); } });
    }

    fn path_of(it: &IShellItem) -> Option<PathBuf> {
        unsafe {
            let p = it.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const c_void));
            s.map(PathBuf::from)
        }
    }

    /// Takes the newest action back: moves return, copies and new items go to the Recycle Bin, deleted items are
    /// restored from it, renames are reversed. false = nothing to undo.
    pub fn undo(h: *mut c_void) -> bool {
        let Some(u) = UNDO.with_borrow_mut(|v| v.pop()) else { return false };
        let item = |p: &PathBuf| unsafe { SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide(p).as_ptr()), None) };
        let run = |f: &dyn Fn(&IFileOperation) -> Result<()>, flags: FILEOPERATION_FLAGS| -> Result<()> {
            unsafe {
                let fo: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
                fo.SetOwnerWindow(HWND(h))?;
                fo.SetOperationFlags(flags)?;
                f(&fo)?;
                fo.PerformOperations()
            }
        };
        let _ = match u {
            Undo::Moved(v) => run(&|fo| {
                for (now, was) in &v {
                    let (Some(dir), Some(name)) = (was.parent().map(PathBuf::from), was.file_name()) else { continue };
                    let n: Vec<u16> = name.encode_wide().chain([0]).collect();
                    unsafe { fo.MoveItem(&item(now)?, &item(&dir)?, PCWSTR(n.as_ptr()), None)?; }
                }
                Ok(())
            }, FOF_NOCONFIRMMKDIR),
            Undo::Copied(v) => run(&|fo| { for p in &v { unsafe { fo.DeleteItem(&item(p)?, None)?; } } Ok(()) }, FOF_ALLOWUNDO | FOF_NOCONFIRMATION),
            Undo::Created(p) => run(&|fo| unsafe { fo.DeleteItem(&item(&p)?, None) }, FOF_ALLOWUNDO | FOF_NOCONFIRMATION),
            Undo::Renamed(now, was) => std::fs::rename(&now, &was).map_err(|_| Error::from(E_FAIL)),
            Undo::Recycled(v) => {
                // the Recycle Bin's own "Restore" on each item: back where it came from, and gone from the bin. The
                // delete reported the files' places on disk; the bin's items for them are found by that place
                let keys: Vec<String> = v.iter().map(|p| p.to_string_lossy().to_lowercase()).collect();
                let mut found: Vec<IShellItem> = vec![];
                unsafe {
                    if let Ok(bin) = SHGetKnownFolderItem::<IShellItem>(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, None) {
                        if let Ok(en) = bin.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) {
                            let mut one = [None];
                            while en.Next(&mut one, None).is_ok() {
                                let Some(it) = one[0].take() else { break };
                                if path_of(&it).is_some_and(|p| keys.contains(&p.to_string_lossy().to_lowercase())) { found.push(it); }
                            }
                        }
                    }
                }
                for it in &found {
                    unsafe {
                        let Ok(cm) = it.BindToHandler::<_, IContextMenu>(None, &BHID_SFUIObject) else { continue };
                        let Ok(menu) = CreatePopupMenu() else { continue };
                        if cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL).is_ok() {
                            let ci = CMINVOKECOMMANDINFO { cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32, hwnd: HWND(h), lpVerb: PCSTR(b"undelete\0".as_ptr()), nShow: SW_SHOWNORMAL.0, ..Default::default() };
                            let _ = cm.InvokeCommand(&ci);
                        }
                        let _ = DestroyMenu(menu);
                    }
                }
                Ok(())
            }
        };
        true
    }

    /// Notes where each item of a file operation ended up, for Undo.
    #[implement(IFileOperationProgressSink)]
    struct Sink { moved: RefCell<Vec<(PathBuf, PathBuf)>>, copied: RefCell<Vec<PathBuf>>, recycled: RefCell<Vec<PathBuf>> }

    impl IFileOperationProgressSink_Impl for Sink_Impl {
        fn StartOperations(&self) -> Result<()> { Ok(()) }
        fn FinishOperations(&self, _: HRESULT) -> Result<()> { Ok(()) }
        fn PreRenameItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> Result<()> { Ok(()) }
        fn PostRenameItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR, _: HRESULT, _: Ref<IShellItem>) -> Result<()> { Ok(()) }
        fn PreMoveItem(&self, _: u32, _: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR) -> Result<()> { Ok(()) }
        fn PostMoveItem(&self, _: u32, it: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR, hr: HRESULT, new: Ref<IShellItem>) -> Result<()> {
            if hr.is_ok() { if let (Some(was), Some(now)) = (it.ok().ok().and_then(path_of), new.ok().ok().and_then(path_of)) { self.moved.borrow_mut().push((now, was)); } }
            Ok(())
        }
        fn PreCopyItem(&self, _: u32, _: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR) -> Result<()> { Ok(()) }
        fn PostCopyItem(&self, _: u32, _: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR, hr: HRESULT, new: Ref<IShellItem>) -> Result<()> {
            if hr.is_ok() { if let Some(now) = new.ok().ok().and_then(path_of) { self.copied.borrow_mut().push(now); } }
            Ok(())
        }
        fn PreDeleteItem(&self, _: u32, _: Ref<IShellItem>) -> Result<()> { Ok(()) }
        fn PostDeleteItem(&self, _: u32, _: Ref<IShellItem>, hr: HRESULT, new: Ref<IShellItem>) -> Result<()> {
            // recycled: `new` is the item inside the Recycle Bin (deleted for good: none)
            if hr.is_ok() { if let Some(p) = new.ok().ok().and_then(path_of) { self.recycled.borrow_mut().push(p); } }
            Ok(())
        }
        fn PreNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> Result<()> { Ok(()) }
        fn PostNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR, _: &PCWSTR, _: u32, _: HRESULT, _: Ref<IShellItem>) -> Result<()> { Ok(()) }
        fn UpdateProgress(&self, _: u32, _: u32) -> Result<()> { Ok(()) }
        fn ResetTimer(&self) -> Result<()> { Ok(()) }
        fn PauseTimer(&self) -> Result<()> { Ok(()) }
        fn ResumeTimer(&self) -> Result<()> { Ok(()) }
    }

    fn known(id: &GUID) -> Option<PathBuf> {
        unsafe {
            let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const c_void));
            s.map(PathBuf::from)
        }
    }

    /// [user Desktop, public Desktop] (the user one may be redirected, e.g. to OneDrive).
    pub fn desktop_dirs() -> Vec<PathBuf> { [&FOLDERID_Desktop, &FOLDERID_PublicDesktop].iter().filter_map(|id| known(id)).collect() }

    /// The user's everyday folders where they really are (they may be moved, e.g. to OneDrive), for the side pane.
    pub fn places() -> Vec<PathBuf> {
        [&FOLDERID_Desktop, &FOLDERID_Downloads, &FOLDERID_Documents, &FOLDERID_Pictures]
            .iter().filter_map(|id| known(id)).filter(|p| p.is_dir()).collect()
    }

    /// The Recycle Bin's small system icon (its index in the system image list), -1 if the shell gives none.
    pub fn bin_icon() -> i32 {
        unsafe {
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            let bin: Vec<u16> = "::{645FF040-5081-101B-9F08-00AA002F954E}".encode_utf16().chain([0]).collect();
            if SHParseDisplayName(PCWSTR(bin.as_ptr()), None, &mut pidl, 0, None).is_err() || pidl.is_null() { return -1; }
            use windows_sys::Win32::UI::Shell as S;
            let mut fi: S::SHFILEINFOW = std::mem::zeroed();
            let ok = S::SHGetFileInfoW(pidl as *const u16, 0, &mut fi, size_of::<S::SHFILEINFOW>() as u32, S::SHGFI_PIDL | S::SHGFI_SYSICONINDEX | S::SHGFI_SMALLICON);
            CoTaskMemFree(Some(pidl as *const c_void));
            if ok != 0 { fi.iIcon } else { -1 }
        }
    }

    pub fn init() { unsafe { let _ = OleInitialize(None); } }

    fn wide(p: &PathBuf) -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() }

    /// Copy / move into `to`, or recycle. Must not be called while ST is borrowed (the shell pumps messages).
    pub fn fileop(h: *mut c_void, op: Op, from: &[PathBuf], to: &PathBuf) -> Result<()> {
        unsafe {
            let fo: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            fo.SetOwnerWindow(HWND(h))?;
            fo.SetOperationFlags(match op { Op::Delete => FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE, Op::Purge => FILEOPERATION_FLAGS(0), _ => FOF_RENAMEONCOLLISION })?;
            // where everything ends up is noted, for Ctrl+Z
            let sink: IFileOperationProgressSink = Sink { moved: RefCell::new(vec![]), copied: RefCell::new(vec![]), recycled: RefCell::new(vec![]) }.into();
            let cookie = fo.Advise(&sink).ok();
            let item = |p: &PathBuf| SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide(p).as_ptr()), None);
            let dest = if matches!(op, Op::Delete | Op::Purge) { None } else { Some(item(to)?) };
            for p in from {
                let it = item(p)?;
                match (op, &dest) {
                    (Op::Copy, Some(d)) => fo.CopyItem(&it, d, PCWSTR::null(), None)?,
                    (Op::Move, Some(d)) => fo.MoveItem(&it, d, PCWSTR::null(), None)?,
                    _ => fo.DeleteItem(&it, None)?,
                }
            }
            let r = fo.PerformOperations();
            if let Some(c) = cookie { let _ = fo.Unadvise(c); }
            let s = sink.as_impl();
            let (moved, copied, recycled) = (s.moved.take(), s.copied.take(), s.recycled.take());
            if !moved.is_empty() { push_undo(Undo::Moved(moved)); }
            if !copied.is_empty() { push_undo(Undo::Copied(copied)); }
            if !recycled.is_empty() { push_undo(Undo::Recycled(recycled)); }
            r
        }
    }

    /// Puts `paths` on the system clipboard like Explorer: the shell's own data object plus the "Preferred DropEffect"
    /// marker (copy or cut), so Ctrl+V works in a real Explorer window and in other file managers.
    pub fn clip_set(h: *mut c_void, paths: &[PathBuf], cut: bool) -> Result<()> {
        unsafe {
            let data: IDataObject = ui(HWND(h), paths)?;
            let fmt = FORMATETC { cfFormat: RegisterClipboardFormatW(w!("Preferred DropEffect")) as u16, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0 as u32, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
            let hg = GlobalAlloc(GMEM_MOVEABLE, 4)?;
            *(GlobalLock(hg) as *mut u32) = if cut { DROPEFFECT_MOVE.0 } else { DROPEFFECT_COPY.0 };
            let _ = GlobalUnlock(hg);
            let med = STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: hg }, pUnkForRelease: std::mem::ManuallyDrop::new(None) };
            data.SetData(&fmt, &med, true)?; // the data object owns the memory from here
            OleSetClipboard(&data)
        }
    }

    /// What is on the system clipboard as files: (paths, cut?). None = no files there.
    pub fn clip_get() -> Option<(Vec<PathBuf>, bool)> {
        unsafe {
            let o = OleGetClipboard().ok()?;
            let paths = hdrop_paths(&o).ok().filter(|p| !p.is_empty())?;
            let fmt = FORMATETC { cfFormat: RegisterClipboardFormatW(w!("Preferred DropEffect")) as u16, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0 as u32, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
            let cut = o.GetData(&fmt).ok().is_some_and(|mut m| {
                let p = GlobalLock(m.u.hGlobal) as *const u32;
                let v = if p.is_null() { 0 } else { *p };
                let _ = GlobalUnlock(m.u.hGlobal);
                ReleaseStgMedium(&mut m);
                v & DROPEFFECT_MOVE.0 != 0
            });
            Some((paths, cut))
        }
    }

    pub fn clip_clear() { unsafe { let _ = OleSetClipboard(None); } }

    /// Plain text on the clipboard.
    pub fn clip_text(h: *mut c_void, text: &str) -> Result<()> {
        use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
        let wt: Vec<u16> = text.encode_utf16().chain([0]).collect();
        unsafe {
            OpenClipboard(Some(HWND(h)))?;
            let _ = EmptyClipboard();
            let r = (|| -> Result<()> {
                let g = GlobalAlloc(GMEM_MOVEABLE, wt.len() * 2)?;
                std::ptr::copy_nonoverlapping(wt.as_ptr(), GlobalLock(g) as *mut u16, wt.len());
                let _ = GlobalUnlock(g);
                SetClipboardData(13, Some(HANDLE(g.0)))?; // CF_UNICODETEXT; the clipboard owns the memory from here
                Ok(())
            })();
            let _ = CloseClipboard();
            r
        }
    }

    struct Pidls(Vec<*mut ITEMIDLIST>);
    impl Drop for Pidls { fn drop(&mut self) { for p in &self.0 { unsafe { CoTaskMemFree(Some(*p as *const c_void)) }; } } }

    /// A shell UI object (IContextMenu, IDataObject...) for items that all live in one folder.
    fn ui<T: Interface>(h: HWND, paths: &[PathBuf]) -> Result<T> {
        unsafe {
            let mut abs = Pidls(vec![]);
            for p in paths {
                let mut pidl = std::ptr::null_mut();
                SHParseDisplayName(PCWSTR(wide(p).as_ptr()), None, &mut pidl, 0, None)?;
                abs.0.push(pidl);
            }
            let parent: IShellFolder = SHBindToParent(abs.0[0], None)?;
            let kids: Vec<*const ITEMIDLIST> = abs.0.iter().map(|&p| ILFindLastID(p) as *const _).collect();
            parent.GetUIObjectOf(h, &kids, None)
        }
    }

    thread_local! { static CM2: RefCell<Option<IContextMenu2>> = const { RefCell::new(None) }; }

    /// What a menu choice leaves to the window (the shell did everything else).
    #[derive(PartialEq, Clone, Copy)]
    pub enum Choice { Done, Rename, Delete, PinStart, PinBar, Bookmark }
    const MY_START: u32 = 0x9001;
    const MY_BAR: u32 = 0x9002;
    const MY_MARK: u32 = 0x9003;

    /// Forwards owner-draw / init-popup menu messages so submenus like "Send to" get filled in.
    pub fn menu_msg(m: u32, wp: usize, lp: isize) -> bool {
        CM2.with_borrow(|c| c.as_ref().is_some_and(|c| unsafe { c.HandleMenuMsg(m, WPARAM(wp), LPARAM(lp)).is_ok() }))
    }

    /// The shell's context menu for `paths` (so 7-Zip, Git, "Open with", "Send to" and the rest show up), shown in our
    /// own menu window (menu.rs), with WinRust's own pins and bookmark added. What the caller still has to do comes back.
    pub fn show(h: *mut c_void, paths: &[PathBuf]) -> Result<Choice> {
        unsafe {
            let hwnd = HWND(h);
            let cm: IContextMenu = ui(hwnd, paths)?;
            let menu = CreatePopupMenu()?;
            cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL | CMF_CANRENAME).ok()?;
            let cm2 = cm.cast::<IContextMenu2>().ok();
            let cm3 = cm.cast::<IContextMenu3>().ok();
            CM2.set(cm2.clone());
            // the shell's handler fills submenus and draws some icons when told to (the newer interface first: some
            // handlers draw only through it)
            // handlers differ in which one they draw through: drawing is asked of both (twice does no harm); filling a
            // submenu only of the first that takes it (twice would list "Send to" twice)
            let msg = |m: u32, wp: usize, lp: isize| {
                let (a, b) = (|| cm3.as_ref().is_some_and(|c| c.HandleMenuMsg2(m, WPARAM(wp), LPARAM(lp), None).is_ok()), || cm2.as_ref().is_some_and(|c| c.HandleMenuMsg(m, WPARAM(wp), LPARAM(lp)).is_ok()));
                if m == WM_DRAWITEM || m == WM_MEASUREITEM { let x = a(); b() || x } else { a() || b() }
            };
            let verb = |id: u32| -> String {
                if id == 0 { return String::new(); }
                let mut b = [0u16; 128];
                if cm.GetCommandString(id as usize - 1, GCS_VERBW, None, PSTR(b.as_mut_ptr() as *mut u8), 128).is_err() { return String::new(); }
                String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(0)]).to_lowercase()
            };
            let mut items = crate::menu::from_hmenu(menu.0 as _, &msg, &verb);
            // WinRust's own: the bar's Start panel and taskbar (Windows' pins are left out), a bookmark for a folder;
            // before "Send to"
            use crate::menu::Item;
            let mut ours = vec![Item::new(MY_START, "\u{E718}", "Pin to Start", ""), Item::new(MY_BAR, "\u{E718}", "Pin to taskbar", "")];
            if paths.len() == 1 && paths[0].is_dir() { ours.push(Item::new(MY_MARK, if super::marked(&paths[0]) { "\u{E735}" } else { "\u{E734}" }, if super::marked(&paths[0]) { "Remove bookmark" } else { "Bookmark" }, "Ctrl+D")); }
            ours.push(Item::sep());
            let at = items.iter().position(|i| !i.sub.is_empty() && i.text.eq_ignore_ascii_case("send to")).map_or(items.len(), |i| i.saturating_sub(1).max(1).min(items.len()));
            items.splice(at..at, ours);
            let mut pt = POINT::default();
            GetCursorPos(&mut pt)?;
            let id = crate::menu::track(h as _, items, (pt.x, pt.y));
            let mut rename = Choice::Done;
            if id >= MY_START {
                rename = match id { MY_START => Choice::PinStart, MY_BAR => Choice::PinBar, _ => Choice::Bookmark };
            } else if id > 0 {
                if verb(id) == "rename" {
                    rename = Choice::Rename; // renaming happens in our list, not through the shell
                } else if verb(id) == "delete" {
                    rename = Choice::Delete; // through our file operation: Ctrl+Z can bring it back
                } else {
                    let ci = CMINVOKECOMMANDINFO { cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32, hwnd, lpVerb: PCSTR((id - 1) as usize as *const u8), nShow: SW_SHOWNORMAL.0, ..Default::default() };
                    cm.InvokeCommand(&ci)?;
                }
            }
            CM2.set(None);
            DestroyMenu(menu)?;
            Ok(rename)
        }
    }

    /// Starts an OLE drag of `paths` (blocks until dropped).
    pub fn drag(h: *mut c_void, paths: &[PathBuf]) -> Result<()> {
        unsafe {
            let hwnd = HWND(h);
            let data: IDataObject = ui(hwnd, paths)?;
            SHDoDragDrop(Some(hwnd), &data, None, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK)?;
            Ok(())
        }
    }

    fn hdrop_paths(o: &IDataObject) -> Result<Vec<PathBuf>> {
        unsafe {
            let fmt = FORMATETC { cfFormat: CF_HDROP.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0 as u32, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
            let mut med = o.GetData(&fmt)?;
            let hd = HDROP(med.u.hGlobal.0);
            let mut out = vec![];
            for i in 0..DragQueryFileW(hd, u32::MAX, None) {
                let mut buf = vec![0u16; DragQueryFileW(hd, i, None) as usize + 1];
                let n = DragQueryFileW(hd, i, Some(&mut buf)) as usize;
                out.push(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
            }
            ReleaseStgMedium(&mut med);
            Ok(out)
        }
    }

    #[implement(IDropTarget)]
    struct DropT { main: isize, paths: RefCell<Vec<PathBuf>>, tabs: bool }

    thread_local! {
        /// A drag resting on another tab: which one and since when (after a moment it opens, to drop into its folder).
        static TAB_HOVER: std::cell::Cell<Option<(usize, std::time::Instant)>> = const { std::cell::Cell::new(None) };
    }
    const TAB_OPEN_MS: u128 = 550;

    /// Where a drop at screen point `pt` goes: the folder under it (its row, to highlight), or the shown folder.
    fn target(pt: &POINTL) -> (PathBuf, isize) {
        match super::folder_row_at(pt.x, pt.y) { Some((i, p)) => (p, i as isize), None => (super::cur_dir(), -1) }
    }

    /// Over the tab row: the folder of the tab under the pointer.
    fn tab_target(pt: &POINTL) -> Option<(usize, PathBuf)> {
        let i = super::ui::tab_at(pt.x, pt.y)?;
        super::ST.with(|c| c.try_borrow().ok().and_then(|s| s.tabs.get(i).cloned())).map(|p| (i, p))
    }

    /// What dropping `paths` into `to` does: copy (other drive, or Ctrl), move, or nothing (into the folder they are in,
    /// or into one of themselves).
    fn drop_effect(paths: &[PathBuf], to: &PathBuf, k: MODIFIERKEYS_FLAGS) -> DROPEFFECT {
        if paths.is_empty() { return effect(k); }
        if paths.iter().any(|p| p == to || to.starts_with(p)) || paths.iter().all(|p| p.parent() == Some(to.as_path())) { return DROPEFFECT_NONE; }
        if k.0 & 8 != 0 || paths[0].components().next() != to.components().next() { DROPEFFECT_COPY } else { DROPEFFECT_MOVE }
    }

    fn effect(k: MODIFIERKEYS_FLAGS) -> DROPEFFECT { if k.0 & 8 != 0 { DROPEFFECT_COPY } else { DROPEFFECT_MOVE } } // 8 = MK_CONTROL

    impl IDropTarget_Impl for DropT_Impl {
        fn DragEnter(&self, o: Ref<'_, IDataObject>, k: MODIFIERKEYS_FLAGS, pt: &POINTL, e: *mut DROPEFFECT) -> Result<()> {
            *self.paths.borrow_mut() = o.ok().ok().and_then(|o| hdrop_paths(o).ok()).unwrap_or_default();
            self.DragOver(k, pt, e)
        }
        fn DragOver(&self, k: MODIFIERKEYS_FLAGS, pt: &POINTL, e: *mut DROPEFFECT) -> Result<()> {
            if self.tabs {
                // held on another tab for a moment, it opens: the drag goes on into that folder's list
                let Some((i, to)) = tab_target(pt) else { TAB_HOVER.set(None); unsafe { *e = DROPEFFECT_NONE }; return Ok(()) };
                let active = super::ST.with(|c| c.try_borrow().map_or(i, |s| s.active));
                match TAB_HOVER.get() {
                    _ if i == active => TAB_HOVER.set(None),
                    Some((j, t)) if j == i => if t.elapsed().as_millis() >= TAB_OPEN_MS {
                        TAB_HOVER.set(None);
                        unsafe { let _ = PostMessageW(Some(HWND(self.main as *mut c_void)), super::ui::WM_UI, WPARAM(1), LPARAM(i as isize)); }
                    },
                    _ => TAB_HOVER.set(Some((i, std::time::Instant::now()))),
                }
                unsafe { *e = drop_effect(&self.paths.borrow(), &to, k) };
                return Ok(());
            }
            if super::desk() { unsafe { *e = effect(k) }; return Ok(()); }
            let (to, row) = target(pt);
            let fx = drop_effect(&self.paths.borrow(), &to, k);
            super::ui::set_drop_row(if fx == DROPEFFECT_NONE { -1 } else { row }); // the folder it would go into lights up
            unsafe { *e = fx };
            Ok(())
        }
        fn DragLeave(&self) -> Result<()> { TAB_HOVER.set(None); super::ui::set_drop_row(-1); Ok(()) }
        fn Drop(&self, o: Ref<'_, IDataObject>, k: MODIFIERKEYS_FLAGS, pt: &POINTL, e: *mut DROPEFFECT) -> Result<()> {
            if super::desk::dragging() {
                // desktop icons dropped back on the desktop: a move of the icons, not of the files (the dragger finishes it)
                super::desk::dropped_here((pt.x, pt.y));
                unsafe { *e = DROPEFFECT_NONE };
                return Ok(());
            }
            super::ui::set_drop_row(-1);
            TAB_HOVER.set(None);
            let paths = hdrop_paths(o.ok()?)?;
            let to = if self.tabs {
                match tab_target(pt) { Some((_, p)) => p, None => { unsafe { *e = DROPEFFECT_NONE }; return Ok(()) } }
            } else if super::desk() { super::cur_dir() } else { target(pt).0 };
            // like Explorer: same drive moves, other drive copies, Ctrl forces copy; into where it already is: nothing
            let fx = drop_effect(&paths, &to, k);
            unsafe { *e = fx };
            if fx == DROPEFFECT_NONE { return Ok(()); }
            let _ = fileop(self.main as *mut c_void, if fx == DROPEFFECT_COPY { Op::Copy } else { Op::Move }, &paths, &to);
            // the desktop lays its icons out again; a folder window follows the change by itself (watch)
            if super::desk() { unsafe { let _ = PostMessageW(Some(HWND(self.main as *mut c_void)), WM_APP, WPARAM(0), LPARAM(0)); } }
            Ok(())
        }
    }

    pub fn register_drop(list: *mut c_void, main: isize) -> Result<()> {
        let t: IDropTarget = DropT { main, paths: RefCell::new(vec![]), tabs: false }.into();
        unsafe { RegisterDragDrop(HWND(list), &t) }
    }

    /// The tab row takes drops too: onto a tab goes into its folder, and resting on one opens it. On the toolbar and
    /// on the window itself (the title row lets the pointer through to it, for moving the window).
    pub fn register_tab_drop(wnd: *mut c_void, main: isize) -> Result<()> {
        let t: IDropTarget = DropT { main, paths: RefCell::new(vec![]), tabs: true }.into();
        unsafe { RegisterDragDrop(HWND(wnd), &t) }
    }
}


/// The desktop's own logic: icons stay where the user puts them (positions saved per name), dropping one onto a folder
/// icon moves it into the folder, and a host window for Wallpaper Engine (it draws behind the icons).
mod desk {
    #![allow(unsafe_op_in_unsafe_fn)]
    use super::*;
    use std::{cell::Cell, collections::{HashMap, HashSet}};
    use windows_sys::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, HDC, ScreenToClient};

    pub const SPAWN_WORKERW: u32 = 0x052C; // what wallpaper apps send to "Progman" to get a window behind the icons
    const KEY: u32 = 0x0001_0001; // rgb(1, 0, 1): the list's background colour, made see-through while a live wallpaper runs

    thread_local! {
        static DRAG_FROM: Cell<Option<(i32, i32)>> = const { Cell::new(None) }; // our own icon drag: where it started (list coords)
        static DROP_AT: Cell<Option<(i32, i32)>> = const { Cell::new(None) }; // ...and where it was dropped (screen coords)
        static WORKERW: Cell<isize> = const { Cell::new(0) };
        static LIVE: Cell<bool> = const { Cell::new(false) }; // a live wallpaper is running in WORKERW
    }

    fn pos_file() -> PathBuf {
        PathBuf::from(env::var_os("APPDATA").unwrap_or_default()).join(r"rust-explorer\desktop-icons.txt")
    }

    /// Saved icon positions: name -> (x, y) in list pixels.
    fn saved() -> HashMap<String, (i32, i32)> {
        fs::read_to_string(pos_file()).unwrap_or_default().lines().filter_map(|l| {
            let mut p = l.splitn(3, '\t');
            Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.to_string()))
        }).map(|(x, y, n): (i32, i32, String)| (n, (x, y))).collect()
    }

    fn names() -> Vec<String> { ST.with_borrow(|st| st.rows.iter().map(|r| r.name.clone()).collect()) }

    fn pos(list: HWND, i: usize) -> (i32, i32) {
        let mut p = POINT { x: 0, y: 0 };
        put(list, LVM_GETITEMPOSITION, i, &mut p as *mut _ as isize);
        (p.x, p.y)
    }

    fn set_pos(list: HWND, i: usize, (x, y): (i32, i32)) {
        let p = POINT { x, y };
        put(list, LVM_SETITEMPOSITION32, i, &p as *const _ as isize);
    }

    /// Writes every icon's position (after a move or a fresh layout).
    pub fn save(list: HWND) {
        let lines: String = names().iter().enumerate().map(|(i, n)| { let (x, y) = pos(list, i); format!("{x}\t{y}\t{n}\n") }).collect();
        let f = pos_file();
        let _ = fs::create_dir_all(f.parent().unwrap_or(&f));
        let _ = fs::write(f, lines);
    }

    /// The icon grid: cell size, and how many columns / rows fit.
    struct Grid { cx: i32, cy: i32, cols: i32, rows: i32 }

    impl Grid {
        fn of(list: HWND) -> Grid {
            let s = put(list, LVM_GETITEMSPACING, 0, 0) as u32;
            let (cx, cy) = (((s & 0xFFFF) as i32).max(1), ((s >> 16) as i32).max(1));
            let mut r: RECT = unsafe { std::mem::zeroed() };
            unsafe { GetClientRect(list, &mut r) };
            if r.bottom <= 0 {
                // the first fill runs in WM_CREATE, before WM_SIZE gives the list its size: the size it is about to get
                r = unsafe { RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) - desk_top() - desk_bottom() } };
            }
            Grid { cx, cy, cols: (r.right / cx).max(1), rows: (r.bottom / cy).max(1) }
        }
        /// Nearest cell to a position; None if it is off the grid (screen got smaller).
        fn cell(&self, (x, y): (i32, i32)) -> Option<(i32, i32)> {
            let c = ((x + self.cx / 2).div_euclid(self.cx), (y + self.cy / 2).div_euclid(self.cy));
            (c.0 >= 0 && c.1 >= 0 && c.0 < self.cols && c.1 < self.rows).then_some(c)
        }
        fn at(&self, (c, r): (i32, i32)) -> (i32, i32) { (c * self.cx, r * self.cy) }
        /// Free cell nearest to `want`; with no wish the first free one going down the columns, like Windows.
        fn free(&self, taken: &HashSet<(i32, i32)>, want: Option<(i32, i32)>) -> (i32, i32) {
            let all = (0..self.cols).flat_map(|c| (0..self.rows).map(move |r| (c, r))).filter(|c| !taken.contains(c));
            match want {
                Some((wc, wr)) => all.min_by_key(|&(c, r)| (c - wc).pow(2) + (r - wr).pow(2)),
                None => all.min(),
            }
            .unwrap_or((0, 0))
        }
    }

    /// Fills the (non-virtual) desktop list with `n` rows and puts every icon on its saved cell, new ones on free cells.
    pub fn fill(list: HWND, n: usize) {
        for i in 0..n {
            let mut it: LVITEMW = unsafe { std::mem::zeroed() };
            it.mask = LVIF_TEXT | LVIF_IMAGE;
            it.iItem = i as i32;
            it.pszText = usize::MAX as *mut u16; // LPSTR_TEXTCALLBACKW: text and icon come from LVN_GETDISPINFO
            it.iImage = -1; // I_IMAGECALLBACK
            put(list, LVM_INSERTITEMW, 0, &it as *const _ as isize);
        }
        let (g, old, names) = (Grid::of(list), saved(), names());
        let mut taken = HashSet::new();
        let mut later = vec![];
        for (i, name) in names.iter().enumerate() {
            match old.get(name).and_then(|&p| g.cell(p)).filter(|c| !taken.contains(c)) {
                Some(c) => { taken.insert(c); set_pos(list, i, g.at(c)); }
                None => later.push(i),
            }
        }
        for i in later {
            let c = g.free(&taken, None);
            taken.insert(c);
            set_pos(list, i, g.at(c));
        }
        save(list);
    }

    // ---------- moving icons ----------

    /// Our own drag of desktop icons starts (`from` in list coordinates).
    pub fn drag_start(from: (i32, i32)) { DRAG_FROM.set(Some(from)); DROP_AT.set(None); }
    pub fn dragging() -> bool { DRAG_FROM.get().is_some() }
    /// Called by the drop target: the drag ended on the desktop itself at screen point `at`.
    pub fn dropped_here(at: (i32, i32)) { DROP_AT.set(Some(at)); }

    /// After the drag: on a folder icon (not one of the dragged) the selection moves into that folder; anywhere else the
    /// icons move by the drag distance, snapped to the grid, each to the nearest free cell. true = handled here.
    pub fn drag_end(h: HWND, list: HWND) -> bool {
        let Some(from) = DRAG_FROM.take() else { return false };
        // normally our drop target recorded where it ended; over a live wallpaper OLE never reaches it (the wallpaper window of
        // another process is under the pointer), so then: the pointer is over our desktop window = the drop was on the desktop
        let at = DROP_AT.take().or_else(|| unsafe {
            let mut c = POINT { x: 0, y: 0 };
            GetCursorPos(&mut c);
            (GetAncestor(WindowFromPoint(c), GA_ROOT) == h).then_some((c.x, c.y))
        });
        let Some(at) = at else { return false };
        let mut pt = POINT { x: at.0, y: at.1 };
        unsafe { ScreenToClient(list, &mut pt) };
        let (_, dir, paths) = selected();
        let mut sel = vec![];
        let mut i = -1isize;
        loop {
            i = put(list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
            if i < 0 { break; }
            sel.push(i as usize);
        }
        // onto a folder icon
        let mut ht: LVHITTESTINFO = unsafe { std::mem::zeroed() };
        ht.pt = pt;
        let tgt = put(list, LVM_HITTEST, 0, &mut ht as *mut _ as isize);
        if tgt >= 0 && !sel.contains(&(tgt as usize)) {
            let target = ST.with_borrow(|st| st.rows.get(tgt as usize).filter(|r| r.dir).map(|r| st.dir.join(&r.name)));
            if let Some(t) = target {
                let _ = sh::fileop(h as _, sh::Op::Move, &paths, &t);
                let _ = dir;
                unsafe { PostMessageW(h, WM_APP, 0, 0) }; // reload: the moved icons are gone from here
                return true;
            }
        }
        let g = Grid::of(list);
        let n = put(list, LVM_GETITEMCOUNT, 0, 0).max(0) as usize;
        let mut taken: HashSet<(i32, i32)> = (0..n).filter(|i| !sel.contains(i)).filter_map(|i| g.cell(pos(list, i))).collect();
        let (dx, dy) = (pt.x - from.0, pt.y - from.1);
        for &i in &sel {
            let (x, y) = pos(list, i);
            let want = g.cell((x + dx, y + dy)).unwrap_or_else(|| {
                // dragged off the grid: the nearest edge cell
                (((x + dx) / g.cx).clamp(0, g.cols - 1), ((y + dy) / g.cy).clamp(0, g.rows - 1))
            });
            let c = if taken.contains(&want) { g.free(&taken, Some(want)) } else { want };
            taken.insert(c);
            set_pos(list, i, g.at(c));
        }
        save(list);
        true
    }

    // ---------- Wallpaper Engine (and other live wallpapers) ----------
    //
    // They look for the "Progman" window (that is our desktop's class), send it 0x052C and put their window into the
    // "WorkerW" that appears: Windows 11 24H2 has it as a child of Progman, under the icons. Ours is the same, and while it
    // holds a wallpaper the icon list turns see-through (colour key) so the wallpaper shows between the icons.

    pub unsafe fn spawn_workerw(h: HWND) {
        if WORKERW.get() != 0 && IsWindow(WORKERW.get() as HWND) != 0 { return; }
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(h, &mut r);
        let ww = CreateWindowExW(0, w("WorkerW").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE, 0, 0, r.right, r.bottom, h, 0 as _, GetModuleHandleW(ptr::null()), ptr::null());
        SetWindowPos(ww, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE); // under the icon list
        WORKERW.set(ww as isize);
    }

    /// Every couple of seconds: a wallpaper window inside WorkerW = live mode on, none = back to the picture.
    pub unsafe fn check_live(list: HWND) {
        let ww = WORKERW.get() as HWND;
        let live = !ww.is_null() && IsWindow(ww) != 0 && !GetWindow(ww, GW_CHILD).is_null();
        if live == LIVE.replace(live) { return; }
        // the list stays "transparent" (its background is what the desktop paints: the key colour while live, see erase()),
        // so it keeps drawing the label shadows; the colour key then cuts the background out
        let ex = GetWindowLongPtrW(list, GWL_EXSTYLE);
        if live {
            SetWindowLongPtrW(list, GWL_EXSTYLE, ex | WS_EX_LAYERED as isize);
            SetLayeredWindowAttributes(list, KEY, 0, LWA_COLORKEY);
        } else {
            SetWindowLongPtrW(list, GWL_EXSTYLE, ex & !(WS_EX_LAYERED as isize));
        }
        InvalidateRect(list, ptr::null(), 1);
    }

    /// The desktop's background: the key colour while a live wallpaper shows through, else the picture. false = none to paint.
    pub unsafe fn erase(dc: HDC, h: HWND) -> bool {
        let (bmp, cw, ch) = WALL.get();
        if LIVE.get() {
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(h, &mut r);
            let b = CreateSolidBrush(KEY);
            FillRect(dc, &r, b);
            DeleteObject(b);
        } else if bmp != 0 {
            wall::paint(dc, bmp, cw, ch);
        } else {
            return false;
        }
        true
    }

    /// WorkerW without a wallpaper in it shows the picture wallpaper, like the desktop behind it.
    pub unsafe extern "system" fn workerw_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if m == WM_ERASEBKGND {
            let (bmp, cw, ch) = WALL.get();
            if bmp != 0 { wall::paint(wp as _, bmp, cw, ch); } else { let mut r: RECT = std::mem::zeroed(); GetClientRect(h, &mut r); FillRect(wp as _, &r, GetStockObject(BLACK_BRUSH) as _); }
            return 1;
        }
        DefWindowProcW(h, m, wp, lp)
    }

    #[cfg(test)]
    mod tests {
        use super::Grid;
        use std::collections::HashSet;

        #[test]
        fn grid_cells() {
            let g = Grid { cx: 100, cy: 90, cols: 5, rows: 4 };
            assert_eq!(g.cell((0, 0)), Some((0, 0)));
            assert_eq!(g.cell((149, 44)), Some((1, 0))); // rounds to the nearest cell
            assert_eq!(g.cell((900, 0)), None); // off the grid
            let mut taken: HashSet<_> = [(0, 0), (0, 1)].into();
            assert_eq!(g.free(&taken, None), (0, 2)); // down the first column first
            taken.insert((2, 2));
            assert_eq!(g.free(&taken, Some((2, 2))), (1, 2)); // nearest free to the wish (ties: lowest)
        }
    }
}

/// Trace of the icon drags (%TEMP%\rx-drag.log), to find out where one stops working. Cheap: a few lines per drag.
fn drag_log(s: &str) {
    use std::io::Write;
    let p = std::env::temp_dir().join("rx-drag.log");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(p) { let _ = writeln!(f, "{s}"); }
}

/// Address bar subclass: Enter / Esc go to the main window, everything else is the normal edit control.
unsafe extern "system" fn edit_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if matches!(wp, 13 | 27) && matches!(m, WM_KEYDOWN | WM_CHAR) {
            if m == WM_KEYDOWN { PostMessageW(GetAncestor(h, GA_ROOT), WM_GO, wp, 0); }
            return 0; // WM_CHAR swallowed so there is no beep
        }
        CallWindowProcW(std::mem::transmute::<isize, WNDPROC>(EDIT_OLD.load(std::sync::atomic::Ordering::Relaxed)), h, m, wp, lp)
    }
}

unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if m == winrust_common::style::msg() {
            // the topbar switched the WinRust style: take it now (the desktop's list keeps its see-through colours)
            winrust_common::style::reload();
            if desk() { InvalidateRect(h, ptr::null(), 1); } else { ui::restyle(h); }
            return 0;
        }
        match m {
            WM_CREATE => {
                let inst = GetModuleHandleW(ptr::null());
                // desktop mode has no tab bar / address bar: the null handles are harmless to every SendMessage below
                let (mut tab, mut edit) = (0 as HWND, 0 as HWND);
                if !desk() {
                    ui::dark_title(h);
                    // the chrome (tabs, toolbar, address pill) is one painted window; the address box is a real edit inside its pill
                    tab = CreateWindowExW(0, w("RxChrome").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN, 0, 0, 0, 0, h, 0 as _, inst, ptr::null());
                    ui::nav_init(CreateWindowExW(0, w("RxNav").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE, 0, 0, 0, 0, h, 0 as _, inst, ptr::null()));
                    edit = CreateWindowExW(0, w("EDIT").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32, 0, 0, 0, 0, tab, 0 as _, inst, ptr::null());
                    put(edit, WM_SETFONT, ui::font(14, 400, ui::ui_face()) as usize, 1);
                    EDIT_OLD.store(SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize), std::sync::atomic::Ordering::Relaxed);
                    // what was remembered, before the first listing (sorting and hidden files depend on it)
                    let p = prefs::load();
                    SHOW_HIDDEN.store(p.hidden, Relaxed);
                    FOLDERS_FIRST.store(p.folders_first, Relaxed);
                    SORT_COL.store(p.sort_col, Relaxed);
                    SORT_ASC.store(p.sort_asc, Relaxed);
                    FILT.with_borrow_mut(|f| f.sub = p.subfolders);
                    PREFS.set(p);
                    RECENT.set(prefs::read_paths("recent.txt"));
                    MARKS.set(prefs::read_paths("bookmarks.txt"));
                    // floating over the list's bottom corners: the filter (its box inside), the status; the zoom slider
                    let float = WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN;
                    let fbar = CreateWindowExW(0, w("RxFilter").as_ptr(), ptr::null(), float | WS_VISIBLE, 0, 0, 0, 0, h, 0 as _, inst, ptr::null());
                    let sbar = CreateWindowExW(0, w("RxStatus").as_ptr(), ptr::null(), float | WS_VISIBLE, 0, 0, 0, 0, h, 0 as _, inst, ptr::null());
                    let pop = CreateWindowExW(0, w("RxZoomPop").as_ptr(), ptr::null(), float, 0, 0, 0, 0, h, 0 as _, inst, ptr::null());
                    ui::overlays_set(fbar, sbar, pop);
                    let fe = CreateWindowExW(0, w("EDIT").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32, 0, 0, 0, 0, fbar, 0 as _, inst, ptr::null());
                    put(fe, WM_SETFONT, ui::font(12, 400, ui::ui_face()) as usize, 1);
                    FILT_OLD.store(SetWindowLongPtrW(fe, GWLP_WNDPROC, filter_proc as *const () as isize), Relaxed);
                    FILT.with_borrow_mut(|f| f.edit = fe as isize);
                    filter_cue();
                    ui::nav_rebuild();
                }
                let view = if desk() { LVS_ICON } else { LVS_REPORT };
                let list = CreateWindowExW(0, w("SysListView32").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | view | if desk() { 0 } else { LVS_OWNERDATA } | LVS_EDITLABELS | LVS_SHOWSELALWAYS | LVS_SHAREIMAGELISTS, 0, 0, 0, 0, h, 0 as _, inst, ptr::null());
                put(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize);
                let mut fi: SHFILEINFOW = std::mem::zeroed();
                let il = SHGetFileInfoW(w("C:\\").as_ptr(), 0, &mut fi, size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_SMALLICON);
                put(list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, il as isize);
                if desk() {
                    // system large icons; indexes are shared with the small list, so icon_of() works unchanged
                    let big = SHGetFileInfoW(w("C:\\").as_ptr(), 0, &mut fi, size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_LARGEICON);
                    put(list, LVM_SETIMAGELIST, LVSIL_NORMAL as usize, big as isize);
                    put(list, LVM_SETICONSPACING, 0, ((ui::sc(96) as isize) << 16) | ui::sc(88) as isize);
                    // transparent background (the desktop paints it) and shadowed labels, readable on light wallpapers too
                    let ex = LVS_EX_TRANSPARENTBKGND | 0x0080_0000; // LVS_EX_TRANSPARENTSHADOWTEXT
                    put(list, LVM_SETEXTENDEDLISTVIEWSTYLE, ex as usize, ex as isize);
                    for msg in [LVM_SETBKCOLOR, LVM_SETTEXTBKCOLOR] { put(list, msg, 0, CLR_NONE); }
                    put(list, LVM_SETTEXTCOLOR, 0, 0x00FF_FFFF);
                    let (cw, ch) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
                    WALL.set((wall::load(cw, ch), cw, ch));
                    // grey (not ClearType) smoothing: on a live wallpaper the labels sit on a colour key, where ClearType leaves coloured fringes
                    put(list, WM_SETFONT, windows_sys::Win32::Graphics::Gdi::CreateFontW(-ui::sc(13), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 4, 0, w(ui::ui_face()).as_ptr()) as usize, 1);
                    // like explorer's desktop: wallpaper apps look for a SHELLDLL_DefView under Progman
                    CreateWindowExW(0, w("SHELLDLL_DefView").as_ptr(), ptr::null(), WS_CHILD, 0, 0, 0, 0, h, 0 as _, inst, ptr::null());
                    SetTimer(h, 7, 2000, None); // live wallpaper on / off (desk::check_live)
                } else {
                    thumbs::start(h as isize);
                    sizes::HDD_MODE.store(prefs_now().hdd_mode, Relaxed);
                    sizes::start(h as isize);
                    // the same columns, in the same order, as Explorer's details view
                    // the same columns as Explorer's details view, plus what is inside each folder (sizes.rs), like File Pilot
                    for (i, (t, wd)) in [("Name", 360), ("Date modified", 150), ("Type", 120), ("Folders", 76), ("Files", 76), ("Size", 96)].into_iter().enumerate() { col(list, i as i32, t, ui::sc(wd), false); }
                    ST.with_borrow_mut(|st| st.list = list);
                    ui::style_list(list);
                    set_zoom(h, prefs_now().zoom, false); // the size last used
                }
                // the tabs to start with: the folder asked for, else (setting on) the tabs open when the last window closed, else Home
                let home = env::var_os("USERPROFILE").map_or_else(|| PathBuf::from("C:\\"), PathBuf::from);
                let (tabs, active) = if desk() {
                    (vec![sh::desktop_dirs().into_iter().next().unwrap_or_default()], 0)
                } else if let Some(a) = env::args().nth(1) {
                    (vec![PathBuf::from(a)], 0)
                } else {
                    SAVES_SESSION.set(true);
                    let (t, a) = if prefs_now().restore_tabs { prefs::restore(&prefs::read("session.txt"), |p| p.is_dir()) } else { (vec![], 0) };
                    if t.is_empty() { (vec![home], 0) } else { (t, a) }
                };
                let start = tabs[active].clone();
                ST.with_borrow_mut(|st| { st.list = list; st.tab = tab; st.edit = edit; st.tabs = tabs; st.active = active; });
                if !desk() { ui::place_edit(tab); }
                let _ = sh::register_drop(list as _, h as isize);
                if !desk() { glist::create(h); }
                if !desk() { let _ = sh::register_tab_drop(tab as _, h as isize); let _ = sh::register_tab_drop(h as _, h as isize); }
                // live wallpaper: the list is see-through (colour key), so over empty desktop OLE finds the wallpaper window under it,
                // and walks up from there to the desktop window: it must accept the drop too
                if desk() { let _ = sh::register_drop(h as _, h as isize); }
                load(h, start);
                0
            }
            // the bar came, went or changed size (it sets the work area), or the screen changed: the icon list follows
            WM_SETTINGCHANGE | WM_DISPLAYCHANGE if desk() => {
                if m == WM_DISPLAYCHANGE {
                    SetWindowPos(h, HWND_BOTTOM, 0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN), SWP_NOACTIVATE); // WM_SIZE lays out
                } else if wp == SPI_SETWORKAREA as usize {
                    let (list, mut r) = (ST.with_borrow(|st| st.list), std::mem::zeroed::<RECT>());
                    GetClientRect(h, &mut r);
                    MoveWindow(list, 0, desk_top(), r.right, (r.bottom - desk_top() - desk_bottom()).max(0), 1);
                }
                0
            }
            WM_CTLCOLOREDIT if glist::edit_colors(wp as _, lp as _).is_some() => glist::edit_colors(wp as _, lp as _).unwrap(), // the rename box
            WM_ACTIVATEAPP if wp == 0 => { ui::zoom_pop_hide(); DefWindowProcW(h, m, wp, lp) } // another app came forward
            WM_SIZE => {
                let list = ST.with_borrow(|st| st.list);
                let (cw, ch) = ((lp & 0xFFFF) as i32, ((lp >> 16) & 0xFFFF) as i32);
                if desk() {
                    MoveWindow(list, 0, desk_top(), cw, (ch - desk_top() - desk_bottom()).max(0), 1);
                } else {
                    ui::layout_children(cw, ch);
                    InvalidateRect(h, ptr::null(), 0);
                }
                0
            }
            ui::WM_UI => {
                match wp {
                    1 => { let n = ST.with_borrow(|st| st.tabs.len()); if (lp as usize) < n { tab_select(h, lp as usize); } }
                    2 => tab_close_at(h, lp as usize),
                    3 => tab_new(),
                    4 => nav(h, true),
                    5 => nav(h, false),
                    6 => { if let Some(p) = cur_dir().parent() { load(h, p.to_path_buf()) } }
                    7 => reload(h),
                    8 => toggle_view(h),
                    9 => filter_focus(None),
                    10 => opts::open(h),
                    11 => toggle_mark(h, cur_dir()),
                    // a place in the side pane: a folder opens here, a recent file with its program
                    20 => { if let Some((p, _)) = ui::nav_item(lp as usize) { if p.as_os_str() == ui::RECYCLE_BIN { open_shell_place(&p) } else if p.is_dir() { load(h, p) } else { open_file(&p) } } }
                    21 => { if let Some(p) = ui::crumb_path(lp as usize) { load(h, p); } } // a part of the path in the address pill
                    22 => nav_menu(h, lp as usize),
                    23 => { if let Some((p, _)) = ui::nav_item(lp as usize) { if p.as_os_str() == ui::RECYCLE_BIN { open_shell_place(&p) } else { open_in_new_tab(h, p); } } } // middle click
                    30 => {
                        // the side pane's button: hides or shows the pane, the tabs move along
                        set_prefs(h, |p| p.side_pane = !p.side_pane, false);
                        let mut r: RECT = std::mem::zeroed();
                        GetClientRect(h, &mut r);
                        ui::layout_children(r.right, r.bottom);
                        InvalidateRect(h, ptr::null(), 0);
                    }
                    24 => set_prefs(h, |p| { let s = lp as u8; if let Some(i) = p.collapsed.iter().position(|&c| c == s) { p.collapsed.remove(i); } else { p.collapsed.push(s); } }, false),
                    25 => zoom_by(h, lp as i32), // Ctrl+wheel over the list: lp = steps (+ bigger)
                    26 => tree_toggle(h, lp as usize), // the arrow of a folder in the tree
                    27 => toggle_sub(h), // the "Subfolders" glyph in the filter bar
                    28 => set_zoom(h, lp as usize, true), // a step picked on the zoom slider
                    29 => bg_menu(h), // right click on the empty rows at the end
                    _ => {}
                }
                0
            }
            WM_LISTED => {
                let p = PENDING.with_borrow_mut(|p| if p.as_ref().is_some_and(|p| p.seq == wp as u32) { p.take() } else { None });
                if let Some(p) = p {
                    match p.rx.try_recv() { Ok(Some(rows)) => commit(h, p.dir, rows, p.record), _ => ui::status_dirty(h) }
                }
                0
            }
            WM_CRAWL => {
                // more subfolder hits: shown with the selection kept (the user may already be moving through them)
                let current = CRAWL.with_borrow(|c| c.as_ref().is_some_and(|c| prefs::same(&c.root, &cur_dir())));
                if current && filtering() && FILT.with_borrow(|f| f.sub) && !ui::searching() { let (keep, top) = kept(); show(h, &keep, top); }
                0
            }
            WM_CHANGED => {
                // the folder changed on disk: shown again, unless a rename is being typed or other rows are up
                let list = ST.with_borrow(|st| st.list);
                let editing = put(list, LVM_GETEDITCONTROL, 0, 0) != 0;
                if wp as u32 == WATCH_GEN.load(Relaxed) && !editing && !ui::searching() && PENDING.with_borrow(|p| p.is_none()) { reload(h); }
                0
            }
            WM_FILTKEY => {
                let list = ST.with_borrow(|st| st.list);
                match wp {
                    0x0D => {
                        // Enter: open the selected row (the first hit unless the arrows moved on)
                        let i = put(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
                        if i >= 0 { SetFocus(list); open_row(h, i as usize); }
                    }
                    0x1B => { if filtering() { filter_clear(); show(h, &[], 0); } SetFocus(list); }
                    0x45 => toggle_sub(h),
                    0x46 => filter_focus(None),
                    0x44 => toggle_mark(h, cur_dir()),
                    _ => {}
                }
                0
            }
            WM_COMMAND if lp != 0 && lp as HWND == filter_edit() => {
                match (wp >> 16) as u32 {
                    0x0300 => filter_changed(h), // EN_CHANGE
                    0x0100 => { // EN_SETFOCUS: with "Subfolders" on, the crawl starts before the first key
                        if FILT.with_borrow(|f| f.sub) { crawl_for(h, &cur_dir()); }
                        ui::status_dirty(h);
                    }
                    0x0200 => ui::status_dirty(h), // EN_KILLFOCUS: the box's outline
                    _ => {}
                }
                0
            }
            // no title bar of the system's: the client starts at the window's top (the tabs are the title bar). The sides
            // and the bottom keep their resizing frame; maximized, the part Windows pushes off screen is given back
            WM_NCCALCSIZE if !desk() && wp != 0 => {
                let p = &mut *(lp as *mut NCCALCSIZE_PARAMS);
                let top = p.rgrc[0].top;
                let r = DefWindowProcW(h, m, wp, lp);
                p.rgrc[0].top = top;
                if IsZoomed(h) != 0 { p.rgrc[0].top += GetSystemMetrics(SM_CYFRAME) + GetSystemMetrics(SM_CXPADDEDBORDER); }
                r
            }
            // the empty parts of the tab row drag the window (and double-click maximizes); its top edge resizes
            WM_NCHITTEST if !desk() => {
                let r = DefWindowProcW(h, m, wp, lp);
                if r != HTCLIENT as isize { return r; }
                let mut p = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
                windows_sys::Win32::Graphics::Gdi::ScreenToClient(h, &mut p);
                let mut cr: RECT = std::mem::zeroed();
                GetClientRect(h, &mut cr);
                if IsZoomed(h) == 0 && p.y < ui::sc(5) {
                    return if p.x < ui::sc(12) { HTTOPLEFT } else if p.x > cr.right - ui::sc(12) { HTTOPRIGHT } else { HTTOP } as isize;
                }
                if p.y < ui::sc(ui::TAB_H) { HTCAPTION as isize } else { r }
            }
            WM_DESTROY if !desk() => {
                save_session();
                CRAWL.with_borrow(|c| if let Some(c) = c { c.stop() });
                WATCH_GEN.fetch_add(1, Relaxed);
                opts::close();
                PostQuitMessage(0);
                0
            }
            WM_ERASEBKGND if desk() && desk::erase(wp as _, h) => 1,
            WM_WINDOWPOSCHANGING if desk() => { // the desktop never rises above other windows
                let wp = &mut *(lp as *mut WINDOWPOS);
                wp.hwndInsertAfter = HWND_BOTTOM;
                wp.flags |= SWP_NOACTIVATE;
                0
            }
            WM_INITMENUPOPUP if sh::menu_msg(m, wp, lp) => 0,
            WM_DRAWITEM | WM_MEASUREITEM if sh::menu_msg(m, wp, lp) => 1,
            WM_NOTIFY => {
                let nm = &*(lp as *const NMHDR);
                let list = ST.with_borrow(|st| st.list);
                match nm.code {
                    NM_CUSTOMDRAW if nm.hwndFrom == list && !desk() => return ui::list_draw(lp), // the rows are painted by hand
                    LVN_ITEMCHANGED | LVN_ODSTATECHANGED => {
                        glist::invalidate();
                        // the empty rows at the end are only room: a selection that reaches them goes back to the last row
                        let n = ST.with_borrow(|st| st.rows.len());
                        let (from, to) = if nm.code == LVN_ITEMCHANGED { let i = (*(lp as *const NMLISTVIEW)).iItem; (i, i) } else { let o = &*(lp as *const NMLVODSTATECHANGE); (o.iFrom, o.iTo) };
                        if !desk() && n > 0 && to >= n as i32 {
                            let mut it: LVITEMW = std::mem::zeroed();
                            it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
                            for k in (from.max(n as i32))..=to { put(list, LVM_SETITEMSTATE, k as usize, &it as *const _ as isize); }
                            let f = put(list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize);
                            if f < 0 || f >= n as isize { it.state = LVIS_SELECTED | LVIS_FOCUSED; put(list, LVM_SETITEMSTATE, n - 1, &it as *const _ as isize); }
                        }
                        ui::status_dirty(h); // selection count in the status line
                    }
                    LVN_GETDISPINFOW => {
                        let it = &mut (*(lp as *mut NMLVDISPINFOW)).item;
                        // a request that arrives while ST is borrowed is skipped; the list repaints later
                        ST.with(|c| {
                            let Ok(mut st) = c.try_borrow_mut() else { return };
                            let State { dir, rows, thumbs, .. } = &mut *st;
                            let thumbs = *thumbs;
                            let Some(i) = usize::try_from(it.iItem).ok() else { return };
                            let Some(r) = rows.get_mut(i) else { return };
                            if it.mask & LVIF_TEXT != 0 && it.cchTextMax > 0 {
                                let s = match it.iSubItem { 0 if !desk() => r.name.rsplit('\\').next().unwrap_or(&r.name).to_string(), 0 => shown_name(&r.name, desk()), 1 => date_text(r.time), 2 => type_text(&r.name, r.dir), _ if r.dir => String::new(), _ => size_text(r.size) };
                                let u: Vec<u16> = s.encode_utf16().take(it.cchTextMax as usize - 1).collect();
                                ptr::copy_nonoverlapping(u.as_ptr(), it.pszText, u.len());
                                *it.pszText.add(u.len()) = 0;
                            }
                            if it.mask & LVIF_IMAGE != 0 && thumbs {
                                it.iImage = 0; // a cell of the picture size; the picture is drawn by ui::list_draw
                            } else if it.mask & LVIF_IMAGE != 0 {
                                if r.icon < 0 { r.icon = icon_of(&dir.join(&r.name), r.dir); }
                                it.iImage = r.icon;
                            }
                        });
                    }
                    LVN_ITEMACTIVATE => {
                        if let Ok(i) = usize::try_from((*(lp as *const NMITEMACTIVATE)).iItem) { open_row(h, i); }
                    }
                    NM_RCLICK if nm.hwndFrom == list => {
                        let (_, _, paths) = selected();
                        if !paths.is_empty() { menu_choice(h, sh::show(h, &paths).unwrap_or(sh::Choice::Done), &paths); } else { bg_menu(h); }
                    }
                    LVN_COLUMNCLICK => sort_click(h, (*(lp as *const NMLISTVIEW)).iSubItem as usize),
                    LVN_BEGINDRAG => {
                        let (_, _, paths) = selected();
                        drag_log(&format!("BEGINDRAG desk={} selected={}", desk(), paths.len()));
                        if !paths.is_empty() {
                            if desk() { let p = (*(lp as *const NMLISTVIEW)).ptAction; desk::drag_start((p.x, p.y)); }
                            let r = sh::drag(h, &paths);
                            drag_log(&format!("drag finished: {r:?}"));
                            // dropped on the desktop itself: the icons were moved (or went into a folder icon), no reload needed
                            let handled = desk() && desk::drag_end(h, list);
                            drag_log(&format!("moved on the desktop: {handled}"));
                            if !handled && desk() { reload(h); } // the folder window follows changes by itself (watch)
                        }
                    }
                    LVN_KEYDOWN => {
                        let vk = (*(lp as *const NMLVKEYDOWN)).wVKey as u32;
                        let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                        if desk() && matches!(vk, 0x08 | 0x46 | 0x4D | 0x54 | 0x57 | 0x09 | 0x4C | 0x73) { return 0; } // browsing keys make no sense on the desktop
                        let (list, dir, paths) = selected();
                        let shift = GetKeyState(VK_SHIFT as i32) < 0;
                        if !desk() {
                            match vk {
                                0x46 if ctrl && shift => { search_focus(); return 0; } // Ctrl+Shift+F: the whole PC ("? " in the address bar)
                                0x46 if ctrl => { filter_focus(None); return 0; } // Ctrl+F: the filter box
                                0x45 if ctrl => { toggle_sub(h); return 0; } // Ctrl+E: the filter searches subfolders too
                                0x44 if ctrl => { toggle_mark(h, dir.clone()); return 0; } // Ctrl+D: bookmark this folder
                                0xBC if ctrl => { opts::open(h); return 0; } // Ctrl+,: settings
                                0xBB | 0x6B if ctrl => { zoom_by(h, 1); return 0; } // Ctrl+Plus: bigger
                                0xBD | 0x6D if ctrl => { zoom_by(h, -1); return 0; } // Ctrl+Minus: smaller
                                0x30 | 0x60 if ctrl => { set_zoom(h, ZOOM_DETAILS, true); return 0; } // Ctrl+0: the usual details
                                0x1B if filtering() => { filter_clear(); show(h, &[], 0); return 0; } // Esc: the filter goes
                                0x48 if ctrl => { set_prefs(h, |p| p.hidden = !p.hidden, true); return 0; } // Ctrl+H: hidden files
                                // tree: Right opens the folder, Left closes it or goes to the folder it is in
                                0x27 | 0x25 if ST.with_borrow(|st| st.view) == View::Tree => {
                                    let i = put(list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize);
                                    let row = usize::try_from(i).ok().and_then(|i| ST.with_borrow(|st| st.rows.get(i).map(|r| (r.dir, r.open, r.name.clone()))));
                                    if let Some((is_dir, open, name)) = row {
                                        if vk == 0x27 && is_dir && !open { tree_toggle(h, i as usize); }
                                        else if vk == 0x25 && open { tree_toggle(h, i as usize); }
                                        else if vk == 0x25 {
                                            if let Some((parent, _)) = name.rsplit_once('\\') {
                                                let p = ST.with_borrow(|st| st.rows.iter().position(|r| r.name == parent));
                                                if let Some(p) = p {
                                                    put(list, LVM_SETITEMSTATE, usize::MAX, &LVITEMW { stateMask: LVIS_SELECTED, state: 0, ..std::mem::zeroed() } as *const _ as isize);
                                                    put(list, LVM_SETITEMSTATE, p, &LVITEMW { stateMask: LVIS_SELECTED | LVIS_FOCUSED, state: LVIS_SELECTED | LVIS_FOCUSED, ..std::mem::zeroed() } as *const _ as isize);
                                                    put(list, LVM_ENSUREVISIBLE, p, 0);
                                                }
                                            }
                                        }
                                    }
                                    return 0;
                                }
                                _ => {}
                            }
                        }
                        match vk {
                            0x08 => if let Some(p) = dir.parent() { load(h, p.to_path_buf()) },
                            0x2E if !paths.is_empty() => { let _ = sh::fileop(h, if shift { sh::Op::Purge } else { sh::Op::Delete }, &paths, &dir); } // Shift+Del: for good
                            0x5A if ctrl => { if !sh::undo(h) { return 0; } } // Ctrl+Z: the last copy / move / delete / rename / new item back
                            0x41 if ctrl => { select_all(list); return 0; } // Ctrl+A
                            0x1B => { // Esc: nothing selected
                                put(list, LVM_SETITEMSTATE, usize::MAX, &LVITEMW { stateMask: LVIS_SELECTED, ..std::mem::zeroed() } as *const _ as isize);
                                return 0;
                            }
                            0x43 if ctrl && shift && !paths.is_empty() => { copy_paths(h, &paths); return 0; } // Ctrl+Shift+C: the paths as text
                            0x52 if ctrl => {} // Ctrl+R: reload, like F5
                            // Space on a picture: a quick look in photoview (pictures only: videos and text are not previewed yet)
                            0x20 if !desk() => if let (Some(p), Some(exe)) = (paths.first().filter(|p| is_pic(p)), tool("photoview")) { let _ = std::process::Command::new(exe).arg(p).spawn(); },
                            0x71 => { rename_selected(); return 0; }
                            0x73 | 0x4C if vk == 0x73 || ctrl => { // F4 / Ctrl+L: address bar
                                let edit = ST.with_borrow(|st| st.edit);
                                SetFocus(edit);
                                put(edit, EM_SETSEL, 0, -1);
                                return 0;
                            }
                            0x5D if !paths.is_empty() => { menu_choice(h, sh::show(h, &paths).unwrap_or(sh::Choice::Done), &paths); return 0; }
                            0x43 | 0x58 if ctrl && !paths.is_empty() => { let _ = sh::clip_set(h, &paths, vk == 0x58); return 0; }
                            0x56 if ctrl => paste(h, &dir),
                            0x4E if ctrl => new_entry(&dir, "New folder", true),
                            0x48 if ctrl => { SHOW_HIDDEN.fetch_xor(true, Relaxed); } // Ctrl+H on the desktop
                            0x74 => {} // F5: just the reload below
                            0x4D if ctrl => { toggle_view(h); return 0; } // Ctrl+M: details <-> thumbnails
                            0x54 if ctrl => tab_new(),
                            0x57 if ctrl => { tab_close(h); return 0; }
                            0x09 if ctrl => { // Ctrl+Tab / Ctrl+Shift+Tab: next / previous tab
                                let next = ST.with_borrow(|st| if shift { (st.active + st.tabs.len() - 1) % st.tabs.len() } else { (st.active + 1) % st.tabs.len() });
                                tab_select(h, next);
                                return 0;
                            }
                            0x31..=0x39 if ctrl && !desk() => { // Ctrl+1..9: that tab (9: the last one)
                                let n = ST.with_borrow(|st| st.tabs.len());
                                let i = if vk == 0x39 { n - 1 } else { (vk - 0x31) as usize };
                                if i < n { tab_select(h, i); }
                                return 0;
                            }
                            _ => return 0,
                        }
                        reload(h);
                    }
                    LVN_BEGINLABELEDITW => {
                        // the box over the drawn name (the list would put it over the icon)
                        let (list, i) = (ST.with_borrow(|st| st.list), (*(lp as *const NMLVDISPINFOW)).item.iItem);
                        if let Ok(i) = usize::try_from(i) { ui::label_edit_fit(list, put(list, LVM_GETEDITCONTROL, 0, 0) as HWND, i); }
                        return 0;
                    }
                    LVN_ENDLABELEDITW => {
                        let it = &(*(lp as *const NMLVDISPINFOW)).item;
                        if !it.pszText.is_null() { if let Ok(i) = usize::try_from(it.iItem) { rename_row(h, i, &wstr(it.pszText)); } }
                    }
                    _ => {}
                }
                0
            }
            // the Back / Forward keys of some keyboards (the mouse buttons are taken in the message loop)
            WM_APPCOMMAND if !desk() && matches!((lp >> 16) & 0x0FFF, 1 | 2) => { nav(h, (lp >> 16) & 0x0FFF == 1); 1 }
            WM_SETFOCUS => { SetFocus(ST.with_borrow(|st| st.list)); 0 }
            WM_APP => { reload(h); 0 }
            WM_NAV => {
                match wp {
                    1 => nav(h, true),
                    2 => nav(h, false),
                    3 => { let (_, dir, sel) = selected(); properties(&sel.first().cloned().unwrap_or(dir)); }
                    _ => if let Some(p) = cur_dir().parent() { load(h, p.to_path_buf()) },
                }
                0
            }
            sizes::WM_SIZES => {
                // wp = 1: a drive's totals were dropped (more changes at once than its watch could hold): the folder
                // shown counts again instead of losing its numbers
                if wp == 1 && PREFS.with_borrow(|p| p.folder_sizes) {
                    let d = cur_dir();
                    sizes::more(ST.with_borrow(|st| st.all.iter().filter(|r| r.dir).map(|r| d.join(&r.name)).collect()));
                }
                InvalidateRect(ST.with_borrow(|st| st.list), ptr::null(), 0);
                glist::invalidate();
                0
            }
            thumbs::WM_PIC => {
                if thumbs::store(wp) { InvalidateRect(ST.with_borrow(|st| st.list), ptr::null(), 0); glist::invalidate(); }
                0
            }
            WM_GO => {
                let (edit, list) = ST.with_borrow(|st| (st.edit, st.list));
                if wp == 13 {
                    let mut buf = vec![0u16; GetWindowTextLengthW(edit) as usize + 1];
                    let n = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32) as usize;
                    let p = PathBuf::from(String::from_utf16_lossy(&buf[..n]).trim().trim_matches('"'));
                    let text = String::from_utf16_lossy(&buf[..n]);
                    if let Some(q) = text.trim().strip_prefix('?') {
                        if !q.trim().is_empty() { search(h, q.trim()); SetFocus(list); return 0; } // keep the query in the bar
                    } else if p.is_dir() { load(h, p); }
                }
                SetWindowTextW(edit, w(&cur_dir().to_string_lossy()).as_ptr()); // also undoes a bad path / Esc
                SetFocus(list);
                0
            }
            desk::SPAWN_WORKERW if desk() => { desk::spawn_workerw(h); 0 }
            WM_TIMER if !desk() && wp == ZOOM_TIMER => { zoom_tick(h); 0 }
            WM_TIMER if !desk() && wp == PREFS_TIMER => { KillTimer(h, PREFS_TIMER); prefs::save(&prefs_now()); 0 }
            WM_TIMER if desk() && wp == 7 => { desk::check_live(ST.with_borrow(|st| st.list)); 0 }
            WM_DESTROY => { PostQuitMessage(0); 0 }
            _ => DefWindowProcW(h, m, wp, lp),
        }
    }
}

fn main() {
    // a panic ends the window (it happens inside a window procedure); what it was is kept in %TEMP%\rx-panic.log
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(env::temp_dir().join("rx-panic.log")) {
            let _ = writeln!(f, "{info}
{}", std::backtrace::Backtrace::force_capture());
        }
    }));
    sh::init();
    unsafe {
        SetProcessDPIAware(); // sharp text at any scaling; every size below goes through ui::sc
        ui::init_scale();
        ui::gdiplus_start();
        ui::dark_mode();
        InitCommonControlsEx(&INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES });
        let inst = GetModuleHandleW(ptr::null());
        let cls = w("RustExplorer");
        let desktop = env::args().nth(1).as_deref() == Some("--desktop");
        // desktop without a wallpaper file shows black, not window white
        let bg = if desktop { GetStockObject(BLACK_BRUSH) as _ } else { windows_sys::Win32::Graphics::Gdi::CreateSolidBrush(ui::pal().bg) };
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(ui::proc), hInstance: inst, lpszClassName: w("RxChrome").as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), ..std::mem::zeroed() });
        RegisterClassW(&WNDCLASSW { style: CS_DBLCLKS, lpfnWndProc: Some(glist::proc), hInstance: inst, lpszClassName: w(glist::CLASS).as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), ..std::mem::zeroed() });
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(ui::nav_proc), hInstance: inst, lpszClassName: w("RxNav").as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), ..std::mem::zeroed() });
        // the menu's windows cast a shadow (CS_DROPSHADOW)
        RegisterClassW(&WNDCLASSW { style: 0x0002_0000, lpfnWndProc: Some(menu::proc), hInstance: inst, lpszClassName: w(menu::CLASS).as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), ..std::mem::zeroed() });
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(opts::proc), hInstance: inst, lpszClassName: w(opts::CLASS).as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), ..std::mem::zeroed() });
        for (name, f) in [("RxFilter", ui::filter_bar_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT), ("RxStatus", ui::status_bar_proc), ("RxZoomPop", ui::zoom_pop_proc)] {
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(f), hInstance: inst, lpszClassName: w(name).as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), ..std::mem::zeroed() });
        }
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), hInstance: inst, lpszClassName: cls.as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), hbrBackground: bg, ..std::mem::zeroed() });
        if desktop {
            DESKTOP.store(true, Relaxed);
            // the whole screen: it paints the wallpaper too; the icon list sits above the bar (desk_top / desk_bottom)
            // class "Progman" like explorer's desktop, so Wallpaper Engine & co. find it; title "rs-desktop" tells it apart
            // from explorer's ("Program Manager") for rs-shell
            let pm = w("Progman");
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), hInstance: inst, lpszClassName: pm.as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), hbrBackground: bg, ..std::mem::zeroed() });
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(desk::workerw_proc), hInstance: inst, lpszClassName: w("WorkerW").as_ptr(), ..std::mem::zeroed() });
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(DefWindowProcW), hInstance: inst, lpszClassName: w("SHELLDLL_DefView").as_ptr(), ..std::mem::zeroed() });
            let d = CreateWindowExW(WS_EX_TOOLWINDOW, pm.as_ptr(), w("rs-desktop").as_ptr(), WS_POPUP, 0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN), 0 as _, 0 as _, inst, ptr::null());
            SetWindowPos(d, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        } else {
            let main = CreateWindowExW(0, cls.as_ptr(), ptr::null(), WS_OVERLAPPEDWINDOW | WS_VISIBLE | WS_CLIPCHILDREN, CW_USEDEFAULT, CW_USEDEFAULT, ui::sc(1000), ui::sc(660), 0 as _, 0 as _, inst, ptr::null());
            // measure the frame again now that WM_NCCALCSIZE takes the title bar away
            SetWindowPos(main, ptr::null_mut(), 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, 0 as _, 0, 0) > 0 {
            // a frame of an animation: the input waiting first. Posted messages come before input, so with a frame
            // posted at every screen refresh the wheel's notches waited behind them and the list went on scrolling
            // after the wheel had stopped; now each frame draws with every notch turned so far.
            if msg.message == glist::WM_FRAME {
                let mut inp: MSG = std::mem::zeroed();
                for _ in 0..64 {
                    if PeekMessageW(&mut inp, 0 as _, 0, 0, PM_REMOVE | PM_QS_INPUT) == 0 { break; }
                    pump(&inp);
                }
            }
            pump(&msg);
        }
    }
}

/// One message of the loop (see main).
unsafe fn pump(msg: &MSG) {
    unsafe {
        {
            // Alt+Left / Right / Up / Enter arrive as WM_SYSKEYDOWN at the list, which never reports them: handle them here
            if msg.message == WM_SYSKEYDOWN && !desk() {
                let act = match msg.wParam { 0x25 => 1, 0x27 => 2, 0x0D => 3, 0x26 => 4, _ => 0 };
                if act != 0 { PostMessageW(GetAncestor(msg.hwnd, GA_ROOT), WM_NAV, act, 0); return; }
            }
            // a click anywhere else lets go of the address box (the side pane and the toolbar never take the focus
            // themselves, so it used to stay there); a click on the box itself puts it back right after
            if !desk() && matches!(msg.message, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_NCLBUTTONDOWN | WM_NCRBUTTONDOWN) {
                ui::zoom_pop_click(msg.hwnd);
                let (edit, list) = ST.with(|c| c.try_borrow().map(|s| (s.edit, s.list)).unwrap_or((0 as _, 0 as _)));
                if !edit.is_null() && GetFocus() == edit && msg.hwnd != edit { SetFocus(list); }
            }
            // the mouse's side buttons, over any part of the window (list, side pane, toolbar, title bar): back / forward.
            // Taken here, before the child under the pointer sees them, so they also make no WM_APPCOMMAND (one step each).
            if !desk() && matches!(msg.message, WM_XBUTTONDOWN | WM_XBUTTONUP | WM_NCXBUTTONDOWN | WM_NCXBUTTONUP) {
                if matches!(msg.message, WM_XBUTTONUP | WM_NCXBUTTONUP) {
                    let act = match (msg.wParam >> 16) & 0xFFFF { 1 => 1, 2 => 2, _ => 0 }; // XBUTTON1 = back, XBUTTON2 = forward
                    if act != 0 { PostMessageW(GetAncestor(msg.hwnd, GA_ROOT), WM_NAV, act, 0); }
                }
                return;
            }
            TranslateMessage(msg);
            DispatchMessageW(msg);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn r(name: &str, dir: bool, size: u64, time: u64) -> Row { Row { name: name.into(), dir, size, time, icon: -1, sub: (0, 0), depth: 0, open: false } }
    fn names(v: &[Row]) -> Vec<&str> { v.iter().map(|r| r.name.as_str()).collect() }

    #[test]
    fn desktop_rows_show_file_names_without_shortcut_extensions() {
        assert_eq!(shown_name(r"C:\Users\Public\Desktop\Steam.lnk", true), "Steam");
        assert_eq!(shown_name("Notes.txt", true), "Notes.txt");
        assert_eq!(shown_name(r"C:\x\Game.URL", true), "Game");
        assert_eq!(shown_name(r"C:\x\Steam.lnk", false), r"C:\x\Steam.lnk");
    }

    #[test]
    fn folders_first_then_by_the_chosen_column() {
        let mut v = vec![r("b.txt", false, 5, 1), r("A", true, 0, 2), r("a.txt", false, 9, 3)];
        sort_rows(&mut v, 0, true);
        assert_eq!(names(&v), ["A", "a.txt", "b.txt"]);
        sort_rows(&mut v, 3, false); // biggest first, the folder still on top
        assert_eq!(names(&v), ["A", "a.txt", "b.txt"]);
        sort_rows(&mut v, 1, true); // oldest first
        assert_eq!(names(&v), ["A", "b.txt", "a.txt"]);
    }

    #[test]
    fn search_hit_paths_sort_by_file_name() {
        let mut v = vec![r("C:\\z\\b.txt", false, 1, 1), r("D:\\a\\a.txt", false, 1, 1)];
        sort_rows(&mut v, 0, true);
        assert_eq!(names(&v), ["D:\\a\\a.txt", "C:\\z\\b.txt"]);
    }

    #[test]
    fn history_goes_back_and_forward_and_a_new_visit_drops_the_forward_trail() {
        let mut h = Hist::default();
        for p in ["a", "b", "c"] { h.visit(p.into()); }
        assert_eq!(h.back(), Some("b".into()));
        assert_eq!(h.back(), Some("a".into()));
        assert_eq!(h.back(), None);
        assert_eq!(h.forward(), Some("b".into()));
        h.visit("x".into()); // from b: c is no longer ahead
        assert_eq!(h.forward(), None);
        assert_eq!(h.back(), Some("b".into()));
        h.visit("x".into()); h.visit("x".into()); // same folder twice is one step
        assert_eq!(h.v.len(), 3);
    }

    #[test]
    fn type_and_size_read_like_explorers() {
        assert_eq!(type_text("x", true), "File folder");
        assert_eq!(type_text("a.txt", false), "TXT file");
        assert_eq!(type_text("C:\\d\\b.tar.gz", false), "GZ file");
        assert_eq!(type_text(".gitignore", false), "File"); // a dot file has no extension
        assert_eq!(type_text("Makefile", false), "File");
        assert_eq!(size_text(1), "1 KB");
        assert_eq!(size_text(2048), "2 KB");
        assert_eq!(size_text(5 << 20), "5.0 MB");
        assert_eq!(size_text(3 << 30), "3.00 GB");
    }

    #[test]
    fn dates_are_formatted_and_zero_is_blank() {
        assert_eq!(date_text(0), "");
        let t = date_text(132_000_000_000_000_000); // some day in 2019, local time zone
        assert!(t.len() == 16 && t.starts_with("20"), "{t}");
    }
}

#[cfg(test)]
mod search_tests {
    #[test]
    fn local_search_finds_names_below_the_folder() {
        let d = std::env::temp_dir().join(format!("rx-search-test-{}", std::process::id()));
        std::fs::create_dir_all(d.join(r"sub\deep")).unwrap();
        for f in [r"Report.txt", r"sub\notes.md", r"sub\deep\REPORT-2.pdf"] { std::fs::write(d.join(f), "x").unwrap(); }
        let (rows, cut, flipped) = super::local_search(&d, "report");
        let (wrong, _, was_flipped) = super::local_search(&d, "кузщке"); // "report" typed in the Russian layout
        let mut names: Vec<String> = rows.iter().map(|r| std::path::Path::new(&r.name).file_name().unwrap().to_string_lossy().into_owned()).collect();
        names.sort();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(names, ["REPORT-2.pdf", "Report.txt"]); // any case, any depth, absolute paths
        assert!(!cut && !flipped);
        assert!(was_flipped && wrong.len() == 2);
    }

    #[test]
    fn layout_flip_both_ways() {
        assert_eq!(super::flip_layout("сфкпщ"), "cargo");
        assert_eq!(super::flip_layout("ljrevtyns"), "документы");
        assert_eq!(super::flip_layout("Ыфму"), "Save");
    }
}
