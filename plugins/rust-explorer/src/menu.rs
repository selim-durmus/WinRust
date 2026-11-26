//! Our own right-click menu: a small dark window with a search box on top, instead of the system's long menu.
//! The items of a file's menu still come from the shell (every program's own entries, "Open with", "Send to"), read out
//! of the menu the shell fills in; only a few that are just noise are left out. Typing filters the items, submenus
//! included ("winrar add" finds WinRAR > Add to archive). Keys: Up / Down, Enter, Right opens a submenu, Left / Esc
//! closes it, Esc closes the menu; a click outside closes it too. Delete hides the item under the pointer for good
//! (menu-hidden.txt; the settings page brings them all back).
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use ui::{fill, mix, pal, rc, sc, text};
use windows_sys::Win32::Graphics::Gdi::*;

/// One entry: text (and a key hint on the right), what choosing it returns (`id`), an icon (a bitmap from the shell,
/// or one of our glyphs), a submenu, or a separator line.
#[derive(Clone, Default)]
pub struct Item {
    pub text: String,
    pub hint: String,
    pub id: u32,
    pub icon: isize,       // HBITMAP or 0
    pub icon_alpha: bool,  // the bitmap has alpha (premultiplied); else drawn as is
    pub glyph: &'static str,
    pub sub: Vec<Item>,
    pub sep: bool,
    pub disabled: bool,
    pub bold: bool,
}

impl Item {
    pub fn new(id: u32, glyph: &'static str, text: &str, hint: &str) -> Item { Item { id, glyph, text: text.into(), hint: hint.into(), ..Default::default() } }
    pub fn sep() -> Item { Item { sep: true, ..Default::default() } }
}

pub const CLASS: &str = "RxMenu";
const ROW: i32 = 30;
const SEPH: i32 = 9;
const SEARCH: i32 = 38;
const PAD: i32 = 6;

/// Items of the shell menu that only add clutter: left out (by their English text, or the verb).
const HIDDEN: [&str; 7] = ["give access to", "restore previous versions", "include in library", "copy as path", "take ownership", "send to", "add to favorites"];

/// Items hidden with Delete in an open menu (their text, lower case), in %APPDATA%\rust-explorer\menu-hidden.txt.
pub fn user_hidden() -> Vec<String> { prefs::read("menu-hidden.txt").lines().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()).collect() }

/// Brings every item hidden with Delete back.
pub fn unhide_all() { prefs::write("menu-hidden.txt", ""); }

fn hide_for_good(key: &str) {
    let mut v = user_hidden();
    if !v.iter().any(|k| k == key) { v.push(key.to_string()); }
    prefs::write("menu-hidden.txt", &(v.join("\n") + "\n"));
}

/// The text an item is known by: in a search, "Parent › Item" shows, the item is the last part.
fn key_of(it: &Item) -> String { it.text.rsplit(" \u{203A} ").next().unwrap_or("").to_lowercase() }

/// Leaves out the items named in `hidden` (and submenus left empty), with no stray separators.
fn strip(items: Vec<Item>, hidden: &[String]) -> Vec<Item> {
    let out = items.into_iter().filter_map(|mut it| {
        if it.sep { return Some(it); }
        if hidden.iter().any(|h| *h == key_of(&it)) { return None; }
        if !it.sub.is_empty() {
            it.sub = strip(std::mem::take(&mut it.sub), hidden);
            if it.sub.is_empty() { return None; }
        }
        Some(it)
    }).collect();
    clean_seps(out)
}

/// Separators only between items: none at the ends, never two in a row.
fn clean_seps(out: Vec<Item>) -> Vec<Item> {
    let mut clean: Vec<Item> = vec![];
    for it in out {
        if it.sep && clean.last().is_none_or(|l: &Item| l.sep) { continue; }
        clean.push(it);
    }
    while clean.last().is_some_and(|l| l.sep) { clean.pop(); }
    clean
}

/// Verbs left out: copy as path, and the pins of Windows' Start, taskbar and Quick access (and the installer's "Pin to
/// Start"): the menu offers WinRust's own pins and bookmarks instead.
const HIDDEN_VERBS: [&str; 5] = ["copyaspath", "winrustpintostart", "pintostartscreen", "taskbarpin", "pintohome"];

/// Key hints for the usual shell verbs (the shell gives none).
fn hint_of(verb: &str) -> &'static str {
    match verb { "cut" => "Ctrl+X", "copy" => "Ctrl+C", "paste" => "Ctrl+V", "delete" => "Del", "rename" => "F2", "properties" => "Alt+Enter", "open" => "Enter", _ => "" }
}

/// Reads a menu the shell filled in (QueryContextMenu) into items. `msg` hands a menu message to the shell's handler
/// (submenus like "Send to" are filled only when told they open; some icons are drawn by the handler); `verb` names an
/// item's command.
pub unsafe fn from_hmenu(m: HMENU, msg: &dyn Fn(u32, usize, isize) -> bool, verb: &dyn Fn(u32) -> String) -> Vec<Item> {
    let mut out = vec![];
    for i in 0..GetMenuItemCount(m).max(0) {
        let mut buf = [0u16; 260];
        let mut mi: MENUITEMINFOW = std::mem::zeroed();
        mi.cbSize = size_of::<MENUITEMINFOW>() as u32;
        mi.fMask = MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_STRING | MIIM_BITMAP | MIIM_DATA | MIIM_CHECKMARKS;
        mi.dwTypeData = buf.as_mut_ptr();
        mi.cch = 259;
        if GetMenuItemInfoW(m, i as u32, 1, &mut mi) == 0 { continue; }
        if mi.fType & MFT_SEPARATOR != 0 { out.push(Item::sep()); continue; }
        let raw = String::from_utf16_lossy(&buf[..(mi.cch as usize).min(259)]);
        if raw.trim().is_empty() { continue; } // drawn by its handler, no text to show
        let (label, tab_hint) = raw.split_once('\t').map_or((raw.as_str(), ""), |(a, b)| (a, b));
        let label = label.replace("&&", "\u{1}").replace('&', "").replace('\u{1}', "&");
        let v = if mi.hSubMenu.is_null() { verb(mi.wID) } else { String::new() };
        if HIDDEN.contains(&label.to_lowercase().as_str()) || HIDDEN_VERBS.contains(&v.as_str()) { continue; }
        let mut it = Item { text: label, id: mi.wID, disabled: mi.fState & MFS_DISABLED != 0, bold: mi.fState & MFS_DEFAULT != 0, ..Default::default() };
        it.hint = if tab_hint.is_empty() { hint_of(&v).into() } else { tab_hint.into() };
        if !mi.hSubMenu.is_null() {
            msg(WM_INITMENUPOPUP, mi.hSubMenu as usize, i as isize);
            it.sub = from_hmenu(mi.hSubMenu, msg, verb);
            if it.sub.iter().all(|s| s.sep) { continue; } // an empty submenu
        }
        // the item's picture; some (the UAC shield) come as the "unchecked" check-mark picture instead
        let hb = if mi.hbmpItem.is_null() && !mi.hbmpUnchecked.is_null() { mi.hbmpUnchecked as isize } else { mi.hbmpItem as isize };
        if hb == -1 {
            it.icon = drawn_icon(msg, &mi, m); // HBMMENU_CALLBACK: the handler paints it
        } else if !(0..=11).contains(&hb) {
            // a real bitmap (1..11 are the system's own menu glyphs). Handles are 32-bit values that come sign-extended:
            // half of them look negative, and "> 11" used to drop those icons at random
            it.icon = hb;
            it.icon_alpha = has_alpha(hb);
        }
        out.push(it);
    }
    clean_seps(out)
}

/// A 32-bit bitmap with an alpha channel (drawn blended); others (no alpha at all) are drawn as they are, or they would
/// not show.
unsafe fn has_alpha(hb: isize) -> bool {
    let mut bm: BITMAP = std::mem::zeroed();
    if GetObjectW(hb as HGDIOBJ, size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _) == 0 || bm.bmBitsPixel != 32 { return false; }
    if bm.bmBits.is_null() { return true; } // not readable: trust it
    let px = std::slice::from_raw_parts(bm.bmBits as *const u8, (bm.bmWidthBytes * bm.bmHeight.abs()) as usize);
    px.chunks_exact(4).any(|p| p[3] != 0)
}

/// An icon the shell handler draws itself (WM_DRAWITEM), painted onto a small bitmap in the menu's colour.
unsafe fn drawn_icon(msg: &dyn Fn(u32, usize, isize) -> bool, mi: &MENUITEMINFOW, m: HMENU) -> isize {
    let s = sc(16);
    let screen = GetDC(ptr::null_mut());
    let dc = CreateCompatibleDC(screen);
    let bmp = CreateCompatibleBitmap(screen, s, s);
    ReleaseDC(ptr::null_mut(), screen);
    let old = SelectObject(dc, bmp);
    let b = CreateSolidBrush(menu_bg());
    FillRect(dc, &rc(0, 0, s, s), b);
    DeleteObject(b);
    let mut mis: MEASUREITEMSTRUCT = std::mem::zeroed();
    mis.CtlType = ODT_MENU;
    mis.itemID = mi.wID;
    mis.itemData = mi.dwItemData;
    msg(WM_MEASUREITEM, 0, &mut mis as *mut _ as isize);
    let mut dis: DRAWITEMSTRUCT = std::mem::zeroed();
    dis.CtlType = ODT_MENU;
    dis.itemID = mi.wID;
    dis.itemAction = ODA_DRAWENTIRE;
    dis.hwndItem = m as HWND;
    dis.hDC = dc;
    dis.rcItem = rc(0, 0, s, s);
    dis.itemData = mi.dwItemData;
    let ok = msg(WM_DRAWITEM, 0, &dis as *const _ as isize);
    SelectObject(dc, old);
    DeleteDC(dc);
    if ok { bmp as isize } else { DeleteObject(bmp); 0 }
}

fn menu_bg() -> u32 { if winrust_common::style::style().light() { winrust_common::style::card() } else { mix(pal().bg, pal().text, 0.03) } }

// ---------- the menu windows ----------

/// One open menu window: the items it shows (after the search), the one under the pointer / keys, its scroll.
struct Level { wnd: HWND, view: Vec<Item>, hot: i32, top: i32, opened_from: i32 }

struct Menu { levels: Vec<Level>, all: Vec<Item>, result: u32, done: bool, query: String }

thread_local! {
    static MENU: RefCell<Option<Menu>> = const { RefCell::new(None) };
    static EDIT_OLD: Cell<isize> = const { Cell::new(0) };
    static FONTS: Cell<[isize; 4]> = const { Cell::new([0; 4]) }; // text, bold, glyphs, key hints
}

fn fonts() -> [isize; 4] {
    if FONTS.get()[0] == 0 { FONTS.set([ui::font(13, 400, ui::ui_face()), ui::font(13, 600, ui::ui_face()), ui::font(13, 400, winrust_common::style::icon_face()), ui::font(11, 400, ui::ui_face())]); }
    FONTS.get()
}

fn row_h(it: &Item) -> i32 { if it.sep { sc(SEPH) } else { sc(ROW) } }

/// What a level shows: the items, or with a search the items (and submenu items, "Parent › Item") holding every word.
fn filtered(all: &[Item], q: &str) -> Vec<Item> {
    let words: Vec<String> = q.to_lowercase().split_whitespace().map(String::from).collect();
    if words.is_empty() { return all.to_vec(); }
    fn walk(out: &mut Vec<Item>, items: &[Item], prefix: &str, words: &[String]) {
        for it in items.iter().filter(|i| !i.sep) {
            let name = if prefix.is_empty() { it.text.clone() } else { format!("{prefix} \u{203A} {}", it.text) };
            if it.sub.is_empty() {
                let l = name.to_lowercase();
                if words.iter().all(|w| l.contains(w.as_str())) { let mut x = it.clone(); x.text = name; out.push(x); }
            } else {
                walk(out, &it.sub, &name, words);
            }
        }
    }
    let mut out = vec![];
    walk(&mut out, all, "", &words);
    out
}

/// The size a level needs for `items` (with the search box on the first one).
unsafe fn measure(items: &[Item], search: bool) -> (i32, i32) {
    let dc = GetDC(ptr::null_mut());
    let f = fonts();
    let of = SelectObject(dc, f[1] as HFONT);
    let tw = |s: &str| { let mut r = rc(0, 0, 0, 0); DrawTextW(dc, w(s).as_ptr(), -1, &mut r, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX); r.right };
    let text_w = items.iter().map(|i| tw(&i.text)).max().unwrap_or(0);
    SelectObject(dc, f[3] as HFONT);
    let hint_w = items.iter().map(|i| tw(&i.hint)).max().unwrap_or(0);
    SelectObject(dc, of);
    ReleaseDC(ptr::null_mut(), dc);
    let width = (sc(PAD) * 2 + sc(36) + text_w + sc(28) + hint_w + sc(22)).clamp(sc(230), sc(520));
    let height = sc(PAD) * 2 + if search { sc(SEARCH) } else { 0 } + items.iter().map(row_h).sum::<i32>().max(sc(ROW));
    (width, height)
}

/// The work area of the monitor at `pt`.
unsafe fn work_at(x: i32, y: i32) -> RECT {
    let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(mon, &mut mi);
    mi.rcWork
}

/// Puts level window `h` of size (w, ht) at `x`, `y` (flipped left / up to stay on its screen; `alt_x` = where it goes
/// when it does not fit on the right, for submenus).
unsafe fn place(h: HWND, x: i32, y: i32, wd: i32, ht: i32, alt_x: i32, up_from: Option<i32>) {
    // inside the window it belongs to (and its screen), like File Pilot: no menu hanging out of the window
    let b = BOUNDS.get();
    let ht = ht.min(b.bottom - b.top);
    let x = if x + wd > b.right { alt_x } else { x }.clamp(b.left, (b.right - wd).max(b.left));
    let y = if y + ht > b.bottom {
        // no room below: it opens upward from the pointer (or sits on the bottom edge)
        match up_from { Some(u) if u - ht >= b.top => u - ht, _ => b.bottom - ht }
    } else { y }.max(b.top);
    SetWindowPos(h, HWND_TOPMOST, x, y, wd, ht, SWP_NOACTIVATE);
}

thread_local! {
    static BOUNDS: Cell<RECT> = const { Cell::new(RECT { left: 0, top: 0, right: 0, bottom: 0 }) }; // where the menu may go
    static PASS: Cell<Option<(isize, u32, usize, isize)>> = const { Cell::new(None) }; // the click that closed it, passed on after
}

unsafe fn new_level_window(owner: HWND) -> HWND {
    let h = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST, w(CLASS).as_ptr(), ptr::null(), WS_POPUP, 0, 0, 0, 0, owner, ptr::null_mut(), GetModuleHandleW(ptr::null()), ptr::null());
    let round: i32 = 2; // DWMWCP_ROUND
    windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(h, 33, &round as *const _ as _, 4);
    h
}

/// Shows the menu at screen point `at` and waits for a choice: the chosen item's id, 0 when nothing was chosen.
pub unsafe fn track(owner: HWND, items: Vec<Item>, at: (i32, i32)) -> u32 {
    let items = strip(items, &user_hidden());
    if items.is_empty() { return 0; }
    let wnd = new_level_window(owner);
    let edit = CreateWindowExW(0, w("EDIT").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32, 0, 0, 0, 0, wnd, ptr::null_mut(), GetModuleHandleW(ptr::null()), ptr::null());
    SendMessageW(edit, WM_SETFONT, fonts()[0] as usize, 0);
    SendMessageW(edit, 0x1501, 1, w("Search\u{2026}").as_ptr() as isize); // EM_SETCUEBANNER
    EDIT_OLD.set(SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize));
    MENU.set(Some(Menu { levels: vec![Level { wnd, view: items.clone(), hot: -1, top: 0, opened_from: -1 }], all: items.clone(), result: 0, done: false, query: String::new() }));
    let (wd, ht) = measure(&items, true);
    let (wa, mut wr) = (work_at(at.0, at.1), std::mem::zeroed::<RECT>());
    // the window as seen (its invisible resize border left out)
    if windows_sys::Win32::Graphics::Dwm::DwmGetWindowAttribute(owner, 9, &mut wr as *mut _ as *mut _, size_of::<RECT>() as u32) != 0 { GetWindowRect(owner, &mut wr); }
    BOUNDS.set(RECT { left: wr.left.max(wa.left), top: wr.top.max(wa.top), right: wr.right.min(wa.right), bottom: wr.bottom.min(wa.bottom) });
    place(wnd, at.0, at.1, wd, ht, at.0 - wd, Some(at.1));
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(wnd, &mut cr);
    MoveWindow(edit, sc(PAD) + sc(34), sc(PAD) + (sc(SEARCH) - sc(20)) / 2, cr.right - sc(PAD) * 2 - sc(44), sc(20), 1);
    ShowWindow(wnd, SW_SHOW);
    SetForegroundWindow(wnd);
    SetFocus(edit);
    let fg0 = GetForegroundWindow(); // what is in front with the menu up: when that changes, the menu goes
    // the menu's own message loop: it returns when an item is chosen or the menu is closed
    let mut msg: MSG = std::mem::zeroed();
    loop {
        if MENU.with_borrow(|m| m.as_ref().is_none_or(|m| m.done)) { break; }
        if GetMessageW(&mut msg, ptr::null_mut(), 0, 0) <= 0 { PostQuitMessage(msg.wParam as i32); break; }
        // a click anywhere else closes the menu (and is not passed on)
        // a click anywhere else closes the menu and still does what it does (a right click on another file opens its menu)
        if matches!(msg.message, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_NCLBUTTONDOWN | WM_NCRBUTTONDOWN) && !ours(msg.hwnd) {
            PASS.set(Some((msg.hwnd as isize, msg.message, msg.wParam, msg.lParam)));
            finish(0);
            continue;
        }
        if msg.message == WM_KEYDOWN && key(msg.wParam as u32) { continue; }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
        // another window came to the front (Alt+Tab, a click on the taskbar): the menu goes
        let fg = GetForegroundWindow();
        if !fg.is_null() && fg != fg0 && fg != owner && !ours(fg) { finish(0); }
    }
    let m = MENU.take();
    if let Some(m) = &m { for l in m.levels.iter().rev() { DestroyWindow(l.wnd); } }
    SetForegroundWindow(owner);
    if let Some((w, msg, wp, lp)) = PASS.take() { PostMessageW(w as HWND, msg, wp, lp); }
    m.map_or(0, |m| m.result)
}

/// A window of the menu (a level, or the search box)?
unsafe fn ours(h: HWND) -> bool {
    MENU.with(|m| m.try_borrow().ok().is_some_and(|m| m.as_ref().is_some_and(|m| m.levels.iter().any(|l| l.wnd == h || IsChild(l.wnd, h) != 0))))
}

fn finish(result: u32) {
    MENU.with_borrow_mut(|m| if let Some(m) = m { m.result = result; m.done = true; });
}

/// The search changed: the first level shows what matches, deeper levels close.
unsafe fn requery(q: String) {
    let (wnd, view, closing) = MENU.with_borrow_mut(|m| {
        let m = m.as_mut().unwrap();
        m.query = q;
        let closing: Vec<HWND> = m.levels.drain(1..).map(|l| l.wnd).collect();
        let view = filtered(&m.all, &m.query);
        let l = &mut m.levels[0];
        l.view = view.clone();
        l.top = 0;
        l.hot = view.iter().position(|i| !i.sep && !i.disabled).map_or(-1, |i| i as i32);
        (l.wnd, view, closing)
    });
    for c in closing { DestroyWindow(c); }
    let mut r: RECT = std::mem::zeroed();
    GetWindowRect(wnd, &mut r);
    let (wd, ht) = measure(&view, true);
    let _ = wd;
    place(wnd, r.left, r.top, r.right - r.left, ht, r.left, None);
    InvalidateRect(wnd, ptr::null(), 0);
}

/// Opens the submenu of row `i` of level `li` (closing deeper ones); `focus` = its first item is hot (keyboard).
unsafe fn open_sub(li: usize, i: i32, focus: bool) {
    let info = MENU.with_borrow_mut(|m| {
        let m = m.as_mut()?;
        let closing: Vec<HWND> = m.levels.drain(li + 1..).map(|l| l.wnd).collect();
        let it = m.levels[li].view.get(i as usize)?.clone();
        Some((closing, it, m.levels[li].wnd, m.levels[li].top))
    });
    let Some((closing, it, parent, top)) = info else { return };
    for c in closing { DestroyWindow(c); }
    if it.sub.is_empty() || it.disabled { return; }
    // the row's place on screen: the submenu starts beside it
    let mut pr: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut pr);
    let y = pr.top + row_top(li, i) - top;
    let wnd = new_level_window(GetWindow(parent, GW_OWNER));
    let (wd, ht) = measure(&it.sub, false);
    MENU.with_borrow_mut(|m| if let Some(m) = m { m.levels.push(Level { wnd, view: it.sub.clone(), hot: if focus { it.sub.iter().position(|s| !s.sep).map_or(-1, |p| p as i32) } else { -1 }, top: 0, opened_from: i }); });
    place(wnd, pr.right - sc(4), y - sc(PAD), wd, ht, pr.left - wd + sc(4), None);
    ShowWindow(wnd, SW_SHOWNOACTIVATE);
}

/// Top of row `i` of level `li` in its window (before scrolling).
fn row_top(li: usize, i: i32) -> i32 {
    MENU.with_borrow(|m| {
        let Some(m) = m.as_ref() else { return 0 };
        let l = &m.levels[li];
        sc(PAD) + if li == 0 { sc(SEARCH) } else { 0 } + l.view.iter().take(i.max(0) as usize).map(row_h).sum::<i32>()
    })
}

/// Row of level `li` at client y (None over the search box, separators or empty space).
fn row_at(li: usize, y: i32) -> Option<i32> {
    MENU.with_borrow(|m| {
        let m = m.as_ref()?;
        let l = &m.levels[li];
        let mut t = sc(PAD) + if li == 0 { sc(SEARCH) } else { 0 } - l.top;
        for (i, it) in l.view.iter().enumerate() {
            let hh = row_h(it);
            if y >= t && y < t + hh { return (!it.sep).then_some(i as i32); }
            t += hh;
        }
        None
    })
}

/// Level of window `h`; None for a window being destroyed (or while the state is being changed).
fn level_of(h: HWND) -> Option<usize> { MENU.with(|m| m.try_borrow().ok()?.as_ref()?.levels.iter().position(|l| l.wnd == h)) }

/// Chooses row `i` of level `li`: a submenu opens, anything else is the answer.
unsafe fn activate(li: usize, i: i32) {
    let it = MENU.with_borrow(|m| m.as_ref().and_then(|m| m.levels[li].view.get(i as usize).cloned()));
    let Some(it) = it else { return };
    if it.sep || it.disabled { return; }
    if !it.sub.is_empty() { open_sub(li, i, true); return; }
    finish(it.id);
}

/// Keys for the deepest open level. true = handled (not passed on to the search box).
unsafe fn key(vk: u32) -> bool {
    let Some((li, hot, n, wnd, has_sub)) = MENU.with_borrow(|m| {
        let m = m.as_ref()?;
        let li = m.levels.len() - 1;
        let l = &m.levels[li];
        Some((li, l.hot, l.view.len() as i32, l.wnd, l.view.get(l.hot.max(0) as usize).is_some_and(|i| !i.sub.is_empty())))
    }) else { return false };
    let step = |d: i32| {
        MENU.with_borrow_mut(|m| {
            let l = &mut m.as_mut().unwrap().levels[li];
            let mut k = l.hot;
            for _ in 0..n {
                k = (k + d).rem_euclid(n.max(1));
                if l.view.get(k as usize).is_some_and(|i| !i.sep && !i.disabled) { break; }
            }
            l.hot = k;
        });
        ensure_visible(li);
        InvalidateRect(wnd, ptr::null(), 0);
    };
    match vk {
        0x28 => { step(1); true }        // Down
        0x26 => { step(-1); true }       // Up
        0x0D => { if hot >= 0 { activate(li, hot); } true } // Enter
        0x27 if has_sub => { open_sub(li, hot, true); true } // Right
        0x25 | 0x1B if li > 0 => { // Left / Esc in a submenu: back to its parent
            // destroyed after the state is let go: the window's last messages look at the state too
            let gone = MENU.with_borrow_mut(|m| m.as_mut().and_then(|m| m.levels.pop()).map(|l| l.wnd));
            if let Some(g) = gone { DestroyWindow(g); }
            true
        }
        0x1B => { finish(0); true }       // Esc
        0x2E if GetKeyState(VK_CONTROL as i32) >= 0 => hide_hot(), // Delete: the item under the pointer goes for good
        _ => false,
    }
}

/// Delete in the menu: hides the item under the pointer (or the one picked with the keys) in every menu from now on.
/// While typing a search, Delete still deletes the letter after the caret.
unsafe fn hide_hot() -> bool {
    let Some((edit, query)) = MENU.with_borrow(|m| m.as_ref().map(|m| (FindWindowExW(m.levels[0].wnd, ptr::null_mut(), w("EDIT").as_ptr(), ptr::null()), m.query.clone()))) else { return false };
    if !query.is_empty() && (SendMessageW(edit, 0xB0, 0, 0) >> 16) as usize & 0xFFFF < query.encode_utf16().count() { return false; } // EM_GETSEL
    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    let under = level_of(WindowFromPoint(pt));
    let Some((li, it)) = MENU.with_borrow(|m| {
        let m = m.as_ref()?;
        let li = under.unwrap_or(m.levels.len() - 1);
        let l = &m.levels[li];
        l.view.get(l.hot.max(0) as usize).filter(|_| l.hot >= 0).map(|it| (li, it.clone()))
    }) else { return true };
    if it.sep { return true; }
    let key = key_of(&it);
    hide_for_good(&key);
    // out of the open menu at once: levels below the one it was on close, the rest shrink
    let hidden = [key];
    let (closing, wnds) = MENU.with_borrow_mut(|m| {
        let m = m.as_mut().unwrap();
        m.all = strip(std::mem::take(&mut m.all), &hidden);
        let closing: Vec<HWND> = m.levels.drain(li + 1..).map(|l| l.wnd).collect();
        let (all, q) = (m.all.clone(), m.query.clone());
        for (k, l) in m.levels.iter_mut().enumerate() {
            l.view = if k == 0 { filtered(&all, &q) } else { strip(std::mem::take(&mut l.view), &hidden) };
            let n = l.view.len() as i32;
            l.hot = (l.hot.min(n - 1)..n).chain((0..l.hot.min(n)).rev()).find(|&i| l.view.get(i.max(0) as usize).is_some_and(|x| !x.sep && !x.disabled)).unwrap_or(-1);
            l.top = 0;
        }
        // a submenu left empty closes
        let mut closing = closing;
        while m.levels.len() > 1 && m.levels.last().is_some_and(|l| l.view.is_empty()) { closing.push(m.levels.pop().unwrap().wnd); }
        (closing, m.levels.iter().enumerate().map(|(k, l)| (l.wnd, measure(&l.view, k == 0).1)).collect::<Vec<_>>())
    });
    for c in closing { DestroyWindow(c); }
    for (wnd, ht) in wnds {
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(wnd, &mut r);
        place(wnd, r.left, r.top, r.right - r.left, ht, r.left, None);
        InvalidateRect(wnd, ptr::null(), 0);
    }
    if MENU.with_borrow(|m| m.as_ref().is_some_and(|m| m.all.is_empty())) { finish(0); }
    true
}

/// Scrolls level `li` so its hot row shows (long menus taller than the screen).
unsafe fn ensure_visible(li: usize) {
    let (wnd, hot) = MENU.with_borrow(|m| m.as_ref().map(|m| (m.levels[li].wnd, m.levels[li].hot))).unwrap_or((ptr::null_mut(), -1));
    if hot < 0 { return; }
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(wnd, &mut cr);
    let (t, b) = (row_top(li, hot), row_top(li, hot + 1));
    let first = sc(PAD) + if li == 0 { sc(SEARCH) } else { 0 };
    MENU.with_borrow_mut(|m| {
        let l = &mut m.as_mut().unwrap().levels[li];
        if t - l.top < first { l.top = t - first; }
        if b - l.top > cr.bottom - sc(PAD) { l.top = b - (cr.bottom - sc(PAD)); }
    });
}

/// The search box: its text filters the menu; the arrows, Enter and Esc go to the menu.
unsafe extern "system" fn edit_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if m == WM_CHAR && matches!(wp, 0x0D | 0x1B) { return 0; } // no beep
    // the usual text keys the plain box lacks: Ctrl+A selects all, Ctrl+Backspace / Ctrl+Delete delete a word (the box
    // would type a box character for them)
    let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
    if m == WM_CHAR && wp == 0x01 { SendMessageW(h, EM_SETSEL, 0, -1); return 0; }
    if (m == WM_CHAR && wp == 0x7F) || (m == WM_KEYDOWN && wp == 0x2E && ctrl) {
        delete_word(h, wp == 0x7F);
        return 0;
    }
    if m == WM_CHAR && wp < 0x20 && !matches!(wp, 0x08 | 0x03 | 0x16 | 0x18 | 0x1A) { return 0; } // other Ctrl+letters: nothing
    let r = CallWindowProcW(std::mem::transmute::<isize, WNDPROC>(EDIT_OLD.get()), h, m, wp, lp);
    if m == WM_CHAR || (m == WM_KEYDOWN && matches!(wp, 0x2E | 0x08)) {
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(h, buf.as_mut_ptr(), 256).max(0) as usize;
        let q = String::from_utf16_lossy(&buf[..n]);
        if MENU.with_borrow(|x| x.as_ref().is_some_and(|x| x.query != q)) { requery(q); }
    }
    r
}

/// Deletes the word before the caret (`back`) or after it, with the spaces next to it, and searches again.
unsafe fn delete_word(h: HWND, back: bool) {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(h, buf.as_mut_ptr(), 256).max(0) as usize;
    let t = &buf[..n];
    let sel = SendMessageW(h, EM_GETSEL, 0, 0) as usize;
    let (s, e) = (sel & 0xFFFF, (sel >> 16) & 0xFFFF);
    let (a, b) = if s != e { (s.min(e), s.max(e)) } else if back {
        let mut i = s.min(n);
        while i > 0 && t[i - 1] == b' ' as u16 { i -= 1; }
        while i > 0 && t[i - 1] != b' ' as u16 { i -= 1; }
        (i, s)
    } else {
        let mut i = s.min(n);
        while i < n && t[i] != b' ' as u16 { i += 1; }
        while i < n && t[i] == b' ' as u16 { i += 1; }
        (s, i)
    };
    SendMessageW(h, EM_SETSEL, a, b as isize);
    SendMessageW(h, EM_REPLACESEL, 0, w("").as_ptr() as isize);
    let q = String::from_utf16_lossy(&buf[..a]) + &String::from_utf16_lossy(&buf[b.min(n)..n]);
    if MENU.with_borrow(|x| x.as_ref().is_some_and(|x| x.query != q)) { requery(q); }
}

unsafe fn paint(h: HWND, li: usize) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(h, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right.max(1), cr.bottom.max(1)));
    let old = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT as i32);
    let bg = menu_bg();
    let b = CreateSolidBrush(bg);
    FillRect(dc, &cr, b);
    DeleteObject(b);
    let f = fonts();
    let snap = MENU.with(|m| m.try_borrow().ok().and_then(|m| m.as_ref().and_then(|m| m.levels.get(li).map(|l| (l.view.clone(), l.hot, l.top, m.levels.get(li + 1).map(|n| n.opened_from))))));
    let Some((view, hot, top, open_row)) = snap else { EndPaint(h, &ps); return };
    let mut y = sc(PAD) - top;
    if li == 0 {
        // the search box: magnifier, the edit (a child window), a hairline under it
        let of = SelectObject(dc, f[2] as HFONT);
        text(dc, "\u{E721}", rc(sc(PAD) + sc(8), sc(PAD), sc(PAD) + sc(30), sc(PAD) + sc(SEARCH)), pal().dim, DT_CENTER);
        SelectObject(dc, of);
        fill(dc, rc(sc(PAD), sc(PAD) + sc(SEARCH) - 1, cr.right - sc(PAD), sc(PAD) + sc(SEARCH)), pal().line, 0);
        y += sc(SEARCH);
        if view.is_empty() {
            let of = SelectObject(dc, f[0] as HFONT);
            text(dc, "Nothing found", rc(sc(PAD) + sc(36), y, cr.right, y + sc(ROW)), pal().dim, DT_LEFT);
            SelectObject(dc, of);
        }
    }
    // rows (clipped under the search box when scrolled)
    let clip_top = sc(PAD) + if li == 0 { sc(SEARCH) } else { 0 };
    IntersectClipRect(dc, 0, clip_top, cr.right, cr.bottom);
    for (i, it) in view.iter().enumerate() {
        let hh = row_h(it);
        let r = rc(sc(PAD), y, cr.right - sc(PAD), y + hh);
        y += hh;
        if r.bottom < clip_top || r.top > cr.bottom { continue; }
        if it.sep { fill(dc, rc(r.left + sc(10), (r.top + r.bottom) / 2, r.right - sc(10), (r.top + r.bottom) / 2 + 1), pal().line, 0); continue; }
        let on = i as i32 == hot || open_row == Some(i as i32);
        if on && !it.disabled { fill(dc, r, mix(bg, pal().accent, if i as i32 == hot { 0.55 } else { 0.25 }), sc(8)); }
        let ink = if it.disabled { mix(pal().dim, bg, 0.4) } else { pal().text };
        // icon: the shell's bitmap, or our glyph
        let s = sc(16);
        let (ix, iy) = (r.left + sc(10), (r.top + r.bottom - s) / 2);
        if it.icon != 0 {
            let mdc = CreateCompatibleDC(dc);
            let o = SelectObject(mdc, it.icon as HGDIOBJ);
            let mut bm: BITMAP = std::mem::zeroed();
            GetObjectW(it.icon as HGDIOBJ, size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _);
            if it.icon_alpha {
                let bf = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: 1 }; // AC_SRC_ALPHA
                AlphaBlend(dc, ix, iy, s, s, mdc, 0, 0, bm.bmWidth, bm.bmHeight.abs(), bf);
            } else {
                SetStretchBltMode(dc, HALFTONE);
                StretchBlt(dc, ix, iy, s, s, mdc, 0, 0, bm.bmWidth, bm.bmHeight.abs(), SRCCOPY);
            }
            SelectObject(mdc, o);
            DeleteDC(mdc);
        } else if !it.glyph.is_empty() {
            let of = SelectObject(dc, f[2] as HFONT);
            text(dc, it.glyph, rc(ix - sc(2), r.top, ix + s + sc(2), r.bottom), if on { pal().text } else { pal().dim }, DT_CENTER);
            SelectObject(dc, of);
        }
        let of = SelectObject(dc, f[if it.bold { 1 } else { 0 }] as HFONT);
        let right = r.right - sc(if it.sub.is_empty() { 12 } else { 26 });
        if !it.hint.is_empty() {
            SelectObject(dc, f[3] as HFONT); // key hints small and quiet: the names are what is read
            text(dc, &it.hint, rc(r.left, r.top, right, r.bottom), mix(pal().dim, bg, 0.35), DT_RIGHT);
            SelectObject(dc, f[if it.bold { 1 } else { 0 }] as HFONT);
        }
        text(dc, &it.text, rc(r.left + sc(36), r.top, right - sc(60).min(if it.hint.is_empty() { 0 } else { sc(60) }), r.bottom), ink, DT_LEFT);
        if !it.sub.is_empty() {
            SelectObject(dc, f[2] as HFONT);
            text(dc, "\u{E76C}", rc(r.right - sc(26), r.top, r.right - sc(6), r.bottom), pal().dim, DT_CENTER);
        }
        SelectObject(dc, of);
    }
    SelectClipRgn(dc, ptr::null_mut());
    BitBlt(wdc, 0, 0, cr.right, cr.bottom, dc, 0, 0, SRCCOPY);
    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteDC(dc);
    EndPaint(h, &ps);
}

pub unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let y = ((lp >> 16) & 0xFFFF) as i16 as i32;
    let li = level_of(h);
    match m {
        WM_PAINT => { if let Some(li) = li { paint(h, li); } else { let mut ps: PAINTSTRUCT = std::mem::zeroed(); BeginPaint(h, &mut ps); EndPaint(h, &ps); } 0 }
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => 3, // MA_NOACTIVATE: clicks do not take the focus from the search box
        WM_CTLCOLOREDIT => {
            let dc = wp as HDC;
            SetTextColor(dc, pal().text);
            SetBkColor(dc, menu_bg());
            thread_local! { static BR: Cell<isize> = const { Cell::new(0) }; }
            if BR.get() == 0 { BR.set(CreateSolidBrush(menu_bg()) as isize); }
            BR.get()
        }
        WM_MOUSEMOVE => {
            let Some(li) = li else { return 0 };
            let row = row_at(li, y).unwrap_or(-1);
            let (changed, open_other) = MENU.with_borrow_mut(|x| {
                let x = x.as_mut().unwrap();
                let changed = x.levels[li].hot != row;
                x.levels[li].hot = row;
                let open_other = x.levels.get(li + 1).is_some_and(|l| l.opened_from != row);
                (changed, open_other)
            });
            if changed {
                InvalidateRect(h, ptr::null(), 0);
                // a submenu opens after a short rest on its row; another row closes the one open
                KillTimer(h, 1);
                SetTimer(h, 1, if open_other { 250 } else { 180 }, None);
            }
            0
        }
        WM_TIMER if wp == 1 => {
            KillTimer(h, 1);
            let Some(li) = li else { return 0 };
            let hot = MENU.with_borrow(|x| x.as_ref().map_or(-1, |x| x.levels[li].hot));
            let open = MENU.with_borrow(|x| x.as_ref().and_then(|x| x.levels.get(li + 1).map(|l| l.opened_from)));
            if hot >= 0 && open != Some(hot) { open_sub(li, hot, false); }
            0
        }
        WM_LBUTTONUP => {
            if let (Some(li), Some(row)) = (li, li.and_then(|li| row_at(li, y))) { activate(li, row); }
            0
        }
        WM_MOUSEWHEEL => {
            let Some(li) = li else { return 0 };
            let d = ((wp >> 16) & 0xFFFF) as i16 as i32;
            let mut cr: RECT = std::mem::zeroed();
            GetClientRect(h, &mut cr);
            let total = row_top(li, i32::MAX / 2) + sc(PAD);
            MENU.with_borrow_mut(|x| { let l = &mut x.as_mut().unwrap().levels[li]; l.top = (l.top - d * sc(ROW) / 40).clamp(0, (total - cr.bottom).max(0)); });
            InvalidateRect(h, ptr::null(), 0);
            0
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}
