//! The settings page: a small window with a switch per option, opened by the gear in the toolbar or Ctrl+,.
//! Every switch takes effect and is saved (prefs.rs) the moment it is clicked.
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use ui::{fill, mix, pal, rc, sc, text};
use windows_sys::Win32::Graphics::Gdi::*;

pub const CLASS: &str = "RxOptions";

type Get = fn(&prefs::Prefs) -> bool;
type Set = fn(&mut prefs::Prefs, bool);

const ROWS: [(&str, &str, Get, Set); 8] = [
    ("Reopen the last tabs", "Start where you left off instead of on Home", |p| p.restore_tabs, |p, v| p.restore_tabs = v),
    ("Type to filter", "Typing in the list filters it, like Ctrl+F", |p| p.type_filter, |p, v| p.type_filter = v),
    ("Filter searches subfolders", "The filter looks through every folder below (Ctrl+E)", |p| p.subfolders, |p, v| p.subfolders = v),
    ("Show hidden files", "Hidden files and folders in lists and searches (Ctrl+H)", |p| p.hidden, |p, v| p.hidden = v),
    ("Folders first", "Keep folders above files whatever the sort", |p| p.folders_first, |p, v| p.folders_first = v),
    ("Recent places", "Folders and files opened lately, at the top of the side pane", |p| p.recents, |p, v| p.recents = v),
    ("Folder sizes", "Count what is inside each folder: size, folders and files", |p| p.folder_sizes, |p, v| p.folder_sizes = v),
    ("Smooth list (preview)", "The new GPU-drawn list in the Details view, still being built", |p| p.smooth_list, |p, v| p.smooth_list = v),
];
const CLEAR: usize = ROWS.len(); // the "Clear recent places" button
const UNHIDE: usize = ROWS.len() + 1; // "Show hidden menu items" (those hidden with Delete in a menu)

const PAD: i32 = 20;
const HEAD: i32 = 64;
const ROW: i32 = 62;
const WIDTH: i32 = 540;

thread_local! {
    static WND: Cell<isize> = const { Cell::new(0) };
    static OWNER: Cell<isize> = const { Cell::new(0) };
    static HOT: Cell<i32> = const { Cell::new(-1) };
}

fn height() -> i32 { sc(HEAD) + sc(ROW) * (ROWS.len() as i32 + 1) + sc(PAD) * 2 + sc(24) }

/// The buttons under the switches (UNHIDE only while some menu items are hidden).
fn button(i: usize) -> Option<RECT> {
    let (l, y) = (sc(PAD), sc(HEAD) + sc(ROW) * ROWS.len() as i32 + sc(14));
    match i {
        CLEAR => Some(rc(l, y, l + sc(190), y + sc(34))),
        UNHIDE => (!crate::menu::user_hidden().is_empty()).then(|| rc(l + sc(200), y, l + sc(200) + sc(250), y + sc(34))),
        _ => None,
    }
}

/// The switch row or button at client point (x, y): a button only reacts inside itself.
fn row_at(x: i32, y: i32) -> Option<usize> {
    let i = (y - sc(HEAD)).div_euclid(sc(ROW));
    if y >= sc(HEAD) && (i as usize) < ROWS.len() { return Some(i as usize); }
    [CLEAR, UNHIDE].into_iter().find(|&b| button(b).is_some_and(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom))
}

/// Opens the page over `owner` (or brings it forward when it is already open).
pub unsafe fn open(owner: HWND) {
    let open = WND.get() as HWND;
    if !open.is_null() && IsWindow(open) != 0 { SetForegroundWindow(open); return; }
    OWNER.set(owner as isize);
    let mut r: RECT = std::mem::zeroed();
    GetWindowRect(owner, &mut r);
    // client size -> window size for the caption and border
    let mut wr = rc(0, 0, sc(WIDTH), height());
    let style = WS_CAPTION | WS_SYSMENU | WS_POPUP;
    AdjustWindowRectEx(&mut wr, style, 0, WS_EX_DLGMODALFRAME);
    let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
    let (x, y) = ((r.left + r.right - ww) / 2, (r.top + r.bottom - wh) / 2);
    let h = CreateWindowExW(WS_EX_DLGMODALFRAME, w(CLASS).as_ptr(), w("Settings").as_ptr(), style, x, y, ww, wh, owner, 0 as _, GetModuleHandleW(ptr::null()), ptr::null());
    WND.set(h as isize);
    ui::dark_title(h);
    ShowWindow(h, SW_SHOW);
}

pub unsafe fn close() {
    let h = WND.replace(0) as HWND;
    if !h.is_null() && IsWindow(h) != 0 { DestroyWindow(h); }
}

unsafe fn paint(h: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(h, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right.max(1), cr.bottom.max(1)));
    let old = SelectObject(dc, bmp);
    let bg = CreateSolidBrush(pal().bg);
    FillRect(dc, &cr, bg);
    DeleteObject(bg);
    SetBkMode(dc, TRANSPARENT as i32);
    let (f_title, f_txt, f_small) = (ui::font(20, 600, ui::ui_face()) as HFONT, ui::font(14, 500, ui::ui_face()) as HFONT, ui::font(12, 400, ui::ui_face()) as HFONT);
    let of = SelectObject(dc, f_title);
    let (l, r) = (sc(PAD), cr.right - sc(PAD));
    text(dc, "File manager", rc(l + sc(4), sc(14), r, sc(HEAD) - sc(10)), pal().text, DT_LEFT);
    let p = prefs_now();
    let hot = HOT.get();
    // one card holding every switch, hairlines between them
    let card = rc(l, sc(HEAD), r, sc(HEAD) + sc(ROW) * ROWS.len() as i32);
    fill(dc, card, ui::listbg(), sc(14));
    for (i, (title, desc, get, _)) in ROWS.iter().enumerate() {
        let y = sc(HEAD) + sc(ROW) * i as i32;
        if hot == i as i32 { fill(dc, rc(l + sc(4), y + sc(4), r - sc(4), y + sc(ROW) - sc(4)), mix(ui::listbg(), pal().text, 0.05), sc(10)); }
        if i > 0 { fill(dc, rc(l + sc(16), y, r - sc(16), y + 1), pal().line, 0); }
        SelectObject(dc, f_txt);
        text(dc, title, rc(l + sc(18), y + sc(10), r - sc(80), y + sc(34)), pal().text, DT_LEFT);
        SelectObject(dc, f_small);
        text(dc, desc, rc(l + sc(18), y + sc(32), r - sc(80), y + sc(54)), pal().dim, DT_LEFT);
        // the switch: a track that fills with the accent colour, the knob on the right while on
        let on = get(&p);
        let (tw, th) = (sc(40), sc(22));
        let tr = rc(r - sc(18) - tw, y + (sc(ROW) - th) / 2, r - sc(18), y + (sc(ROW) + th) / 2);
        fill(dc, tr, if on { pal().accent } else { mix(ui::listbg(), pal().text, 0.22) }, th);
        let k = th - sc(6);
        let kx = if on { tr.right - sc(3) - k } else { tr.left + sc(3) };
        fill(dc, rc(kx, tr.top + sc(3), kx + k, tr.top + sc(3) + k), 0x00FF_FFFF, k);
    }
    // the buttons under the card
    SelectObject(dc, f_txt);
    let n = crate::menu::user_hidden().len();
    for (i, label) in [(CLEAR, "Clear recent places".to_string()), (UNHIDE, format!("Show hidden menu items ({n})"))] {
        let Some(b) = button(i) else { continue };
        fill(dc, b, if hot == i as i32 { mix(pal().bg, pal().text, 0.14) } else { mix(pal().bg, pal().text, 0.08) }, sc(10));
        text(dc, &label, b, pal().text, DT_CENTER);
    }
    BitBlt(wdc, 0, 0, cr.right, cr.bottom, dc, 0, 0, SRCCOPY);
    SelectObject(dc, of);
    for f in [f_title, f_txt, f_small] { DeleteObject(f); }
    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteDC(dc);
    EndPaint(h, &ps);
}

pub unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
    match m {
        WM_PAINT => { paint(h); 0 }
        WM_ERASEBKGND => 1,
        WM_MOUSEMOVE => {
            let k = row_at(x, y).map_or(-1, |i| i as i32);
            if HOT.replace(k) != k { InvalidateRect(h, ptr::null(), 0); }
            0
        }
        WM_LBUTTONDOWN => {
            let owner = OWNER.get() as HWND;
            match row_at(x, y) {
                Some(CLEAR) => forget_recent(None),
                Some(UNHIDE) => crate::menu::unhide_all(),
                Some(i) => {
                    let (_, _, get, set) = ROWS[i];
                    let on = !get(&prefs_now());
                    set_prefs(owner, |p| set(p, on), true);
                }
                None => {}
            }
            InvalidateRect(h, ptr::null(), 0);
            0
        }
        WM_KEYDOWN if wp == 0x1B => { DestroyWindow(h); 0 }
        WM_DESTROY => { WND.set(0); 0 }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}
