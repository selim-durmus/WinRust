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
pub const STATUS_H: i32 = 46; // the bottom bar: filter box, "Subfolders", counts
pub const WM_UI: u32 = WM_APP + 4; // chrome -> window: wp = action (see Hit), lp = tab index
const WM_MOUSELEAVE: u32 = 0x02A3; // not exported under the enabled features

// what the pointer is over / what a click does
const NONE: i32 = -1;
const BACK: i32 = 1;
const FWD: i32 = 2;
const UP: i32 = 3;
const RELOAD: i32 = 4;
const SEARCH: i32 = 5;
const VIEW: i32 = 6;
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
        row_height(list, true);
        SetWindowSubclass(list, Some(list_sub), 1, 0); // paints the column header (the theme alone leaves it dim)
    }
}

pub const ROW_H: i32 = 36;

/// Details rows get ROW_H tall through an empty 1 px wide state image list (a row is as tall as its tallest image).
/// Off for the thumbnail view, where a state image would take room next to every thumbnail.
pub fn row_height(list: HWND, on: bool) {
    unsafe {
        let il = if on { ImageList_Create(1, sc(ROW_H), ILC_COLOR32, 1, 1) as isize } else { 0 };
        let old = SendMessageW(list, LVM_SETIMAGELIST, LVSIL_STATE as usize, il);
        if old != 0 { ImageList_Destroy(old as _); }
    }
}

thread_local! {
    static ROWHOT: Cell<isize> = const { Cell::new(-1) }; // list row under the pointer
    static FONTS: Cell<(isize, isize)> = const { Cell::new((0, 0)) }; // (name, details) fonts of the list rows
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

/// The details rows, painted by hand (NM_CUSTOMDRAW of the list): a rounded accent pill with a marker for the selection,
/// a soft one under the pointer, the name bright and the rest dimmed. The thumbnail view keeps the list's own drawing.
pub unsafe fn list_draw(lp: LPARAM) -> LRESULT {
    let cd = &*(lp as *const NMLVCUSTOMDRAW);
    let list = cd.nmcd.hdr.hwndFrom;
    if ST.with(|c| c.try_borrow().map_or(true, |s| s.thumbs)) { return CDRF_DODEFAULT as isize; }
    let dc = cd.nmcd.hdc;
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(list, &mut cr);
    match cd.nmcd.dwDrawStage {
        CDDS_PREPAINT => (CDRF_NOTIFYITEMDRAW | CDRF_NOTIFYPOSTPAINT) as isize,
        CDDS_POSTPAINT => {
            if SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) == 0 {
                // an empty folder says so instead of showing a blank list
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
                Some((r.name.clone(), r.dir, r.time, r.size, r.icon))
            });
            let Some((name, is_dir, time, size, icon)) = row else { return CDRF_DODEFAULT as isize };
            let mut r = RECT { left: LVIR_BOUNDS as i32, ..std::mem::zeroed() };
            SendMessageW(list, LVM_GETITEMRECT, i, &mut r as *mut _ as isize);
            let row_r = rc(0, r.top, cr.right, r.bottom);
            let b = CreateSolidBrush(listbg());
            FillRect(dc, &row_r, b);
            DeleteObject(b);
            let sel = SendMessageW(list, LVM_GETITEMSTATE, i, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0;
            let hot = ROWHOT.get() == i as isize;
            let focus = GetFocus() == list;
            let pill = rc(sc(6), r.top + sc(2), cr.right - sc(6), r.bottom - sc(2));
            let bg = match (sel, focus) {
                (true, true) => mix(listbg(), pal().accent, if hot { 0.26 } else { 0.20 }),
                (true, false) => mix(listbg(), pal().text, 0.10),
                _ if hot => mix(listbg(), pal().text, 0.055),
                _ => listbg(),
            };
            if bg != listbg() { fill(dc, pill, bg, sc(10)); }
            if sel { fill(dc, rc(pill.left, pill.top + sc(8), pill.left + sc(3), pill.bottom - sc(8)), if focus { pal().accent } else { pal().dim }, sc(3)); }
            // columns from the header widths (left of the row = minus the horizontal scroll)
            let cw: Vec<i32> = (0..4).map(|k| SendMessageW(list, LVM_GETCOLUMNWIDTH, k, 0) as i32).collect();
            let x: Vec<i32> = (0..4).map(|k| r.left + cw[..k].iter().sum::<i32>()).collect();
            let himl = SendMessageW(list, LVM_GETIMAGELIST, LVSIL_SMALL as usize, 0);
            let (mut iw, mut ih) = (0, 0);
            ImageList_GetIconSize(himl as _, &mut iw, &mut ih);
            if icon >= 0 { ImageList_Draw(himl as _, icon, dc, x[0] + sc(16), (r.top + r.bottom - ih) / 2, ILD_TRANSPARENT); }
            if FONTS.get().0 == 0 { FONTS.set((font(14, 400, ui_face()), font(13, 400, ui_face()))); }
            let (f_name, f_det) = FONTS.get();
            SetBkMode(dc, TRANSPARENT as i32);
            let of = SelectObject(dc, f_name as HFONT);
            let p = Path::new(&name);
            let hit_of_search = p.is_absolute(); // search hits are absolute paths: show the name, and where it lies in the type column
            let shown = if hit_of_search { p.file_name().map_or(name.clone(), |n| n.to_string_lossy().into_owned()) } else { name.clone() };
            text(dc, &shown, rc(x[0] + sc(16) + iw + sc(12), r.top, x[1] - sc(8), r.bottom), pal().text, DT_LEFT);
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
            if !is_dir { text(dc, &size_text(size), rc(x[3], r.top, x[3] + cw[3] - sc(14), r.bottom), dim, DT_RIGHT); }
            SelectObject(dc, of);
            CDRF_SKIPDEFAULT as isize
        }
        _ => CDRF_DODEFAULT as isize,
    }
}

/// Name column takes what the other three leave, so the list never needs a horizontal scrollbar.
pub unsafe fn fit_columns(list: HWND) {
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(list, &mut cr);
    let rest: i32 = (1..4).map(|k| SendMessageW(list, LVM_GETCOLUMNWIDTH, k, 0) as i32).sum();
    SendMessageW(list, LVM_SETCOLUMNWIDTH, 0, (cr.right - rest).max(sc(200)) as isize);
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
                    let align = if i == 3 { DT_RIGHT } else { DT_LEFT }; // the size column is right-aligned like its numbers
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

struct Layout { tabs: Vec<RECT>, close: Vec<RECT>, plus: RECT, back: RECT, fwd: RECT, up: RECT, reload: RECT, pill: RECT, star: RECT, search: RECT, view: RECT, gear: RECT }

pub(crate) fn rc(l: i32, t: i32, r: i32, b: i32) -> RECT { RECT { left: l, top: t, right: r, bottom: b } }

fn layout(cw: i32, n: usize) -> Layout {
    let (ty0, ty1) = (sc(8), sc(36));
    let tw = ((cw - sc(12) * 2 - sc(36)) / n.max(1) as i32 - sc(4)).clamp(sc(90), sc(220));
    let tabs: Vec<RECT> = (0..n as i32).map(|i| { let l = sc(12) + i * (tw + sc(4)); rc(l, ty0, l + tw, ty1) }).collect();
    let close = tabs.iter().map(|t| rc(t.right - sc(26), (t.top + t.bottom) / 2 - sc(10), t.right - sc(6), (t.top + t.bottom) / 2 + sc(10))).collect();
    let px = tabs.last().map_or(sc(12), |t| t.right + sc(4));
    let plus = rc(px, ty0 + sc(2), px + sc(26), ty1 - sc(2));
    let (by0, by1, bs) = (sc(44), sc(80), sc(36));
    let btn = |i: i32| rc(sc(12) + i * (bs + sc(4)), by0, sc(12) + i * (bs + sc(4)) + bs, by1);
    // right end: bookmark star, filter, view, settings
    let gear = rc(cw - sc(12) - bs, by0, cw - sc(12), by1);
    let view = rc(gear.left - sc(4) - bs, by0, gear.left - sc(4), by1);
    let search = rc(view.left - sc(4) - bs, by0, view.left - sc(4), by1);
    let star = rc(search.left - sc(4) - bs, by0, search.left - sc(4), by1);
    let reload = btn(3);
    let pill = rc(reload.right + sc(8), by0, star.left - sc(8), by1);
    Layout { tabs, close, plus, back: btn(0), fwd: btn(1), up: btn(2), reload, pill, star, search, view, gear }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool { x >= r.left && x < r.right && y >= r.top && y < r.bottom }

fn hit(l: &Layout, x: i32, y: i32) -> i32 {
    for (i, c) in l.close.iter().enumerate() { if inside(c, x, y) && l.tabs.len() > 1 { return CLOSE + i as i32; } }
    for (i, t) in l.tabs.iter().enumerate() { if inside(t, x, y) { return TAB + i as i32; } }
    for (k, r) in [(BACK, &l.back), (FWD, &l.fwd), (UP, &l.up), (RELOAD, &l.reload), (STAR, &l.star), (SEARCH, &l.search), (VIEW, &l.view), (GEAR, &l.gear), (PLUS, &l.plus)] {
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

unsafe fn paint_chrome(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(hwnd, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut cr);
    // everything painting needs from the window state, copied out so nothing is borrowed while drawing
    let snap = ST.with(|c| c.try_borrow().ok().map(|s| (s.tabs.iter().map(label).collect::<Vec<_>>(), s.active, s.hist.pos > 0, s.hist.pos + 1 < s.hist.v.len(), s.thumbs, s.edit, s.dir.clone())));
    let Some((labels, active, can_back, can_fwd, thumbs, edit, dir)) = snap else { EndPaint(hwnd, &ps); return };
    BUSY.set(false);
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right, cr.bottom));
    let old = SelectObject(dc, bmp);
    let bg = CreateSolidBrush(pal().bg);
    FillRect(dc, &cr, bg);
    DeleteObject(bg);
    SetBkMode(dc, TRANSPARENT as i32);
    let l = layout(cr.right, labels.len());
    let hot = HOT.get();
    let (f_txt, f_glyph, f_small) = (font(13, 400, ui_face()) as HFONT, font(16, 400, winrust_common::style::icon_face()) as HFONT, font(12, 400, winrust_common::style::icon_face()) as HFONT);
    let of = SelectObject(dc, f_txt);

    // tabs
    for (i, t) in l.tabs.iter().enumerate() {
        let on = i == active;
        let (th, ta) = (hv(TAB + i as i32, hot == TAB + i as i32 || hot == CLOSE + i as i32), hv(1000 + i as i32, on));
        let p = press_t(TAB + i as i32);
        fill(dc, *t, mix(mix(mix(pal().bg, pal().hover, th), pal().active, ta), pal().accent, 0.35 * p), sc(8));
        if ta > 0.01 { fill(dc, rc(t.left + (t.right - t.left) / 2 - sc(12), t.bottom - sc(3), t.left + (t.right - t.left) / 2 + sc(12), t.bottom), mix(pal().active, pal().accent, ta), sc(3)); }
        SelectObject(dc, f_txt);
        text(dc, &labels[i], rc(t.left + sc(14), t.top, t.right - sc(if labels.len() > 1 { 30 } else { 10 }), t.bottom), mix(pal().dim, pal().text, ta.max(th)), DT_LEFT);
        if labels.len() > 1 && (on || th > 0.05) {
            let c = l.close[i];
            glow(dc, CLOSE + i as i32, c, mix(pal().bg, pal().active, ta), pal().hover, sc(6), hot == CLOSE + i as i32);
            SelectObject(dc, f_small);
            text(dc, "\u{E711}", c, mix(pal().dim, pal().text, if hot == CLOSE + i as i32 { 1.0 } else { 0.4 }), DT_CENTER);
        }
    }
    glow(dc, PLUS, l.plus, pal().bg, pal().hover, sc(6), hot == PLUS);
    SelectObject(dc, f_small);
    text(dc, "\u{E710}", l.plus, pal().dim, DT_CENTER);

    // toolbar buttons
    SelectObject(dc, f_glyph);
    for (k, r, g, enabled) in [(BACK, l.back, "\u{E72B}", can_back), (FWD, l.fwd, "\u{E72A}", can_fwd), (UP, l.up, "\u{E74A}", true), (RELOAD, l.reload, "\u{E72C}", true), (SEARCH, l.search, "\u{E721}", true), (VIEW, l.view, if thumbs { "\u{E8FD}" } else { "\u{E80A}" }, true), (GEAR, l.gear, "\u{E713}", true)] {
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

/// The bottom bar of a window `cw` x `ch`: (filter box, "Subfolders" switch, status text).
fn bottom_rects(cw: i32, ch: i32) -> (RECT, RECT, RECT) {
    let top = ch - sc(STATUS_H);
    let (y0, y1) = (top + sc(8), ch - sc(8));
    let pw = (cw * 2 / 5).clamp(sc(220), sc(460));
    let pill = rc(sc(10), y0, sc(10) + pw, y1);
    let chip = rc(pill.right + sc(8), y0, pill.right + sc(8) + sc(124), y1);
    (pill, chip, rc(chip.right + sc(12), top, cw - sc(14), ch))
}

/// What a click in the main window's own area hit: 1 the "Subfolders" switch, 2 the filter box, 0 nothing.
pub unsafe fn bottom_hit(h: HWND, x: i32, y: i32) -> u8 {
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let (pill, chip, _) = bottom_rects(cr.right, cr.bottom);
    if inside(&chip, x, y) { 1 } else if inside(&pill, x, y) { 2 } else { 0 }
}

/// The filter box sits inside its pill, after the magnifier.
unsafe fn place_filter(cw: i32, ch: i32) {
    let e = FILT.with(|f| f.try_borrow().map_or(0, |f| f.edit)) as HWND;
    if e.is_null() { return; }
    let (p, _, _) = bottom_rects(cw, ch);
    let eh = sc(20);
    MoveWindow(e, p.left + sc(34), (p.top + p.bottom - eh) / 2, (p.right - p.left - sc(34) - sc(12)).max(0), eh, 1);
}

/// Background of the edit boxes (address and filter).
pub fn pill_brush() -> isize {
    if BRUSH.get() == 0 { BRUSH.set(unsafe { CreateSolidBrush(pill()) } as isize); }
    BRUSH.get()
}

/// The bottom bar: the filter box (accent outline while typing), the "Subfolders" switch (accent while on) and the
/// counts from status_text().
pub unsafe fn paint_status(h: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(h, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let strip = rc(0, cr.bottom - sc(STATUS_H), cr.right, cr.bottom);
    let (sw, shh) = ((strip.right - strip.left).max(1), (strip.bottom - strip.top).max(1));
    // drawn off screen and copied in one go: no flicker while typing
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, sw, shh));
    let old = SelectObject(dc, bmp);
    let bg = CreateSolidBrush(pal().bg);
    FillRect(dc, &rc(0, 0, sw, shh), bg);
    DeleteObject(bg);
    fill(dc, rc(0, 0, cr.right, 1), pal().line, 0); // hairline above the bar
    SetBkMode(dc, TRANSPARENT as i32);
    // the bitmap holds only the bar: everything moves up by its top
    let up = |r: RECT| rc(r.left, r.top - strip.top, r.right, r.bottom - strip.top);
    let (pill_r, chip, txt) = bottom_rects(cr.right, cr.bottom);
    let (pill_r, chip, txt) = (up(pill_r), up(chip), up(txt));
    let (e, sub) = FILT.with(|f| f.try_borrow().map_or((0, false), |f| (f.edit, f.sub)));
    let focused = e != 0 && GetFocus() == e as HWND;
    let (f_txt, f_glyph, f_small) = (font(13, 400, ui_face()) as HFONT, font(14, 400, winrust_common::style::icon_face()) as HFONT, font(12, 400, ui_face()) as HFONT);
    let of = SelectObject(dc, f_glyph);
    fill(dc, pill_r, if focused { pal().accent } else { pal().line }, sc(16));
    fill(dc, rc(pill_r.left + 1, pill_r.top + 1, pill_r.right - 1, pill_r.bottom - 1), pill(), sc(15));
    text(dc, "\u{E721}", rc(pill_r.left + sc(8), pill_r.top, pill_r.left + sc(32), pill_r.bottom), if focused { pal().accent } else { pal().dim }, DT_CENTER);
    // the switch: folder-tree glyph and its name
    let on_bg = mix(pal().bg, pal().accent, 0.22);
    fill(dc, chip, if sub { on_bg } else { mix(pal().bg, pal().text, 0.06) }, sc(16));
    let ink = if sub { pal().accent } else { pal().dim };
    text(dc, "\u{E8B7}", rc(chip.left + sc(8), chip.top, chip.left + sc(30), chip.bottom), ink, DT_CENTER);
    SelectObject(dc, f_txt);
    text(dc, "Subfolders", rc(chip.left + sc(32), chip.top, chip.right - sc(8), chip.bottom), if sub { pal().text } else { pal().dim }, DT_LEFT);
    SelectObject(dc, f_small);
    if txt.right > txt.left + sc(40) { text(dc, &status_text(), txt, pal().dim, DT_RIGHT); }
    SelectObject(dc, of);
    for f in [f_txt, f_glyph, f_small] { DeleteObject(f); }
    BitBlt(wdc, 0, strip.top, sw, shh, dc, 0, 0, SRCCOPY);
    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteDC(dc);
    EndPaint(h, &ps);
}

pub unsafe fn status_dirty(h: HWND) {
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let strip = rc(0, cr.bottom - sc(STATUS_H), cr.right, cr.bottom);
    InvalidateRect(h, &strip, 0);
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
            let (act, idx) = match k {
                BACK => (4, 0), FWD => (5, 0), UP => (6, 0), RELOAD => (7, 0), SEARCH => (9, 0), VIEW => (8, 0), PLUS => (3, 0), GEAR => (10, 0), STAR => (11, 0),
                k if k >= CLOSE => (2, (k - CLOSE) as isize),
                k => (if m == WM_MBUTTONDOWN { 2 } else { 1 }, (k - TAB) as isize), // middle click on a tab closes it
            };
            PostMessageW(GetParent(h), WM_UI, act, idx);
        }
        WM_COMMAND if (wp >> 16) == 0x0100 || (wp >> 16) == 0x0200 => {
            // edit got / lost focus: shown only while typing (the path parts otherwise), outline colour
            ShowWindow(lp as HWND, if (wp >> 16) == 0x0100 { SW_SHOW } else { SW_HIDE });
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
const RECENTS_SHOWN: usize = 8;

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
    NAVTOP.set(NAVTOP.get().clamp(0, (content_h() - cr.bottom).max(0)));
}

/// Chrome on top, the pane on the left (hidden when the window is narrow), the list takes the rest, the bar at the bottom.
pub unsafe fn layout_children(cw: i32, ch: i32) {
    let (chrome, list) = ST.with(|c| c.try_borrow().map_or((ptr::null_mut(), ptr::null_mut()), |s| (s.tab, s.list)));
    let nav = NAVWND.get() as HWND;
    let (top, status) = (sc(CHROME_H), sc(STATUS_H));
    let nw = if cw > sc(760) { sc(NAV_W) } else { 0 };
    let h = (ch - top - status).max(0);
    MoveWindow(chrome, 0, 0, cw, top, 1);
    MoveWindow(nav, 0, top, nw, h, 1);
    MoveWindow(list, nw, top, cw - nw, h, 1);
    fit_columns(list);
    place_filter(cw, ch);
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
    let (f_txt, f_head, f_glyph, f_small) = (font(13, 400, ui_face()) as HFONT, font(13, 600, ui_face()) as HFONT, font(13, 400, winrust_common::style::icon_face()) as HFONT, font(11, 400, ui_face()) as HFONT);
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
    for f in [f_txt, f_head, f_glyph, f_small] { DeleteObject(f); }
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
            let d = ((wp >> 16) & 0xFFFF) as i16 as i32;
            NAVTOP.set(NAVTOP.get() - d * sc(NAV_ROW) * 3 / 120);
            clamp_scroll();
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
