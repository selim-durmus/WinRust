//! The look of the file manager window, in the same palette as topbar: the top part (tabs, back / forward / up / reload,
//! address pill) and the status line are painted by hand (double buffered, eased hover, click flash); the file list is the
//! native list themed dark with taller rows. Everything is scaled by the system DPI.
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use std::{cell::Cell, collections::HashMap, path::Path};
use windows_sys::Win32::{Graphics::{Dwm::DwmSetWindowAttribute, Gdi::*}, System::{LibraryLoader::{GetProcAddress, LoadLibraryW}, SystemInformation::GetTickCount64}, UI::Input::KeyboardAndMouse::{GetFocus, TRACKMOUSEEVENT, TME_LEAVE, TrackMouseEvent}};

// The colours come from the WinRust style the topbar gear picks (normal, terminal, dock, tiling): the same palette as the bar.
pub use winrust_common::style::{pal, ui_face};
// sunk surfaces: darker than the window on the dark styles, white on the light one
pub fn listbg() -> u32 { if winrust_common::style::style().light() { winrust_common::style::card() } else { mix(pal().bg, 0, 0.22) } }
pub fn pill() -> u32 { if winrust_common::style::style().light() { mix(pal().bg, pal().text, 0.06) } else { mix(pal().bg, 0, 0.35) } }
pub fn off() -> u32 { mix(pal().dim, pal().bg, 0.45) }

pub const CHROME_H: i32 = 88;
pub const WM_UI: u32 = WM_APP + 4; // chrome -> window: wp = action (see Hit), lp = tab index
const WM_MOUSELEAVE: u32 = 0x02A3; // not exported under the enabled features

// what the pointer is over / what a click does
const NONE: i32 = -1;
const BACK: i32 = 1;
const FWD: i32 = 2;
const UP: i32 = 3;
const RELOAD: i32 = 4;
const SEARCH: i32 = 5;
const PLUS: i32 = 7;
const STAR: i32 = 8;
const GEAR: i32 = 9;
const TAB: i32 = 100; // + index
const CLOSE: i32 = 200; // + index
const CRUMB: i32 = 400; // + index: a part of the path in the address pill

thread_local! {
    static CRUMBS: RefCell<Vec<(RECT, PathBuf)>> = const { RefCell::new(Vec::new()) }; // where each path part was painted
    static SEARCHING: RefCell<Option<String>> = const { RefCell::new(None) }; // the list shows hits for this query
}

/// The list shows search hits for `q` (None: a folder again); the pill says so instead of showing the path.
pub fn set_search(q: Option<String>) { SEARCHING.set(q); }
/// The "? " whole-PC search hits are up (not a folder).
pub fn searching() -> bool { SEARCHING.with_borrow(|s| s.is_some()) }

/// Folder of the path part `i` of the address pill.
pub fn crumb_path(i: usize) -> Option<PathBuf> { CRUMBS.with_borrow(|c| c.get(i).map(|x| x.1.clone())) }

fn crumb_at(x: i32, y: i32) -> Option<usize> { CRUMBS.with_borrow(|c| c.iter().position(|(r, _)| inside(r, x, y))) }

/// Path parts with the folder each one opens: C:\Users\me -> [("C:", C:\), ("Users", C:\Users), ("me", C:\Users\me)].
fn crumbs_of(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut acc = PathBuf::new();
    dir.components().filter_map(|c| {
        acc.push(c.as_os_str());
        match c {
            std::path::Component::RootDir => None, // folded into the drive part
            std::path::Component::Prefix(p) => { acc.push("\\"); Some((p.as_os_str().to_string_lossy().into_owned(), acc.clone())) }
            _ => Some((c.as_os_str().to_string_lossy().into_owned(), acc.clone())),
        }
    }).collect()
}

thread_local! {
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
    static HV: RefCell<HashMap<i32, (f32, u64)>> = RefCell::new(HashMap::new()); // eased hover per element: (0..1, last update)
    static BUSY: Cell<bool> = const { Cell::new(false) }; // something is still easing: paint another frame
    static HOT: Cell<i32> = const { Cell::new(NONE) };
    static PRESS: Cell<(i32, u64)> = const { Cell::new((i32::MIN, 0)) };
    static BRUSH: Cell<isize> = const { Cell::new(0) }; // address box background
}

pub fn init_scale() {
    unsafe {
        let dc = GetDC(ptr::null_mut());
        SCALE.set(GetDeviceCaps(dc, LOGPIXELSX as i32) as f32 / 96.0);
        ReleaseDC(ptr::null_mut(), dc);
    }
}

pub fn sc(v: i32) -> i32 { (v as f32 * SCALE.get()).round() as i32 }

fn now() -> u64 { unsafe { GetTickCount64() } }

fn ease(t: f32) -> f32 { t * t * (3.0 - 2.0 * t) }

pub(crate) fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| {
        let (x, y) = (((a >> s) & 255) as f32, ((b >> s) & 255) as f32);
        (x + (y - x) * t) as u32
    };
    ch(0) | ch(8) << 8 | ch(16) << 16
}

/// Eased 0..1 hover value of one element; keeps the painter running while it moves.
fn hv(key: i32, on: bool) -> f32 {
    let t = now();
    let target = if on { 1.0 } else { 0.0 };
    let v = HV.with_borrow_mut(|m| {
        let e = m.entry(key).or_insert((0.0, t));
        let dt = t.saturating_sub(e.1).min(50) as f32;
        e.1 = t;
        e.0 = if e.0 < target { (e.0 + dt / 120.0).min(target) } else { (e.0 - dt / 200.0).max(target) };
        e.0
    });
    if v != target { BUSY.set(true); }
    ease(v)
}

/// 1.0 right after a click, easing to 0 over 320 ms.
fn press_t(key: i32) -> f32 {
    let (k, t0) = PRESS.get();
    let t = now().saturating_sub(t0) as f32 / 320.0;
    if k != key || t >= 1.0 { return 0.0; }
    BUSY.set(true);
    (1.0 - t) * (1.0 - t)
}

pub fn font(px: i32, weight: i32, face: &str) -> isize {
    unsafe { CreateFontW(-sc(px), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, w(face).as_ptr()) as isize }
}

// ---------- window frame ----------

/// Dark title bar in the bar's colours, rounded corners.
pub fn dark_title(h: HWND) {
    unsafe {
        let set = |attr: u32, v: i32| { DwmSetWindowAttribute(h, attr, &v as *const _ as _, 4); };
        set(20, !winrust_common::style::style().light() as i32); // immersive dark mode, except on the light style
        set(33, if winrust_common::style::style().square() { 1 } else { 2 }); // round corners, square in the terminal and tiling styles
        set(35, pal().bg as i32); // caption
        set(36, pal().text as i32); // caption text
        set(34, pal().line as i32); // border
    }
}

/// Dark context menus / scrollbars for the whole process (uxtheme's unnamed exports 135 and 136). Missing = stays light.
pub fn dark_mode() {
    unsafe {
        let m = LoadLibraryW(w("uxtheme.dll").as_ptr());
        if m.is_null() { return; }
        if let Some(f) = GetProcAddress(m, 135usize as *const u8) { std::mem::transmute::<_, extern "system" fn(i32) -> i32>(f)(2); } // SetPreferredAppMode(ForceDark)
        if let Some(f) = GetProcAddress(m, 136usize as *const u8) { std::mem::transmute::<_, extern "system" fn()>(f)(); } // FlushMenuThemes
    }
}

/// The WinRust style changed (the topbar's broadcast): the lists take the new colours and font, everything repaints.
pub unsafe fn restyle(h: HWND) {
    unsafe extern "system" fn each(c: HWND, _: LPARAM) -> i32 {
        let mut cls = [0u16; 32];
        let n = GetClassNameW(c, cls.as_mut_ptr(), 32).max(0) as usize;
        if String::from_utf16_lossy(&cls[..n]) == "SysListView32" {
            style_list(c);
        }
        1
    }
    EnumChildWindows(h, Some(each), 0);
    // the list rows' cached fonts and the title bar follow too
    let (a, b) = FONTS.replace((0, 0));
    for f in [a, b, BRUSH.replace(0)].into_iter().filter(|&f| f != 0) {
        DeleteObject(f as HGDIOBJ);
    }
    dark_title(h);
    // the window class was registered with the old background brush (shows while a pane resizes)
    let old = SetClassLongPtrW(h, GCLP_HBRBACKGROUND, CreateSolidBrush(pal().bg) as isize);
    DeleteObject(old as HGDIOBJ);
    RedrawWindow(h, ptr::null(), ptr::null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_FRAME);
}

/// Dark list: themed scrollbars and header, own colours and font, taller rows.
pub fn style_list(list: HWND) {
    unsafe {
        SetWindowTheme(list, w("DarkMode_Explorer").as_ptr(), ptr::null());
        let hdr = SendMessageW(list, LVM_GETHEADER, 0, 0) as HWND;
        SetWindowTheme(hdr, w("DarkMode_ItemsView").as_ptr(), ptr::null());
        for (m, c) in [(LVM_SETBKCOLOR, listbg()), (LVM_SETTEXTBKCOLOR, listbg()), (LVM_SETTEXTCOLOR, pal().text)] { SendMessageW(list, m, 0, c as isize); }
        SendMessageW(list, WM_SETFONT, font(14, 400, ui_face()) as usize, 1);
        row_height(list, ST.with(|c| c.try_borrow().map_or(true, |s| !s.thumbs)));
        SetWindowSubclass(list, Some(list_sub), 1, 0); // paints the column header (the theme alone leaves it dim)
    }
}

thread_local! {
    static ROW: Cell<i32> = const { Cell::new(36) }; // details row height (logical px), set by the zoom
}

/// Details rows `h` logical px tall (zoom); the row fonts grow with them.
pub fn set_row(h: i32) {
    ROW.set(h);
    let (a, b) = FONTS.replace((0, 0));
    for f in [a, b].into_iter().filter(|&f| f != 0) { unsafe { DeleteObject(f as HGDIOBJ) }; }
}

thread_local! {
    static SIZED: Cell<[isize; 4]> = const { Cell::new([0; 4]) }; // two state and two picture-size image lists, see sized_list
    static FLIP: Cell<[bool; 2]> = const { Cell::new([false; 2]) };
}

/// An empty image list `w` x `h` for slot `k` (0 row height, 1 picture size). Two per slot, taken in turn and resized:
/// the list control re-measures its rows only when it is handed a different list (the same one resized is ignored, and
/// the rows never got shorter), and lists still in its hands are never destroyed.
pub fn sized_list(k: usize, w: i32, h: i32) -> isize {
    let mut flip = FLIP.get();
    flip[k] = !flip[k];
    FLIP.set(flip);
    let i = k * 2 + flip[k] as usize;
    let mut all = SIZED.get();
    unsafe {
        if all[i] == 0 { all[i] = ImageList_Create(w, h, ILC_COLOR32, 1, 1) as isize; SIZED.set(all); } else { ImageList_SetIconSize(all[i] as _, w, h); }
    }
    all[i]
}

/// Details rows get their height through an empty 1 px wide state image list (a row is as tall as its tallest image).
/// Off for the thumbnail view, where a state image would take room next to every thumbnail.
pub fn row_height(list: HWND, on: bool) {
    let il = if on { sized_list(0, 1, sc(ROW.get())) } else { 0 };
    unsafe { SendMessageW(list, LVM_SETIMAGELIST, LVSIL_STATE as usize, il) };
    remeasure(list);
}

thread_local! {
    static LIST_FONTS: Cell<[isize; 2]> = const { Cell::new([0; 2]) }; // two equal fonts for the list, see remeasure
}

/// Makes the list measure its rows again. It does so only when it gets a font it does not have yet (the same handle again,
/// or a new image list, changed nothing and rows never got shorter), so it gets the other of two equal fonts in turn.
pub fn remeasure(list: HWND) {
    let mut f = LIST_FONTS.get();
    if f[0] == 0 { f = [font(14, 400, ui_face()), font(14, 400, ui_face())]; LIST_FONTS.set(f); }
    let cur = unsafe { SendMessageW(list, WM_GETFONT, 0, 0) };
    let next = if cur == f[0] { f[1] } else { f[0] };
    unsafe {
        // details rows only grow on their own; a trip through the list view (not drawn) measures them from scratch
        let details = SendMessageW(list, LVM_GETVIEW, 0, 0) == LV_VIEW_DETAILS as isize;
        SendMessageW(list, WM_SETREDRAW, 0, 0);
        if details { SendMessageW(list, LVM_SETVIEW, LV_VIEW_LIST as usize, 0); }
        SendMessageW(list, WM_SETFONT, next as usize, 0);
        if details { SendMessageW(list, LVM_SETVIEW, LV_VIEW_DETAILS as usize, 0); }
        SendMessageW(list, WM_SETREDRAW, 1, 0);
    }
}

thread_local! {
    static SYS: Cell<[(isize, i32); 4]> = const { Cell::new([(0, 0); 4]) }; // system icons: (image list, px), smallest first
    static ICONF: Cell<isize> = const { Cell::new(0) }; // label font of the icon view
}

/// The system's icons in every size they come in (small, large, extra large, jumbo), smallest first: (image list, px).
/// Indexes are the same in all of them.
fn sys_lists() -> [(isize, i32); 4] {
    if SYS.get()[0].0 == 0 {
        let iid = windows_sys::core::GUID::from_u128(0x46eb5926_582e_4017_9fdf_e8998daa0950); // IID_IImageList
        let mut v = [(0isize, 0i32); 4];
        for (k, which) in [1, 0, 2, 4].into_iter().enumerate() { // SHIL_SMALL, SHIL_LARGE, SHIL_EXTRALARGE, SHIL_JUMBO
            let mut p: *mut core::ffi::c_void = ptr::null_mut();
            unsafe { SHGetImageList(which, &iid, &mut p) };
            let (mut cx, mut cy) = (0, 0);
            if !p.is_null() { unsafe { ImageList_GetIconSize(p as _, &mut cx, &mut cy) }; }
            v[k] = (p as isize, cx);
        }
        SYS.set(v);
    }
    SYS.get()
}

/// Draws system icon `idx` `s` px big at (x, y): straight from the image list of that size, else scaled down from the next
/// bigger one (the biggest one when none is big enough).
pub unsafe fn draw_icon(dc: HDC, idx: i32, x: i32, y: i32, s: i32) {
    if idx < 0 { return; }
    let lists = sys_lists();
    let pick = lists.iter().copied().find(|&(il, sz)| il != 0 && sz >= s).or_else(|| lists.iter().copied().rev().find(|&(il, _)| il != 0));
    let Some((il, size)) = pick else { return };
    if size == s { ImageList_Draw(il as _, idx, dc, x, y, ILD_TRANSPARENT); return; }
    let hi = ImageList_GetIcon(il as _, idx, ILD_TRANSPARENT);
    if hi.is_null() { return; }
    DrawIconEx(dc, x, y, hi, s, s, 0, ptr::null_mut(), DI_NORMAL);
    DestroyIcon(hi);
}

thread_local! {
    static ROWHOT: Cell<isize> = const { Cell::new(-1) }; // list row under the pointer
    static FONTS: Cell<(isize, isize)> = const { Cell::new((0, 0)) }; // (name, details) fonts of the list rows
    static WHEEL: Cell<i32> = const { Cell::new(0) }; // Ctrl+wheel turned less than one notch (touchpads)
}

/// "Today, 09:48" / "Yesterday, 21:50" for recent files, the plain date otherwise.
fn friendly_date(ft: u64) -> String {
    let s = date_text(ft);
    let mut now: windows_sys::Win32::Foundation::FILETIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime(&mut now) };
    let now = (now.dwHighDateTime as u64) << 32 | now.dwLowDateTime as u64;
    let (today, yest) = (date_text(now), date_text(now.saturating_sub(864_000_000_000))); // 24 h in 100 ns ticks
    match s.get(..10) {
        Some(d) if s.len() >= 16 && today.get(..10) == Some(d) => format!("Today, {}", &s[11..]),
        Some(d) if s.len() >= 16 && yest.get(..10) == Some(d) => format!("Yesterday, {}", &s[11..]),
        _ => s,
    }
}

/// Selection / hover colours shared by both views: (background, selected, list focused).
unsafe fn row_look(list: HWND, i: usize) -> (u32, bool, bool) {
    let sel = SendMessageW(list, LVM_GETITEMSTATE, i, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0;
    let hot = ROWHOT.get() == i as isize;
    let focus = GetFocus() == list;
    let bg = match (sel, focus) {
        (true, true) => mix(listbg(), pal().accent, if hot { 0.26 } else { 0.20 }),
        (true, false) => mix(listbg(), pal().text, 0.10),
        _ if hot => mix(listbg(), pal().text, 0.055),
        _ => listbg(),
    };
    (bg, sel, focus)
}

/// The rows, painted by hand (NM_CUSTOMDRAW of the list), in both views. Details: a rounded accent pill with a marker for
/// the selection, a soft one under the pointer, the name bright and the rest dimmed, icons and text growing with the
/// zoom. Icons: the picture (thumbs.rs) scaled to the zoom, the name in up to two lines under it.
pub unsafe fn list_draw(lp: LPARAM) -> LRESULT {
    let cd = &*(lp as *const NMLVCUSTOMDRAW);
    let list = cd.nmcd.hdr.hwndFrom;
    let Some((icons, view)) = ST.with(|c| c.try_borrow().ok().map(|s| (s.thumbs, s.view))) else { return CDRF_DODEFAULT as isize };
    let rows_view = matches!(view, View::Details | View::Tree); // the views with columns
    let dc = cd.nmcd.hdc;
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(list, &mut cr);
    match cd.nmcd.dwDrawStage {
        CDDS_PREPAINT => {
            // the columns must fill the width exactly (no horizontal scrollbar, no gap): when the client width changed
            // without a WM_SIZE (a scrollbar came or went), they are fitted again right after this paint
            if rows_view {
                let rest: i32 = (1..COLS).map(|k| SendMessageW(list, LVM_GETCOLUMNWIDTH, k, 0) as i32).sum();
                if SendMessageW(list, LVM_GETCOLUMNWIDTH, 0, 0) as i32 != col0_width(cr.right, rest) { PostMessageW(list, WM_FIT, 0, 0); }
            }
            (CDRF_NOTIFYITEMDRAW | CDRF_NOTIFYPOSTPAINT) as isize
        }
        CDDS_POSTPAINT => {
            let n = SendMessageW(list, LVM_GETITEMCOUNT, 0, 0);
            if n == 0 {
                // an empty folder says so instead of showing a blank list (over a clean background: the dark theme
                // draws the column dividers on down through the empty list)
                let mut hr: RECT = std::mem::zeroed();
                GetWindowRect(SendMessageW(list, LVM_GETHEADER, 0, 0) as HWND, &mut hr);
                let top = if rows_view { hr.bottom - hr.top } else { 0 };
                let b = CreateSolidBrush(listbg());
                FillRect(dc, &rc(0, top, cr.right, cr.bottom), b);
                DeleteObject(b);
                let (f_big, f_txt) = (font(40, 400, winrust_common::style::icon_face()) as HFONT, font(14, 400, ui_face()) as HFONT);
                SetBkMode(dc, TRANSPARENT as i32);
                let mid = cr.bottom / 3;
                let of = SelectObject(dc, f_big);
                text(dc, "\u{E8B7}", rc(0, mid - sc(40), cr.right, mid + sc(10)), mix(listbg(), pal().dim, 0.5), DT_CENTER);
                SelectObject(dc, f_txt);
                let msg = if SEARCHING.with_borrow(|s| s.is_some()) || filtering() { "Nothing found" } else { "This folder is empty" };
                text(dc, msg, rc(0, mid + sc(14), cr.right, mid + sc(40)), pal().dim, DT_CENTER);
                SelectObject(dc, of);
                DeleteObject(f_big);
                DeleteObject(f_txt);
            } else if rows_view {
                // below the last row the dark theme draws the column dividers on down into the empty space: painted over
                let mut lr = RECT { left: LVIR_BOUNDS as i32, ..std::mem::zeroed() };
                SendMessageW(list, LVM_GETITEMRECT, (n - 1) as usize, &mut lr as *mut _ as isize);
                if lr.bottom < cr.bottom {
                    let b = CreateSolidBrush(listbg());
                    FillRect(dc, &rc(0, lr.bottom.max(0), cr.right, cr.bottom), b);
                    DeleteObject(b);
                }
            }
            CDRF_DODEFAULT as isize
        }
        CDDS_ITEMPREPAINT => {
            let i = cd.nmcd.dwItemSpec;
            let row = ST.with(|c| {
                let mut st = c.try_borrow_mut().ok()?;
                let State { dir, rows, .. } = &mut *st;
                let r = rows.get_mut(i)?;
                if r.icon < 0 { r.icon = icon_of(&dir.join(&r.name), r.dir); }
                Some((r.name.clone(), r.dir, r.time, r.size, r.icon, dir.join(&r.name), r.depth, r.open))
            });
            let Some((name, is_dir, time, size, icon, full, depth, open)) = row else { return CDRF_DODEFAULT as isize };
            let p = Path::new(&name);
            let hit_of_search = p.is_absolute(); // search hits are absolute paths: show the name, and where it lies in the type column
            // search hits and the tree's inner rows carry a path: the name is its last part
            let shown = name.rsplit('\\').next().unwrap_or(&name).to_string();
            let (bg, sel, focus) = row_look(list, i);
            SetBkMode(dc, TRANSPARENT as i32);
            if icons {
                let mut b = RECT { left: LVIR_BOUNDS as i32, ..std::mem::zeroed() };
                SendMessageW(list, LVM_GETITEMRECT, i, &mut b as *mut _ as isize);
                let mut ic = RECT { left: LVIR_ICON as i32, ..std::mem::zeroed() };
                SendMessageW(list, LVM_GETITEMRECT, i, &mut ic as *mut _ as isize);
                let clear = CreateSolidBrush(listbg());
                FillRect(dc, &rc(b.left, b.top, b.right, b.bottom.max(ic.top + THUMB_PX.load(Relaxed) + sc(50))), clear);
                DeleteObject(clear);
                let px = THUMB_PX.load(Relaxed);
                let (x, y) = ((ic.left + ic.right - px) / 2, ic.top + sc(6));
                // the name gets two lines under the picture (the list itself counts one)
                let lab = rc(b.left + sc(6), y + px + sc(6), b.right - sc(6), y + px + sc(6) + sc(36));
                if bg != listbg() { fill(dc, rc(b.left + sc(2), b.top + sc(2), b.right - sc(2), lab.bottom + sc(2)), bg, sc(12)); }
                match thumbs::get(&full, px) {
                    Some(pic) => thumbs::draw(dc, pic, x, y, px),
                    None => draw_icon(dc, icon, x, y, px), // the system icon, as big, until the picture is here
                }
                if ICONF.get() == 0 { ICONF.set(font(12, 400, ui_face())); }
                let of = SelectObject(dc, ICONF.get() as HFONT);
                SetTextColor(dc, if sel { pal().text } else { mix(pal().dim, pal().text, 0.75) });
                let mut lr = lab;
                DrawTextW(dc, w(&shown).as_ptr(), -1, &mut lr, DT_CENTER | DT_WORDBREAK | DT_END_ELLIPSIS | DT_EDITCONTROL | DT_NOPREFIX);
                SelectObject(dc, of);
                return CDRF_SKIPDEFAULT as isize;
            }
            if view == View::List {
                let mut b = RECT { left: LVIR_BOUNDS as i32, ..std::mem::zeroed() };
                SendMessageW(list, LVM_GETITEMRECT, i, &mut b as *mut _ as isize);
                let clear = CreateSolidBrush(listbg());
                FillRect(dc, &b, clear);
                DeleteObject(clear);
                if bg != listbg() { fill(dc, rc(b.left + sc(2), b.top + sc(1), b.right - sc(4), b.bottom - sc(1)), bg, sc(8)); }
                let s = sc(16);
                draw_icon(dc, icon, b.left + sc(8), (b.top + b.bottom - s) / 2, s);
                if ICONF.get() == 0 { ICONF.set(font(12, 400, ui_face())); }
                let of = SelectObject(dc, ICONF.get() as HFONT);
                text(dc, &shown, rc(b.left + sc(8) + s + sc(8), b.top, b.right - sc(8), b.bottom), pal().text, DT_LEFT);
                SelectObject(dc, of);
                return CDRF_SKIPDEFAULT as isize;
            }
            let mut r = RECT { left: LVIR_BOUNDS as i32, ..std::mem::zeroed() };
            SendMessageW(list, LVM_GETITEMRECT, i, &mut r as *mut _ as isize);
            let row_r = rc(0, r.top, cr.right, r.bottom);
            let b = CreateSolidBrush(listbg());
            FillRect(dc, &row_r, b);
            DeleteObject(b);
            let pill = rc(sc(6), r.top + sc(2), (r.right).min(cr.right) - sc(6), r.bottom - sc(2)); // r.right = where the columns end
            // every other row a shade lighter, for the eye to follow a row across (File Pilot does the same)
            let base = if i % 2 == 1 { mix(listbg(), pal().text, 0.035) } else { listbg() };
            if bg != listbg() { fill(dc, pill, bg, sc(10)); } else if base != listbg() { fill(dc, rc(pill.left, r.top + sc(1), pill.right, r.bottom - sc(1)), base, sc(8)); }
            if sel { fill(dc, rc(pill.left, pill.top + sc(8), pill.left + sc(3), pill.bottom - sc(8)), if focus { pal().accent } else { pal().dim }, sc(3)); }
            // columns from the header widths (left of the row = minus the horizontal scroll)
            let cw: Vec<i32> = (0..COLS).map(|k| SendMessageW(list, LVM_GETCOLUMNWIDTH, k, 0) as i32).collect();
            let x: Vec<i32> = (0..COLS).map(|k| r.left + cw[..k].iter().sum::<i32>()).collect();
            // the icon grows with the row: 16 px up to 38 px rows, then about half the row
            let s = sc(if ROW.get() < 40 { 16 } else { (ROW.get() as f32 * 0.55).round() as i32 });
            // the tree: each level indented, an arrow on the folders (right = closed, down = open)
            let indent = if view == View::Tree { sc(TREE_STEP) * depth as i32 + sc(TREE_ARROW) } else { 0 };
            if view == View::Tree && is_dir {
                if GLYPHF.get() == 0 { GLYPHF.set(font(11, 400, winrust_common::style::icon_face())); }
                let of = SelectObject(dc, GLYPHF.get() as HFONT);
                let ax = x[0] + sc(10) + sc(TREE_STEP) * depth as i32;
                text(dc, if open { "\u{E70D}" } else { "\u{E76C}" }, rc(ax, r.top, ax + sc(TREE_ARROW), r.bottom), if open { pal().text } else { pal().dim }, DT_CENTER);
                SelectObject(dc, of);
            }
            draw_icon(dc, icon, x[0] + sc(16) + indent, (r.top + r.bottom - s) / 2, s);
            if FONTS.get().0 == 0 {
                FONTS.set((font(13, 400, ui_face()), font(12, 400, ui_face())));
            }
            let (f_name, f_det) = FONTS.get();
            let of = SelectObject(dc, f_name as HFONT);
            text(dc, &shown, rc(x[0] + sc(16) + indent + s + sc(12), r.top, x[1] - sc(8), r.bottom), pal().text, DT_LEFT);
            SelectObject(dc, f_det as HFONT);
            let dim = if sel { mix(pal().dim, pal().text, 0.45) } else { pal().dim };
            text(dc, &friendly_date(time), rc(x[1] + sc(10), r.top, x[2] - sc(8), r.bottom), dim, DT_LEFT);
            // where a hit lies: below the shown folder as "sub\deep", elsewhere the full folder
            let kind = if hit_of_search {
                let parent = p.parent().unwrap_or(p);
                let here = ST.with(|c| c.try_borrow().map(|s| s.dir.clone()).unwrap_or_default());
                match parent.strip_prefix(&here) { Ok(rel) if rel.as_os_str().is_empty() => "(this folder)".into(), Ok(rel) => rel.to_string_lossy().into_owned(), Err(_) => parent.to_string_lossy().into_owned() }
            } else { type_text(&name, is_dir) };
            text(dc, &kind, rc(x[2] + sc(10), r.top, x[3] - sc(8), r.bottom), dim, DT_LEFT | DT_PATH_ELLIPSIS);
            // Folders / Files / Size: a folder's totals from sizes.rs (dimmer while still counting), a file's own size
            let cell = |k: usize| rc(x[k] + sc(10), r.top, x[k] + cw[k] - sc(8), r.bottom);
            if is_dir {
                if let Some(s) = PREFS.with_borrow(|p| p.folder_sizes).then(|| sizes::get(&full)).flatten() {
                    let c = if s.done { dim } else { mix(dim, listbg(), 0.45) };
                    let n = |k: u32| if k == 0 { "\u{2013}".to_string() } else { thousands(k as usize) }; // none: a dash, like File Pilot
                    text(dc, &n(s.dirs), cell(3), c, DT_LEFT);
                    text(dc, &n(s.files), cell(4), c, DT_LEFT);
                    text(dc, &size_text(s.bytes), cell(5), c, DT_LEFT);
                }
            } else {
                let off = mix(dim, listbg(), 0.5);
                text(dc, "\u{2013}", cell(3), off, DT_LEFT);
                text(dc, "\u{2013}", cell(4), off, DT_LEFT);
                text(dc, &size_text(size), cell(5), dim, DT_LEFT);
            }
            SelectObject(dc, of);
            CDRF_SKIPDEFAULT as isize
        }
        _ => CDRF_DODEFAULT as isize,
    }
}

/// Columns of the details view (Name, Date modified, Type, Folders, Files, Size).
pub const COLS: usize = 6;
const TREE_STEP: i32 = 18; // the tree's indent per level
const TREE_ARROW: i32 = 16; // the width of its arrow
thread_local! { static GLYPHF: Cell<isize> = const { Cell::new(0) }; } // the arrows' font

/// Where a click lands in the tree: Some(row) on a folder's arrow (it opens or closes it, nothing is selected).
unsafe fn tree_arrow_at(list: HWND, x: i32, y: i32) -> Option<usize> {
    let mut ht: LVHITTESTINFO = std::mem::zeroed();
    ht.pt = POINT { x, y };
    let i = usize::try_from(SendMessageW(list, LVM_HITTEST, 0, &mut ht as *mut _ as isize)).ok()?;
    let (is_dir, depth) = ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).map(|r| (r.dir, r.depth))))?;
    let mut r = RECT { left: LVIR_BOUNDS as i32, ..std::mem::zeroed() };
    SendMessageW(list, LVM_GETITEMRECT, i, &mut r as *mut _ as isize);
    let ax = r.left + sc(10) + sc(TREE_STEP) * depth as i32;
    (is_dir && x >= ax - sc(4) && x < ax + sc(TREE_ARROW) + sc(4)).then_some(i)
}

/// Name column takes what the other columns leave, so the list never needs a horizontal scrollbar. A few pixels are left
/// over: with the columns exactly as wide as the list it put up a horizontal scrollbar anyway, and taking that away by
/// force made the rows jump up and down while scrolling (the list and the fix kept undoing each other).
fn col0_width(client: i32, rest: i32) -> i32 { (client - rest - sc(4)).max(sc(200)) }

const WM_FIT: u32 = WM_APP + 20; // to the list: fit the columns to its width again

pub unsafe fn fit_columns(list: HWND) {
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(list, &mut cr);
    let rest: i32 = (1..COLS).map(|k| SendMessageW(list, LVM_GETCOLUMNWIDTH, k, 0) as i32).sum();
    let want = col0_width(cr.right, rest);
    if SendMessageW(list, LVM_GETCOLUMNWIDTH, 0, 0) as i32 != want { SendMessageW(list, LVM_SETCOLUMNWIDTH, 0, want as isize); }
}

thread_local! {
    static SCROLL_LEFT: Cell<i32> = const { Cell::new(0) }; // wheel scrolling still to do (px, + = down)
}
const SCROLL_TIMER: usize = 77;

/// One frame of the wheel's glide: the icon view moves by pixels, easing out; details move a row at a time (the list
/// itself only scrolls by whole rows there), quicker while much is left. Stops at either end.
unsafe fn scroll_tick(h: HWND) {
    let left = SCROLL_LEFT.get();
    let icons = ST.with(|c| c.try_borrow().map_or(false, |s| s.thumbs));
    let step = if icons {
        let s = (left as f32 * 0.28) as i32;
        if s == 0 { left.signum() * left.abs().min(sc(2)) } else { s }
    } else {
        let mut r = RECT { left: LVIR_BOUNDS as i32, ..std::mem::zeroed() };
        let top = SendMessageW(h, LVM_GETTOPINDEX, 0, 0);
        SendMessageW(h, LVM_GETITEMRECT, top.max(0) as usize, &mut r as *mut _ as isize);
        let row = (r.bottom - r.top).max(1);
        let rows = (left.abs() / row).max(1);
        if left.abs() < row / 2 { 0 } else { left.signum() * row * (1 + rows / 4) }
    };
    let before = GetScrollPos(h, SB_VERT as i32);
    if step != 0 { SendMessageW(h, LVM_SCROLL, 0, step as isize); }
    let moved = GetScrollPos(h, SB_VERT as i32) != before;
    SCROLL_LEFT.set(if moved { left - step } else { 0 });
    if !moved || SCROLL_LEFT.get() == 0 || step == 0 { SCROLL_LEFT.set(0); KillTimer(h, SCROLL_TIMER); }
}

/// Repaints one row (hover moved on or off it).
unsafe fn redraw_row(list: HWND, i: isize) {
    if i < 0 { return; }
    let mut r = RECT { left: LVIR_BOUNDS as i32, ..std::mem::zeroed() };
    if SendMessageW(list, LVM_GETITEMRECT, i as usize, &mut r as *mut _ as isize) != 0 {
        let mut cr: RECT = std::mem::zeroed();
        GetClientRect(list, &mut cr);
        InvalidateRect(list, &rc(0, r.top, cr.right, r.bottom), 0);
    }
}

/// Column header, drawn by hand: bar colour, white / dim title, a sort arrow on the sorted column. The header reports its
/// custom draw to its parent, the list, so the hook sits on the list.
unsafe extern "system" fn list_sub(h: HWND, m: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    // typing in the list filters it (instead of jumping to the first name with that letter); a space with nothing typed
    // yet stays the quick look
    // Ctrl+wheel: bigger / smaller (one zoom step per notch; a touchpad's small turns add up)
    if m == WM_MOUSEWHEEL && wp & 0x0008 != 0 && !desk() {
        let d = WHEEL.get() + ((wp >> 16) & 0xFFFF) as i16 as i32;
        let steps = d / 120;
        WHEEL.set(d - steps * 120);
        if steps != 0 { PostMessageW(GetAncestor(h, GA_ROOT), WM_UI, 25, steps as isize); }
        return 0;
    }
    // the wheel glides instead of jumping three rows at once
    if m == WM_MOUSEWHEEL && wp & 0x0008 == 0 && !desk() {
        let d = ((wp >> 16) & 0xFFFF) as i16 as i32;
        let mut lines = 3u32;
        SystemParametersInfoW(SPI_GETWHEELSCROLLLINES, 0, &mut lines as *mut _ as *mut _, 0);
        let icons = ST.with(|c| c.try_borrow().map_or(false, |s| s.thumbs));
        let unit = if icons { sc(40) } else { sc(ROW.get()) };
        SCROLL_LEFT.set(SCROLL_LEFT.get() - d * lines.min(10) as i32 * unit / 120);
        SetTimer(h, SCROLL_TIMER, 10, None);
        return 0;
    }
    if m == WM_TIMER && wp == SCROLL_TIMER { scroll_tick(h); return 0; }
    // the tree: a click on a folder's arrow opens or closes it
    if (m == WM_LBUTTONDOWN || m == WM_LBUTTONDBLCLK) && ST.with(|c| c.try_borrow().map_or(false, |s| s.view == View::Tree)) {
        if let Some(i) = tree_arrow_at(h, (lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32) {
            PostMessageW(GetAncestor(h, GA_ROOT), WM_UI, 26, i as isize);
            return 0;
        }
    }
    if m == WM_FIT { fit_columns(h); return 0; }
    // the scrollbar comes and goes with the rows: the columns fill the width again (no gap on the right)
    if m == WM_SIZE && !desk() {
        let r = DefSubclassProc(h, m, wp, lp);
        fit_columns(h);
        return r;
    }
    if m == WM_CHAR && !desk() && wp >= 0x20 && wp != 0x7F && (wp != 0x20 || filtering()) && PREFS.with_borrow(|p| p.type_filter) {
        filter_focus(Some(wp as u16));
        return 0;
    }
    if m == WM_MOUSEMOVE {
        // hover row for list_draw
        let mut ht: LVHITTESTINFO = std::mem::zeroed();
        ht.pt = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
        let i = SendMessageW(h, LVM_HITTEST, 0, &mut ht as *mut _ as isize);
        let old = ROWHOT.replace(i);
        if old != i {
            redraw_row(h, old);
            redraw_row(h, i);
            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
        }
    } else if m == WM_MOUSELEAVE {
        redraw_row(h, ROWHOT.replace(-1));
    } else if m == WM_SETFOCUS || m == WM_KILLFOCUS {
        InvalidateRect(h, ptr::null(), 0); // the selection is accent-coloured only while the list has the focus
    }
    if m == WM_NOTIFY {
        let nm = &*(lp as *const NMCUSTOMDRAW);
        let hdr = SendMessageW(h, LVM_GETHEADER, 0, 0) as HWND;
        if nm.hdr.hwndFrom == hdr && nm.hdr.code == NM_CUSTOMDRAW {
            return match nm.dwDrawStage {
                CDDS_PREPAINT => {
                    let b = CreateSolidBrush(pal().bg); // the bar behind the columns too
                    FillRect(nm.hdc, &nm.rc, b);
                    DeleteObject(b);
                    CDRF_NOTIFYITEMDRAW as isize
                }
                CDDS_ITEMPREPAINT => {
                    let (dc, r, i) = (nm.hdc, nm.rc, nm.dwItemSpec);
                    let b = CreateSolidBrush(pal().bg);
                    FillRect(dc, &r, b);
                    DeleteObject(b);
                    fill(dc, rc(r.right - 1, r.top + sc(8), r.right, r.bottom - sc(8)), pal().line, 0);
                    let mut buf = [0u16; 128];
                    let mut it: HDITEMW = std::mem::zeroed();
                    it.mask = HDI_TEXT;
                    it.pszText = buf.as_mut_ptr();
                    it.cchTextMax = 128;
                    SendMessageW(hdr, HDM_GETITEMW, i, &mut it as *mut _ as isize);
                    let sorted = SORT_COL.load(Relaxed) == i;
                    let arrow = if !sorted { "" } else if SORT_ASC.load(Relaxed) { "  \u{2191}" } else { "  \u{2193}" };
                    let f = font(13, if sorted { 600 } else { 400 }, ui_face()) as HFONT;
                    let of = SelectObject(dc, f);
                    SetBkMode(dc, TRANSPARENT as i32);
                    let align = DT_LEFT; // every column's title on the left, like its values
                    text(dc, &format!("{}{arrow}", wstr(buf.as_ptr())), rc(r.left + sc(10), r.top, r.right - sc(12), r.bottom), if sorted { pal().text } else { pal().dim }, align);
                    SelectObject(dc, of);
                    DeleteObject(f);
                    CDRF_SKIPDEFAULT as isize
                }
                _ => 0,
            };
        }
    }
    DefSubclassProc(h, m, wp, lp)
}

// ---------- layout and hit testing ----------

/// The top: the title bar itself holds the tabs, "+" and the window's own buttons (no title text, no icon); under it the
/// toolbar row (back, forward, up, reload, the address pill, bookmark star, filter, settings).
pub const TAB_H: i32 = 40;
const CAP_W: i32 = 46; // minimize / maximize / close
const MINB: i32 = 10;
const MAXB: i32 = 11;
const XCLOSE: i32 = 12;

struct Layout { tabs: Vec<RECT>, close: Vec<RECT>, plus: RECT, min: RECT, max: RECT, xclose: RECT, back: RECT, fwd: RECT, up: RECT, reload: RECT, pill: RECT, star: RECT, search: RECT, gear: RECT }

pub(crate) fn rc(l: i32, t: i32, r: i32, b: i32) -> RECT { RECT { left: l, top: t, right: r, bottom: b } }

fn layout(cw: i32, n: usize) -> Layout {
    let th = sc(TAB_H);
    let xclose = rc(cw - sc(CAP_W), 0, cw, th);
    let max = rc(xclose.left - sc(CAP_W), 0, xclose.left, th);
    let min = rc(max.left - sc(CAP_W), 0, max.left, th);
    let (ty0, ty1) = (sc(6), th);
    let avail = min.left - sc(8) - sc(40) - sc(24); // what the tabs may take: up to "+", with room to drag the window
    let tw = (avail / n.max(1) as i32).clamp(sc(72), sc(210));
    let tabs: Vec<RECT> = (0..n as i32).map(|i| { let l = sc(8) + i * tw; rc(l, ty0, l + tw, ty1) }).collect();
    let close = tabs.iter().map(|t| rc(t.right - sc(30), (t.top + t.bottom) / 2 - sc(11), t.right - sc(8), (t.top + t.bottom) / 2 + sc(11))).collect();
    let px = tabs.last().map_or(sc(8), |t| t.right + sc(4));
    let plus = rc(px, ty0 + sc(4), px + sc(32), ty1 - sc(4));
    let (by0, by1, bs) = (th + sc(6), th + sc(42), sc(36));
    let btn = |i: i32| rc(sc(12) + i * (bs + sc(4)), by0, sc(12) + i * (bs + sc(4)) + bs, by1);
    // right end of the toolbar: bookmark star, filter, settings
    let gear = rc(cw - sc(12) - bs, by0, cw - sc(12), by1);
    let search = rc(gear.left - sc(4) - bs, by0, gear.left - sc(4), by1);
    let star = rc(search.left - sc(4) - bs, by0, search.left - sc(4), by1);
    let reload = btn(3);
    let pill = rc(reload.right + sc(8), by0, star.left - sc(8), by1);
    Layout { tabs, close, plus, min, max, xclose, back: btn(0), fwd: btn(1), up: btn(2), reload, pill, star, search, gear }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool { x >= r.left && x < r.right && y >= r.top && y < r.bottom }

fn hit(l: &Layout, x: i32, y: i32) -> i32 {
    for (i, c) in l.close.iter().enumerate() { if inside(c, x, y) && l.tabs.len() > 1 { return CLOSE + i as i32; } }
    for (i, t) in l.tabs.iter().enumerate() { if inside(t, x, y) { return TAB + i as i32; } }
    for (k, r) in [(BACK, &l.back), (FWD, &l.fwd), (UP, &l.up), (RELOAD, &l.reload), (STAR, &l.star), (SEARCH, &l.search), (GEAR, &l.gear), (PLUS, &l.plus), (MINB, &l.min), (MAXB, &l.max), (XCLOSE, &l.xclose)] {
        if inside(r, x, y) { return k; }
    }
    NONE
}

/// Puts the address box inside the pill.
pub fn place_edit(chrome: HWND) {
    unsafe {
        let edit = ST.with(|c| c.try_borrow().map_or(ptr::null_mut(), |s| s.edit));
        if edit.is_null() { return; }
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(chrome, &mut r);
        let p = layout(r.right, 1).pill;
        let eh = sc(20);
        MoveWindow(edit, p.left + sc(40), (p.top + p.bottom - eh) / 2, (p.right - p.left - sc(40) - sc(16)).max(0), eh, 1);
        if GetFocus() != edit { ShowWindow(edit, SW_HIDE); } // the path parts are painted instead until the pill is clicked
    }
}

// ---------- painting ----------

/// GDI+ for anti-aliased rounded shapes (started once in main); fill() falls back to plain GDI without it.
pub fn gdiplus_start() {
    use windows_sys::Win32::Graphics::GdiPlus::{GdiplusStartup, GdiplusStartupInput};
    unsafe {
        let (mut tok, mut inp): (usize, GdiplusStartupInput) = (0, std::mem::zeroed());
        inp.GdiplusVersion = 1;
        GdiplusStartup(&mut tok, &inp, ptr::null_mut());
    }
}

/// Fills `r` (right / bottom exclusive) in `c`; `rad` > 0 rounds the corners (rad = corner diameter), anti-aliased.
pub(crate) unsafe fn fill(dc: HDC, r: RECT, c: u32, rad: i32) {
    let rad = if winrust_common::style::style().square() { 0 } else { rad };
    use windows_sys::Win32::Graphics::GdiPlus::*;
    let (wd, ht) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
    if wd <= 0.0 || ht <= 0.0 { return; }
    let mut g: *mut GpGraphics = ptr::null_mut();
    if rad <= 0 || GdipCreateFromHDC(dc, &mut g) != 0 || g.is_null() {
        let b = CreateSolidBrush(c);
        if rad <= 0 {
            FillRect(dc, &r, b);
        } else {
            let (ob, op) = (SelectObject(dc, b), SelectObject(dc, GetStockObject(NULL_PEN)));
            RoundRect(dc, r.left, r.top, r.right + 1, r.bottom + 1, rad, rad);
            SelectObject(dc, ob);
            SelectObject(dc, op);
        }
        DeleteObject(b);
        return;
    }
    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    GdipSetPixelOffsetMode(g, PixelOffsetModeHalf); // pixel i spans [i, i+1): straight edges stay crisp
    let (x, y, d) = (r.left as f32, r.top as f32, (rad as f32).min(wd).min(ht));
    let mut path: *mut GpPath = ptr::null_mut();
    GdipCreatePath(FillModeAlternate, &mut path);
    GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
    GdipAddPathArc(path, x + wd - d, y, d, d, 270.0, 90.0);
    GdipAddPathArc(path, x + wd - d, y + ht - d, d, d, 0.0, 90.0);
    GdipAddPathArc(path, x, y + ht - d, d, d, 90.0, 90.0);
    GdipClosePathFigure(path);
    let mut br: *mut GpSolidFill = ptr::null_mut();
    GdipCreateSolidFill(0xFF00_0000 | (c & 255) << 16 | (c & 0xFF00) | (c >> 16 & 255), &mut br); // COLORREF -> ARGB
    GdipFillPath(g, br as *mut GpBrush, path);
    GdipDeleteBrush(br as *mut GpBrush);
    GdipDeletePath(path);
    GdipDeleteGraphics(g);
}

pub(crate) unsafe fn text(dc: HDC, s: &str, mut r: RECT, c: u32, flags: u32) {
    SetTextColor(dc, c);
    DrawTextW(dc, w(s).as_ptr(), -1, &mut r, flags | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
}

/// A rounded highlight that eases in on hover and flashes in the accent colour on click; `base` is what it sits on.
unsafe fn glow(dc: HDC, key: i32, r: RECT, base: u32, to: u32, rad: i32, on: bool) {
    let (t, p) = (hv(key, on), press_t(key));
    if t > 0.01 || p > 0.0 { fill(dc, r, mix(mix(base, to, t), pal().accent, 0.45 * p), rad); }
}

/// The title bar's colour: a shade darker than the toolbar, so the active tab (toolbar coloured) stands out of it.
fn titlebg() -> u32 { if winrust_common::style::style().light() { mix(pal().bg, pal().text, 0.07) } else { mix(pal().bg, 0, 0.38) } }

unsafe fn paint_chrome(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(hwnd, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut cr);
    // everything painting needs from the window state, copied out so nothing is borrowed while drawing
    let snap = ST.with(|c| c.try_borrow().ok().map(|s| (s.tabs.iter().map(|t| (label(t), t.clone())).collect::<Vec<_>>(), s.active, s.hist.pos > 0, s.hist.pos + 1 < s.hist.v.len(), s.edit, s.dir.clone())));
    let Some((tabs, active, can_back, can_fwd, edit, dir)) = snap else { EndPaint(hwnd, &ps); return };
    BUSY.set(false);
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right, cr.bottom));
    let old = SelectObject(dc, bmp);
    let bg = CreateSolidBrush(pal().bg);
    FillRect(dc, &cr, bg);
    DeleteObject(bg);
    SetBkMode(dc, TRANSPARENT as i32);
    let l = layout(cr.right, tabs.len());
    let hot = HOT.get();
    let (f_txt, f_glyph, f_small) = (font(13, 400, ui_face()) as HFONT, font(16, 400, winrust_common::style::icon_face()) as HFONT, font(10, 400, winrust_common::style::icon_face()) as HFONT);
    let of = SelectObject(dc, f_txt);

    // the title bar: darker; the active tab in the toolbar's colour, joined to it below
    let tb = titlebg();
    fill(dc, rc(0, 0, cr.right, sc(TAB_H)), tb, 0);
    let il = sys_lists()[0];
    for (i, t) in l.tabs.iter().enumerate() {
        let on = i == active;
        let th = hv(TAB + i as i32, hot == TAB + i as i32 || hot == CLOSE + i as i32);
        let p = press_t(TAB + i as i32);
        if on {
            fill(dc, rc(t.left, t.top, t.right, t.bottom + sc(12)), mix(pal().bg, pal().accent, 0.25 * p), sc(12));
        } else if th > 0.01 || p > 0.0 {
            fill(dc, rc(t.left + sc(2), t.top + sc(3), t.right - sc(2), t.bottom - sc(4)), mix(mix(tb, pal().text, 0.07 * th), pal().accent, 0.3 * p), sc(8));
        }
        // a hairline between two tabs, like between the columns (not next to the active one, nor under the pointer)
        if i + 1 < tabs.len() && !on && i + 1 != active && th < 0.01 && hot != TAB + i as i32 + 1 {
            fill(dc, rc(t.right - 1, t.top + sc(9), t.right, t.bottom - sc(8)), pal().line, 0);
        }
        // the folder's own small icon, its name, the close button on the active tab and under the pointer
        let (icon, _) = shell_info(&tabs[i].1, !tabs[i].1.to_string_lossy().starts_with(r"\\"));
        let (mut iw, mut ih) = (0, 0);
        ImageList_GetIconSize(il.0 as _, &mut iw, &mut ih);
        if icon >= 0 { ImageList_Draw(il.0 as _, icon, dc, t.left + sc(12), (t.top + t.bottom - ih) / 2, ILD_TRANSPARENT); }
        let closable = tabs.len() > 1 && (on || th > 0.05);
        SelectObject(dc, f_txt);
        text(dc, &tabs[i].0, rc(t.left + sc(12) + iw + sc(8), t.top, t.right - if closable { sc(32) } else { sc(10) }, t.bottom), if on { pal().text } else { mix(pal().dim, pal().text, th) }, DT_LEFT);
        if closable {
            let c = l.close[i];
            glow(dc, CLOSE + i as i32, c, if on { pal().bg } else { tb }, pal().hover, sc(6), hot == CLOSE + i as i32);
            SelectObject(dc, f_small);
            text(dc, "\u{E8BB}", c, mix(pal().dim, pal().text, if hot == CLOSE + i as i32 { 1.0 } else { 0.5 }), DT_CENTER);
        }
    }
    glow(dc, PLUS, l.plus, tb, pal().hover, sc(8), hot == PLUS);
    SelectObject(dc, f_small);
    text(dc, "\u{E710}", l.plus, mix(pal().dim, pal().text, if hot == PLUS { 1.0 } else { 0.4 }), DT_CENTER);
    // the window's own buttons; close turns red under the pointer, like Windows'
    let zoomed = IsZoomed(GetParent(hwnd)) != 0;
    for (k, r, g) in [(MINB, l.min, "\u{E921}"), (MAXB, l.max, if zoomed { "\u{E923}" } else { "\u{E922}" }), (XCLOSE, l.xclose, "\u{E8BB}")] {
        let t = hv(k, hot == k);
        if t > 0.01 { fill(dc, r, if k == XCLOSE { mix(tb, rgb(196, 43, 28), t) } else { mix(tb, pal().text, 0.08 * t) }, 0); }
        text(dc, g, r, if k == XCLOSE && t > 0.5 { 0x00FF_FFFF } else { mix(pal().dim, pal().text, 0.4 + 0.6 * t) }, DT_CENTER);
    }

    // toolbar buttons
    SelectObject(dc, f_glyph);
    for (k, r, g, enabled) in [(BACK, l.back, "\u{E72B}", can_back), (FWD, l.fwd, "\u{E72A}", can_fwd), (UP, l.up, "\u{E74A}", true), (RELOAD, l.reload, "\u{E72C}", true), (SEARCH, l.search, "\u{E721}", true), (GEAR, l.gear, "\u{E713}", true)] {
        if enabled { glow(dc, k, r, pal().bg, pal().hover, sc(8), hot == k); }
        text(dc, g, r, if enabled { pal().text } else { off() }, DT_CENTER);
    }
    // the star: filled in the accent colour while this folder is a bookmark
    let starred = marked(&dir);
    glow(dc, STAR, l.star, pal().bg, pal().hover, sc(8), hot == STAR);
    text(dc, if starred { "\u{E735}" } else { "\u{E734}" }, l.star, if starred { pal().accent } else { pal().text }, DT_CENTER);

    // address pill: accent outline while it is being edited
    let focused = !edit.is_null() && GetFocus() == edit;
    let f = hv(2000, focused);
    fill(dc, l.pill, mix(pal().line, pal().accent, f), sc(18));
    fill(dc, rc(l.pill.left + 1, l.pill.top + 1, l.pill.right - 1, l.pill.bottom - 1), pill(), sc(17));
    SelectObject(dc, f_glyph);
    let searching = SEARCHING.with_borrow(|s| s.clone());
    text(dc, if searching.is_some() { "\u{E721}" } else { "\u{E838}" }, rc(l.pill.left + sc(10), l.pill.top, l.pill.left + sc(38), l.pill.bottom), mix(pal().dim, pal().accent, f), DT_CENTER);
    // not being edited: the path as clickable parts, like Windows 11 Explorer ("C: > Users > me"); the edit box is hidden
    let mut crumbs = vec![];
    if !focused {
        let (x0, x1) = (l.pill.left + sc(40), l.pill.right - sc(12));
        let (t, b) = (l.pill.top + sc(5), l.pill.bottom - sc(5));
        if let Some(q) = &searching {
            SelectObject(dc, f_txt);
            text(dc, &format!("Results for \u{201C}{q}\u{201D}"), rc(x0 + sc(6), l.pill.top, x1, l.pill.bottom), pal().accent, DT_LEFT);
        } else {
            let parts = crumbs_of(&dir);
            let width = |s: &str| { let mut r = rc(0, 0, 0, 0); DrawTextW(dc, w(s).as_ptr(), -1, &mut r, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX); r.right + sc(16) };
            let chev = sc(18);
            SelectObject(dc, f_txt);
            let ws: Vec<i32> = parts.iter().map(|p| width(&p.0)).collect();
            // too long: drop parts from the left; a "..." part then opens the last one dropped
            let mut first = 0;
            while first + 1 < parts.len() && ws[first..].iter().sum::<i32>() + chev * (parts.len() - first) as i32 + width("\u{2026}") > x1 - x0 { first += 1; }
            let mut shown: Vec<(String, PathBuf, i32)> = vec![];
            if first > 0 { shown.push(("\u{2026}".into(), parts[first - 1].1.clone(), width("\u{2026}"))); }
            shown.extend(parts[first..].iter().zip(&ws[first..]).map(|(p, &wd)| (p.0.clone(), p.1.clone(), wd)));
            let mut x = x0;
            let last = shown.len().saturating_sub(1);
            for (i, (label, path, wd)) in shown.into_iter().enumerate() {
                let r = rc(x, t, (x + wd).min(x1), b);
                glow(dc, CRUMB + i as i32, r, pill(), pal().hover, sc(8), hot == CRUMB + i as i32);
                SelectObject(dc, f_txt);
                text(dc, &label, r, if i == last { pal().text } else { mix(pal().dim, pal().text, if hot == CRUMB + i as i32 { 1.0 } else { 0.35 }) }, DT_CENTER);
                crumbs.push((r, path));
                x = r.right;
                if i < last {
                    SelectObject(dc, f_small);
                    text(dc, "\u{E76C}", rc(x, t, x + chev, b), mix(pal().dim, pill(), 0.3), DT_CENTER);
                    x += chev;
                }
            }
        }
    }
    CRUMBS.set(crumbs);

    fill(dc, rc(0, cr.bottom - 1, cr.right, cr.bottom), pal().line, 0); // hairline under the chrome
    BitBlt(wdc, 0, 0, cr.right, cr.bottom, dc, 0, 0, SRCCOPY);
    SelectObject(dc, of);
    for f in [f_txt, f_glyph, f_small] { DeleteObject(f); }
    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteDC(dc);
    if BUSY.get() { SetTimer(hwnd, 4, 16, None); } else { KillTimer(hwnd, 4); }
    EndPaint(hwnd, &ps);
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 { r as u32 | (g as u32) << 8 | (b as u32) << 16 }

/// Background of the address box.
pub fn pill_brush() -> isize {
    if BRUSH.get() == 0 { BRUSH.set(unsafe { CreateSolidBrush(pill()) } as isize); }
    BRUSH.get()
}

/// The status and the filter changed: the floating bars over the list repaint (the status one also takes the width
/// its text needs).
pub unsafe fn status_dirty(_h: HWND) {
    let (f, s) = (FILTERWND.get() as HWND, STATUSWND.get() as HWND);
    if !f.is_null() { InvalidateRect(f, ptr::null(), 0); }
    if s.is_null() { return; }
    let mut r: RECT = std::mem::zeroed();
    GetWindowRect(s, &mut r);
    let want = status_width();
    if r.right - r.left != want {
        let parent = GetParent(s);
        let mut cr: RECT = std::mem::zeroed();
        GetClientRect(parent, &mut cr);
        MoveWindow(s, cr.right - want, cr.bottom - sc(BAR_H), want, sc(BAR_H), 1);
    }
    InvalidateRect(s, ptr::null(), 0);
}

// ---------- chrome window ----------

pub unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
    let layout_now = || {
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(h, &mut r);
        layout(r.right, ST.with(|c| c.try_borrow().map_or(1, |s| s.tabs.len())))
    };
    match m {
        WM_PAINT => paint_chrome(h),
        WM_ERASEBKGND => return 1,
        // the title bar's empty parts and its top edge belong to the window (drag, double-click, resize)
        WM_NCHITTEST => {
            let mut p = POINT { x, y };
            ScreenToClient(h, &mut p);
            if p.y < sc(TAB_H) && (hit(&layout_now(), p.x, p.y) == NONE || (p.y < sc(5) && IsZoomed(GetParent(h)) == 0)) { return HTTRANSPARENT as isize; }
            return HTCLIENT as isize;
        }
        WM_SIZE => { place_edit(h); InvalidateRect(h, ptr::null(), 0); } // the pill and the tabs re-flow: repaint it all
        WM_TIMER if wp == 4 => { InvalidateRect(h, ptr::null(), 0); }
        WM_MOUSEMOVE => {
            let k = match hit(&layout_now(), x, y) { NONE => crumb_at(x, y).map_or(NONE, |i| CRUMB + i as i32), k => k };
            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
            if HOT.replace(k) != k { InvalidateRect(h, ptr::null(), 0); }
        }
        WM_MOUSELEAVE => {
            HOT.set(NONE);
            InvalidateRect(h, ptr::null(), 0);
        }
        WM_LBUTTONDOWN | WM_MBUTTONDOWN => {
            let l = layout_now();
            let k = hit(&l, x, y);
            if k == NONE {
                if let Some(i) = crumb_at(x, y) {
                    PRESS.set((CRUMB + i as i32, now()));
                    PostMessageW(GetParent(h), WM_UI, 21, i as isize); // a path part: go there
                } else if inside(&l.pill, x, y) {
                    // the rest of the pill: type a path
                    let edit = ST.with(|c| c.try_borrow().map_or(ptr::null_mut(), |s| s.edit));
                    SetFocus(edit);
                    SendMessageW(edit, EM_SETSEL, 0, -1);
                }
                InvalidateRect(h, ptr::null(), 0);
                return 0;
            }
            PRESS.set((k, now()));
            InvalidateRect(h, ptr::null(), 0);
            let parent = GetParent(h);
            match k {
                MINB => { PostMessageW(parent, WM_SYSCOMMAND, SC_MINIMIZE as usize, 0); return 0; }
                MAXB => { PostMessageW(parent, WM_SYSCOMMAND, if IsZoomed(parent) != 0 { SC_RESTORE } else { SC_MAXIMIZE } as usize, 0); return 0; }
                XCLOSE => { PostMessageW(parent, WM_SYSCOMMAND, SC_CLOSE as usize, 0); return 0; }
                _ => {}
            }
            let (act, idx) = match k {
                BACK => (4, 0), FWD => (5, 0), UP => (6, 0), RELOAD => (7, 0), SEARCH => (9, 0), PLUS => (3, 0), GEAR => (10, 0), STAR => (11, 0),
                k if k >= CLOSE => (2, (k - CLOSE) as isize),
                k => (if m == WM_MBUTTONDOWN { 2 } else { 1 }, (k - TAB) as isize), // middle click on a tab closes it
            };
            PostMessageW(parent, WM_UI, act, idx);
        }
        WM_COMMAND if (wp >> 16) == 0x0100 || (wp >> 16) == 0x0200 => {
            // edit got / lost focus: shown only while typing (the path parts otherwise), outline colour
            ShowWindow(lp as HWND, if (wp >> 16) == 0x0100 { SW_SHOW } else { SW_HIDE });
            if (wp >> 16) == 0x0200 {
                // left without Enter: what was typed goes, the path parts show the folder again
                let dir = ST.with(|c| c.try_borrow().map(|s| s.dir.clone()).unwrap_or_default());
                SetWindowTextW(lp as HWND, w(&dir.to_string_lossy()).as_ptr());
            }
            InvalidateRect(h, ptr::null(), 0);
        }
        WM_CTLCOLOREDIT => {
            // the address box: white text on the pill's colour
            let dc = wp as HDC;
            SetTextColor(dc, pal().text);
            SetBkColor(dc, pill());
            return pill_brush();
        }
        _ => return DefWindowProcW(h, m, wp, lp),
    }
    0
}

// ---------- floating over the list: the filter (bottom left), status and view (bottom right), the zoom slider ----------

pub const BAR_H: i32 = 38;
const FILTER_W: i32 = 320;
const POP_W: i32 = 200;
const POP_H: i32 = 300;

thread_local! {
    static FILTERWND: Cell<isize> = const { Cell::new(0) };
    static STATUSWND: Cell<isize> = const { Cell::new(0) };
    static POPWND: Cell<isize> = const { Cell::new(0) };
    static BARBRUSH: Cell<isize> = const { Cell::new(0) };
    static BARHOT: Cell<i32> = const { Cell::new(-1) };
}

/// The three floating windows (children of the main window, above the list).
pub fn overlays_set(filter: HWND, status: HWND, pop: HWND) {
    FILTERWND.set(filter as isize);
    STATUSWND.set(status as isize);
    POPWND.set(pop as isize);
}

/// Places them over the list: `x0` is where the list starts (after the side pane), `cw` x `ch` the window's client.
unsafe fn place_overlays(x0: i32, cw: i32, ch: i32) {
    let (f, s, p) = (FILTERWND.get() as HWND, STATUSWND.get() as HWND, POPWND.get() as HWND);
    if f.is_null() { return; }
    let fw = sc(FILTER_W).min((cw - x0) / 2);
    SetWindowPos(f, HWND_TOP, x0, ch - sc(BAR_H), fw, sc(BAR_H), SWP_NOACTIVATE);
    let e = FILT.with(|x| x.try_borrow().map_or(0, |x| x.edit)) as HWND;
    if !e.is_null() { MoveWindow(e, sc(36), (sc(BAR_H) - sc(20)) / 2 + sc(1), (fw - sc(36) - sc(48)).max(0), sc(20), 1); }
    let sw = status_width();
    SetWindowPos(s, HWND_TOP, cw - sw, ch - sc(BAR_H), sw, sc(BAR_H), SWP_NOACTIVATE);
    SetWindowPos(p, HWND_TOP, cw - sc(POP_W) - sc(10), ch - sc(BAR_H) - sc(POP_H) - sc(8), sc(POP_W), sc(POP_H), SWP_NOACTIVATE);
}

/// Background of the filter box (the bar's colour).
fn bar_brush() -> isize {
    if BARBRUSH.get() == 0 { BARBRUSH.set(unsafe { CreateSolidBrush(pal().bg) } as isize); }
    BARBRUSH.get()
}

/// Draws a floating bar: list colour around it, the bar flush with the list's bottom and one side, the inner top corner
/// rounded, a hairline on its two open edges (accent while `lit`).
unsafe fn bar_shape(dc: HDC, cr: RECT, left_side: bool, lit: bool) {
    let b = CreateSolidBrush(listbg());
    FillRect(dc, &cr, b);
    DeleteObject(b);
    let rad = sc(24);
    let (out, inn) = if left_side {
        (rc(-rad, 0, cr.right, cr.bottom + rad), rc(-rad, 1, cr.right - 1, cr.bottom + rad))
    } else {
        (rc(0, 0, cr.right + rad, cr.bottom + rad), rc(1, 1, cr.right + rad, cr.bottom + rad))
    };
    fill(dc, out, if lit { pal().accent } else { pal().line }, rad);
    fill(dc, inn, pal().bg, rad - 2);
}

unsafe fn sub_button(cr: RECT) -> RECT { rc(cr.right - sc(42), sc(5), cr.right - sc(8), cr.bottom - sc(4)) }

unsafe fn paint_filter(h: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(h, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right.max(1), cr.bottom.max(1)));
    let old = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT as i32);
    let (e, sub) = FILT.with(|f| f.try_borrow().map_or((0, false), |f| (f.edit, f.sub)));
    let focused = e != 0 && GetFocus() == e as HWND;
    bar_shape(dc, cr, true, focused);
    let g = font(14, 400, winrust_common::style::icon_face()) as HFONT;
    let of = SelectObject(dc, g);
    text(dc, "\u{E721}", rc(sc(8), 0, sc(32), cr.bottom), if focused { pal().accent } else { pal().dim }, DT_CENTER);
    // the only thing next to the box: "Subfolders" as a glyph, accent while on
    let b = sub_button(cr);
    let hot = BARHOT.get() == 1;
    if sub { fill(dc, b, mix(pal().bg, pal().accent, 0.25), sc(10)); } else if hot { fill(dc, b, mix(pal().bg, pal().text, 0.08), sc(10)); }
    text(dc, "\u{E8B7}", b, if sub { pal().accent } else if hot { pal().text } else { pal().dim }, DT_CENTER);
    SelectObject(dc, of);
    DeleteObject(g);
    BitBlt(wdc, 0, 0, cr.right, cr.bottom, dc, 0, 0, SRCCOPY);
    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteDC(dc);
    EndPaint(h, &ps);
}

/// The filter's floating bar: its box (a child edit), the magnifier and the "Subfolders" glyph. Notifications of the
/// box go on to the main window.
pub unsafe extern "system" fn filter_bar_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
    match m {
        WM_PAINT => { paint_filter(h); 0 }
        WM_ERASEBKGND => 1,
        WM_COMMAND => SendMessageW(GetParent(h), m, wp, lp),
        WM_CTLCOLOREDIT => {
            let dc = wp as HDC;
            SetTextColor(dc, pal().text);
            SetBkColor(dc, pal().bg);
            bar_brush()
        }
        WM_MOUSEMOVE => {
            let mut cr: RECT = std::mem::zeroed();
            GetClientRect(h, &mut cr);
            let k = if inside(&sub_button(cr), x, y) { 1 } else { -1 };
            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
            if BARHOT.replace(k) != k { InvalidateRect(h, ptr::null(), 0); }
            0
        }
        WM_MOUSELEAVE => { BARHOT.set(-1); InvalidateRect(h, ptr::null(), 0); 0 }
        WM_LBUTTONDOWN => {
            let mut cr: RECT = std::mem::zeroed();
            GetClientRect(h, &mut cr);
            if inside(&sub_button(cr), x, y) { PostMessageW(GetParent(h), WM_UI, 27, 0); } else { filter_focus(None); }
            0
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

/// The glyph of a view (status corner and zoom slider).
fn view_glyph(name: &str) -> &'static str {
    match name { "Details" => "\u{E8FD}", "Tree" => "\u{E8B7}", "List" => "\u{E80A}", _ => "\u{E8B9}" }
}

/// Percent of the zoom range, like File Pilot's corner ("Details 0%" ... "XL Icons 100%").
fn zoom_pct() -> i32 { let (i, n) = zoom_at(); (i * 100 / (n - 1).max(1)) as i32 }

/// Width the status corner needs for its text.
unsafe fn status_width() -> i32 {
    let dc = GetDC(ptr::null_mut());
    let f = font(12, 400, ui_face()) as HFONT;
    let of = SelectObject(dc, f);
    let mw = |s: &str| { let mut r = rc(0, 0, 0, 0); DrawTextW(dc, w(s).as_ptr(), -1, &mut r, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX); r.right };
    let width = sc(16) + mw(&status_text()) + sc(14) + sc(1) + sc(12) + sc(22) + mw(view_name()) + sc(10) + sc(54) + sc(8);
    SelectObject(dc, of);
    DeleteObject(f);
    ReleaseDC(ptr::null_mut(), dc);
    width
}

unsafe fn paint_status_bar(h: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(h, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right.max(1), cr.bottom.max(1)));
    let old = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT as i32);
    bar_shape(dc, cr, false, false);
    let (f, g) = (font(12, 400, ui_face()) as HFONT, font(13, 400, winrust_common::style::icon_face()) as HFONT);
    let of = SelectObject(dc, f);
    // right to left: the zoom in percent, the view's name and glyph, a hairline, the counts
    let pct = rc(cr.right - sc(8) - sc(54), sc(7), cr.right - sc(8), cr.bottom - sc(6));
    fill(dc, pct, mix(pal().bg, pal().accent, 0.3), sc(8));
    fill(dc, rc(pct.left, pct.top, pct.left + sc(3), pct.bottom), pal().accent, sc(3));
    text(dc, &format!("{}%", zoom_pct()), pct, pal().text, DT_CENTER);
    let vn = view_name();
    let mw = |s: &str| { let mut r = rc(0, 0, 0, 0); DrawTextW(dc, w(s).as_ptr(), -1, &mut r, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX); r.right };
    let vx = pct.left - sc(10) - mw(vn);
    text(dc, vn, rc(vx, 0, pct.left - sc(10), cr.bottom), pal().text, DT_LEFT);
    SelectObject(dc, g);
    text(dc, view_glyph(vn), rc(vx - sc(24), 0, vx - sc(4), cr.bottom), pal().dim, DT_CENTER);
    fill(dc, rc(vx - sc(36), sc(11), vx - sc(35), cr.bottom - sc(10)), pal().line, 0);
    SelectObject(dc, f);
    text(dc, &status_text(), rc(sc(16), 0, vx - sc(48), cr.bottom), pal().dim, DT_RIGHT);
    SelectObject(dc, of);
    DeleteObject(f);
    DeleteObject(g);
    BitBlt(wdc, 0, 0, cr.right, cr.bottom, dc, 0, 0, SRCCOPY);
    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteDC(dc);
    EndPaint(h, &ps);
}

/// The status corner: counts, the view and the zoom; a click shows the zoom slider.
pub unsafe extern "system" fn status_bar_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match m {
        WM_PAINT => { paint_status_bar(h); 0 }
        WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN => { zoom_pop_show(); 0 }
        WM_MOUSEWHEEL => SendMessageW(POPWND.get() as HWND, m, wp, lp),
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

// the zoom slider: where each view sits on it (zoom step), bottom to top
const POP_MARKS: [(&str, usize); 6] = [("Details", 0), ("Tree", 1), ("List", 2), ("M Icons", 3), ("L Icons", 11), ("XL Icons", 18)];

/// y of zoom step `k` on the slider: the first steps (one view each) evenly at the bottom, the picture sizes above.
fn pop_y(k: usize, n: usize, top: i32, bottom: i32) -> i32 {
    let row = sc(30);
    if k <= 3 { return bottom - k as i32 * row; }
    let span = (bottom - 3 * row - top) as f32;
    bottom - 3 * row - (span * (k - 3) as f32 / (n - 1 - 3).max(1) as f32) as i32
}

/// Shows the zoom slider for a moment (it goes again 1.6 s after the last change, unless the pointer is on it).
pub fn zoom_pop_show() {
    let p = POPWND.get() as HWND;
    if p.is_null() { return; }
    unsafe {
        if IsWindowVisible(p) == 0 { SetWindowPos(p, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW); }
        InvalidateRect(p, ptr::null(), 0);
        SetTimer(p, 1, 1600, None);
    }
}

unsafe fn paint_pop(h: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(h, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right.max(1), cr.bottom.max(1)));
    let old = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT as i32);
    let b = CreateSolidBrush(listbg());
    FillRect(dc, &cr, b);
    DeleteObject(b);
    fill(dc, cr, pal().line, sc(16));
    fill(dc, rc(1, 1, cr.right - 1, cr.bottom - 1), pal().bg, sc(14));
    let (i, n) = zoom_at();
    let (top, bottom) = (sc(22), cr.bottom - sc(22));
    // the track on the right, the knob at the current step
    let tx = cr.right - sc(26);
    fill(dc, rc(tx - sc(3), top, tx + sc(3), bottom), mix(pal().bg, pal().text, 0.12), sc(6));
    let ky = pop_y(i, n, top, bottom);
    fill(dc, rc(tx - sc(3), ky, tx + sc(3), bottom), mix(pal().bg, pal().accent, 0.5), sc(6));
    fill(dc, rc(tx - sc(9), ky - sc(9), tx + sc(9), ky + sc(9)), pal().accent, sc(18));
    let (f, g) = (font(13, 400, ui_face()) as HFONT, font(13, 400, winrust_common::style::icon_face()) as HFONT);
    let of = SelectObject(dc, f);
    let cur = view_name();
    for (name, k) in POP_MARKS {
        let y = pop_y(k, n, top, bottom);
        let r = rc(sc(10), y - sc(13), tx - sc(18), y + sc(13));
        let on = name == cur;
        if on { fill(dc, r, mix(pal().bg, pal().accent, 0.35), sc(8)); }
        SelectObject(dc, g);
        text(dc, view_glyph(name), rc(r.left + sc(6), r.top, r.left + sc(28), r.bottom), if on { pal().text } else { pal().dim }, DT_CENTER);
        SelectObject(dc, f);
        text(dc, name, rc(r.left + sc(34), r.top, r.right - sc(6), r.bottom), if on { pal().text } else { mix(pal().dim, pal().text, 0.4) }, DT_LEFT);
    }
    SelectObject(dc, of);
    DeleteObject(f);
    DeleteObject(g);
    BitBlt(wdc, 0, 0, cr.right, cr.bottom, dc, 0, 0, SRCCOPY);
    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteDC(dc);
    EndPaint(h, &ps);
}

/// The zoom slider: a click on a view (or on the track) goes there; the wheel walks the steps.
pub unsafe extern "system" fn zoom_pop_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match m {
        WM_PAINT => { paint_pop(h); 0 }
        WM_ERASEBKGND => 1,
        WM_TIMER if wp == 1 => {
            let mut p = POINT { x: 0, y: 0 };
            GetCursorPos(&mut p);
            if WindowFromPoint(p) != h { KillTimer(h, 1); ShowWindow(h, SW_HIDE); }
            0
        }
        WM_LBUTTONDOWN => {
            let y = ((lp >> 16) & 0xFFFF) as i16 as i32;
            let mut cr: RECT = std::mem::zeroed();
            GetClientRect(h, &mut cr);
            let (_, n) = zoom_at();
            let (top, bottom) = (sc(22), cr.bottom - sc(22));
            // the nearest step to the click; a click on a view's name lands on that view
            let k = (0..n).min_by_key(|&k| (pop_y(k, n, top, bottom) - y).abs()).unwrap_or(0);
            let k = POP_MARKS.iter().find(|(_, mk)| (pop_y(*mk, n, top, bottom) - y).abs() < sc(13)).map_or(k, |(_, mk)| *mk);
            PostMessageW(GetParent(h), WM_UI, 28, k as isize);
            zoom_pop_show();
            0
        }
        WM_MOUSEWHEEL => {
            let d = ((wp >> 16) & 0xFFFF) as i16 as i32;
            if d != 0 { PostMessageW(GetParent(h), WM_UI, 25, d.signum() as isize); }
            zoom_pop_show();
            0
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

// ---------- side pane (left): recents, bookmarks, storage, places ----------

pub const NAV_W: i32 = 240;
const NAV_ROW: i32 = 32;
const NAV_HEAD: i32 = 34;
const NAV_DRIVE: i32 = 50; // a drive row also shows how full it is
const NAV_HINT: i32 = 28;

pub const SEC_RECENT: u8 = 0;
pub const SEC_MARKS: u8 = 1;
pub const SEC_DRIVES: u8 = 2;
pub const SEC_PLACES: u8 = 3;
const SECTIONS: [(&str, &str); 4] = [("Recents", "\u{E81C}"), ("Bookmarks", "\u{E734}"), ("Storage", "\u{EDA2}"), ("Places", "\u{E8B7}")];
const RECENTS_SHOWN: usize = 5; // older ones drop off: the section stays small

/// A row of the pane: a section heading (`head`), a hint line (empty path, not clickable) or a place with its system icon.
#[derive(Clone)]
struct NavItem { title: String, path: PathBuf, icon: i32, cap: Option<(u64, u64)>, sec: u8, head: bool } // cap = (free, total) bytes of a drive

fn nav_h(it: &NavItem) -> i32 {
    sc(if it.head { NAV_HEAD } else if it.path.as_os_str().is_empty() { NAV_HINT } else if it.cap.is_some() { NAV_DRIVE } else { NAV_ROW })
}

thread_local! {
    static NAV: RefCell<Vec<NavItem>> = const { RefCell::new(Vec::new()) };
    static FIXED: RefCell<Vec<NavItem>> = const { RefCell::new(Vec::new()) }; // drives and places: read once
    static SHINFO: RefCell<HashMap<PathBuf, (i32, String)>> = RefCell::new(HashMap::new());
    static NAVWND: Cell<isize> = const { Cell::new(0) };
    static NAVHOT: Cell<i32> = const { Cell::new(-1) };
    static NAVTOP: Cell<i32> = const { Cell::new(0) }; // scrolled by this many pixels
    static NAVGOAL: Cell<i32> = const { Cell::new(0) }; // where the wheel is taking it
    static NAVFONTS: Cell<[isize; 4]> = const { Cell::new([0; 4]) }; // text, heading, glyph, small: made once
    static SMALL_IL: Cell<isize> = const { Cell::new(0) }; // the system's small icons
}

/// System icon and display name of a place ("Downloads", "Local Disk (C:)"), asked once per path. `real` = ask the file
/// system (the folder's own icon); otherwise by kind only, which never touches the disk.
fn shell_info(p: &Path, real: bool) -> (i32, String) {
    if let Some(v) = SHINFO.with_borrow(|m| m.get(p).cloned()) { return v; }
    let mut fi: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let wp = w(&p.to_string_lossy());
    let base = SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_DISPLAYNAME;
    let mut ok = real && unsafe { SHGetFileInfoW(wp.as_ptr(), 0, &mut fi, size_of::<SHFILEINFOW>() as u32, base) } != 0;
    if !ok {
        // gone, or not worth asking: by kind (a name without an extension counts as a folder)
        let attr = if p.extension().is_none() { 0x10 } else { 0x80 };
        ok = unsafe { SHGetFileInfoW(wp.as_ptr(), attr, &mut fi, size_of::<SHFILEINFOW>() as u32, base | SHGFI_USEFILEATTRIBUTES) } != 0;
    }
    let mut name = if ok { wstr(fi.szDisplayName.as_ptr()) } else { String::new() };
    if name.is_empty() { name = p.file_name().map_or_else(|| p.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned()); }
    let v = (if ok { fi.iIcon } else { -1 }, name);
    SHINFO.with_borrow_mut(|m| m.insert(p.to_path_buf(), v.clone()));
    v
}

pub fn nav_init(h: HWND) {
    NAVWND.set(h as isize);
    unsafe {
        let mut fi: SHFILEINFOW = std::mem::zeroed();
        SMALL_IL.set(SHGetFileInfoW(w("C:\\").as_ptr(), 0, &mut fi, size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_SMALLICON) as isize);
    }
    let mut v = vec![];
    let place = |p: PathBuf, sec: u8| { let (icon, title) = shell_info(&p, true); NavItem { title, path: p, icon, cap: None, sec, head: false } };
    unsafe {
        use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
        let mask = GetLogicalDrives();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 { continue; }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let ty = GetDriveTypeW(w(&root).as_ptr());
            if ty == 4 {
                // a mapped network drive: no label, size or real icon, they would stall the window while a share is down
                let icon = shell_info(Path::new(&root), false).0;
                v.push(NavItem { title: format!("Network Drive ({letter}:)"), path: PathBuf::from(root), icon, cap: None, sec: SEC_DRIVES, head: false });
                continue;
            }
            if !matches!(ty, 2 | 3) { continue; } // fixed and removable disks only (no empty card readers' CD drives)
            let mut label = [0u16; 64];
            let ok = GetVolumeInformationW(w(&root).as_ptr(), label.as_mut_ptr(), 64, ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), 0);
            let name = if ok != 0 { wstr(label.as_ptr()) } else { String::new() };
            let (mut free, mut total) = (0u64, 0u64);
            let cap = (GetDiskFreeSpaceExW(w(&root).as_ptr(), &mut free, &mut total, ptr::null_mut()) != 0 && total > 0).then_some((free, total));
            let icon = shell_info(Path::new(&root), ok != 0).0;
            v.push(NavItem { title: format!("{} ({letter}:)", if name.is_empty() { "Local Disk" } else { &name }), path: PathBuf::from(root), icon, cap, sec: SEC_DRIVES, head: false });
        }
    }
    for p in sh::places() { v.push(place(p, SEC_PLACES)); }
    FIXED.set(v);
    nav_rebuild();
}

/// Lays the pane out again from the remembered places and the settings (sections folded away, recents on or off).
pub fn nav_rebuild() {
    if NAVWND.get() == 0 { return; }
    let p = prefs_now();
    let recent: Vec<PathBuf> = RECENT.with_borrow(|r| r.iter().take(RECENTS_SHOWN).cloned().collect());
    let marks: Vec<PathBuf> = MARKS.with_borrow(|m| m.clone());
    let fixed = FIXED.with_borrow(|f| f.clone());
    let mut v = vec![];
    for sec in [SEC_RECENT, SEC_MARKS, SEC_DRIVES, SEC_PLACES] {
        if sec == SEC_RECENT && !p.recents { continue; }
        v.push(NavItem { title: SECTIONS[sec as usize].0.into(), path: PathBuf::new(), icon: -1, cap: None, sec, head: true });
        if p.collapsed.contains(&sec) { continue; }
        let mine = |list: &[PathBuf]| list.iter().map(|x| {
            // recent files may be on a slow or gone disk: asked by kind; folders and bookmarks for real
            let real = sec == SEC_MARKS || x.extension().is_none();
            let (icon, title) = shell_info(x, real);
            NavItem { title, path: x.clone(), icon, cap: None, sec, head: false }
        }).collect::<Vec<_>>();
        match sec {
            SEC_RECENT => v.extend(mine(&recent)),
            SEC_MARKS if marks.is_empty() => v.push(NavItem { title: "Ctrl+D or \u{2606} adds the folder".into(), path: PathBuf::new(), icon: -1, cap: None, sec, head: false }),
            SEC_MARKS => v.extend(mine(&marks)),
            _ => v.extend(fixed.iter().filter(|i| i.sec == sec).cloned()),
        }
    }
    NAV.set(v);
    clamp_scroll();
    nav_dirty();
}

/// Path and section of pane row `i` (None for headings and hints).
pub fn nav_item(i: usize) -> Option<(PathBuf, u8)> {
    NAV.with_borrow(|n| n.get(i).filter(|x| !x.head && !x.path.as_os_str().is_empty()).map(|x| (x.path.clone(), x.sec)))
}

pub fn nav_dirty() {
    let h = NAVWND.get() as HWND;
    if !h.is_null() { unsafe { InvalidateRect(h, ptr::null(), 0) }; }
}

fn content_h() -> i32 { NAV.with_borrow(|n| n.iter().map(nav_h).sum::<i32>()) + sc(16) }

fn clamp_scroll() {
    let h = NAVWND.get() as HWND;
    let mut cr: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(h, &mut cr) };
    let max = (content_h() - cr.bottom).max(0);
    NAVTOP.set(NAVTOP.get().clamp(0, max));
    NAVGOAL.set(NAVGOAL.get().clamp(0, max));
}

/// Chrome on top, the pane on the left (hidden when the window is narrow), the list takes the rest down to the bottom
/// edge, the filter and the status float over its bottom corners.
pub unsafe fn layout_children(cw: i32, ch: i32) {
    let (chrome, list) = ST.with(|c| c.try_borrow().map_or((ptr::null_mut(), ptr::null_mut()), |s| (s.tab, s.list)));
    let nav = NAVWND.get() as HWND;
    let top = sc(CHROME_H);
    let nw = if cw > sc(760) { sc(NAV_W) } else { 0 };
    let h = (ch - top).max(0);
    MoveWindow(chrome, 0, 0, cw, top, 1);
    MoveWindow(nav, 0, top, nw, h, 1);
    MoveWindow(list, nw, top, cw - nw, h, 1);
    fit_columns(list);
    place_overlays(nw, cw, ch);
    clamp_scroll();
}

/// Row under y: Some((index, is a heading)); None over hints or empty space.
fn nav_hit(y: i32) -> Option<(usize, bool)> {
    let mut top = sc(8) - NAVTOP.get();
    NAV.with_borrow(|n| {
        for (i, it) in n.iter().enumerate() {
            let hgt = nav_h(it);
            if y >= top && y < top + hgt { return (it.head || !it.path.as_os_str().is_empty()).then_some((i, it.head)); }
            top += hgt;
        }
        None
    })
}

unsafe fn paint_nav(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(hwnd, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut cr);
    BUSY.set(false);
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right.max(1), cr.bottom.max(1)));
    let old = SelectObject(dc, bmp);
    let bg = CreateSolidBrush(pal().bg);
    FillRect(dc, &cr, bg);
    DeleteObject(bg);
    SetBkMode(dc, TRANSPARENT as i32);
    if NAVFONTS.get()[0] == 0 { NAVFONTS.set([font(13, 400, ui_face()), font(13, 600, ui_face()), font(13, 400, winrust_common::style::icon_face()), font(11, 400, ui_face())]); }
    let [f_txt, f_head, f_glyph, f_small] = NAVFONTS.get().map(|f| f as HFONT);
    let of = SelectObject(dc, f_txt);
    // where we are = the deepest bookmark / drive / place that contains the current folder (recents would always match)
    let cur = ST.with(|c| c.try_borrow().ok().map(|s| PathBuf::from(s.dir.to_string_lossy().to_lowercase()))).unwrap_or_default();
    let hot = NAVHOT.get();
    let il = SMALL_IL.get();
    let (mut iw, mut ih) = (0, 0);
    ImageList_GetIconSize(il as _, &mut iw, &mut ih);
    let folded = prefs_now().collapsed;
    NAV.with_borrow(|n| {
        let here = n.iter().enumerate()
            .filter(|(_, it)| !it.head && it.sec != SEC_RECENT && !it.path.as_os_str().is_empty() && cur.starts_with(PathBuf::from(it.path.to_string_lossy().to_lowercase())))
            .max_by_key(|(_, it)| it.path.as_os_str().len()).map(|(i, _)| i);
        let mut y = sc(8) - NAVTOP.get();
        for (i, it) in n.iter().enumerate() {
            let hgt = nav_h(it);
            if y > cr.bottom { break; }
            if y + hgt < 0 { y += hgt; continue; }
            let r = rc(sc(8), y, cr.right - sc(8), y + hgt);
            if it.head {
                // heading: glyph, name, a chevron that turns when the section is folded
                let th = hv(5000 + i as i32, hot == i as i32);
                if th > 0.01 { fill(dc, rc(r.left, y + sc(4), r.right, y + hgt), mix(pal().bg, pal().hover, th * 0.7), sc(10)); }
                SelectObject(dc, f_glyph);
                text(dc, SECTIONS[it.sec as usize].1, rc(r.left + sc(8), y + sc(4), r.left + sc(30), y + hgt), pal().dim, DT_CENTER);
                SelectObject(dc, f_head);
                text(dc, &it.title, rc(r.left + sc(36), y + sc(4), r.right - sc(30), y + hgt), mix(pal().dim, pal().text, 0.35 + 0.65 * th), DT_LEFT);
                SelectObject(dc, f_glyph);
                let chev = if folded.contains(&it.sec) { "\u{E76C}" } else { "\u{E70D}" };
                text(dc, chev, rc(r.right - sc(28), y + sc(4), r.right - sc(6), y + hgt), pal().dim, DT_CENTER);
                y += hgt;
                continue;
            }
            if it.path.as_os_str().is_empty() {
                // a hint (no bookmarks yet)
                SelectObject(dc, f_small);
                text(dc, &it.title, rc(r.left + sc(14), y, r.right - sc(8), y + hgt), off(), DT_LEFT);
                y += hgt;
                continue;
            }
            let (th, ta) = (hv(5000 + i as i32, hot == i as i32), hv(6000 + i as i32, here == Some(i)));
            let p = press_t(5000 + i as i32);
            if th.max(ta) > 0.01 || p > 0.0 { fill(dc, r, mix(mix(mix(pal().bg, pal().hover, th), pal().active, ta), pal().accent, 0.3 * p), sc(10)); }
            if ta > 0.01 { fill(dc, rc(r.left, y + sc(9), r.left + sc(3), y + sc(NAV_ROW) - sc(9)), mix(pal().active, pal().accent, ta), sc(3)); }
            let line = rc(r.left, y, r.right, y + sc(NAV_ROW)); // the title line (a drive has its usage bar below it)
            if it.icon >= 0 { ImageList_Draw(il as _, it.icon, dc, r.left + sc(14), (line.top + line.bottom - ih) / 2, ILD_TRANSPARENT); }
            SelectObject(dc, f_txt);
            text(dc, &it.title, rc(r.left + sc(22) + iw, line.top, r.right - sc(8), line.bottom), mix(pal().dim, pal().text, 0.55 + 0.45 * ta.max(th)), DT_LEFT);
            if let Some((free, total)) = it.cap {
                // usage bar like Windows' "This PC": accent, red once the drive is more than 90 % full
                let used = 1.0 - free as f32 / total as f32;
                let bar = rc(r.left + sc(22) + iw, y + sc(28), r.right - sc(14), y + sc(32));
                fill(dc, bar, mix(pal().bg, pal().text, 0.12), sc(4));
                let fw = ((bar.right - bar.left) as f32 * used) as i32;
                fill(dc, rc(bar.left, bar.top, bar.left + fw.max(sc(4)), bar.bottom), if used > 0.9 { winrust_common::style::BAD } else { pal().accent }, sc(4));
                SelectObject(dc, f_small);
                let gb = |b: u64| b as f64 / 1_073_741_824.0;
                text(dc, &format!("{:.0} GB free of {:.0} GB", gb(free), gb(total)), rc(bar.left, bar.bottom + sc(1), r.right - sc(8), y + hgt - sc(2)), pal().dim, DT_LEFT);
            }
            y += hgt;
        }
    });
    fill(dc, rc(cr.right - 1, 0, cr.right, cr.bottom), pal().line, 0); // hairline between the pane and the list
    BitBlt(wdc, 0, 0, cr.right, cr.bottom, dc, 0, 0, SRCCOPY);
    SelectObject(dc, of);
    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteDC(dc);
    if BUSY.get() { SetTimer(hwnd, 4, 16, None); } else { KillTimer(hwnd, 4); }
    EndPaint(hwnd, &ps);
}

pub unsafe extern "system" fn nav_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let y = ((lp >> 16) & 0xFFFF) as i16 as i32;
    match m {
        WM_PAINT => paint_nav(h),
        WM_ERASEBKGND => return 1,
        WM_SIZE => clamp_scroll(),
        WM_TIMER if wp == 4 => { InvalidateRect(h, ptr::null(), 0); }
        WM_MOUSEMOVE => {
            let k = nav_hit(y).map_or(-1, |(i, _)| i as i32);
            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
            if NAVHOT.replace(k) != k { InvalidateRect(h, ptr::null(), 0); }
        }
        WM_MOUSELEAVE => { NAVHOT.set(-1); InvalidateRect(h, ptr::null(), 0); }
        WM_MOUSEWHEEL => {
            // the wheel sets a goal; timer 5 glides there
            let d = ((wp >> 16) & 0xFFFF) as i16 as i32;
            NAVGOAL.set(NAVGOAL.get() - d * sc(NAV_ROW) * 3 / 120);
            clamp_scroll();
            SetTimer(h, 5, 10, None);
        }
        WM_TIMER if wp == 5 => {
            let (top, goal) = (NAVTOP.get(), NAVGOAL.get());
            let step = ((goal - top) as f32 * 0.3) as i32;
            NAVTOP.set(if step == 0 { goal } else { top + step });
            if NAVTOP.get() == goal { KillTimer(h, 5); }
            InvalidateRect(h, ptr::null(), 0);
        }
        WM_LBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN => {
            if let Some((i, head)) = nav_hit(y) {
                PRESS.set((5000 + i as i32, now()));
                InvalidateRect(h, ptr::null(), 0);
                if head {
                    // a heading folds its section away (or back)
                    if m == WM_LBUTTONDOWN { let sec = NAV.with_borrow(|n| n[i].sec); PostMessageW(GetParent(h), WM_UI, 24, sec as isize); }
                } else {
                    PostMessageW(GetParent(h), WM_UI, match m { WM_LBUTTONDOWN => 20, WM_RBUTTONUP => 22, _ => 23 }, i as isize);
                }
            }
        }
        _ => return DefWindowProcW(h, m, wp, lp),
    }
    0
}
