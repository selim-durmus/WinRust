//! Our own "File in use" window, instead of the shell's (which names the program at best and offers nothing to do).
//! It lists what has the items open (locks.rs): the program, its file, process ID and service, with a button that ends
//! the program or stops the service and one that opens its folder. Once nothing holds the items any more it tries again
//! by itself. The same window answers "What's using this?" from the right-click menu, without the retry.
//! The lookup and the actions run on threads (a service may take seconds to stop, a UAC prompt waits for the user);
//! they post WM_DONE back here.
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use locks::{Action, Holder};
use ui::{fill, mix, pal, rc, sc, text};
use windows_sys::Win32::{Graphics::Gdi::*, UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled}};

pub const CLASS: &str = "RxInUse";
const WM_DONE: u32 = WM_APP + 40;

const WIDTH: i32 = 560;
const PAD: i32 = 20;
const HEAD: i32 = 58;
const CARD: i32 = 74;
const GAP: i32 = 8;
const FOOT: i32 = 64;
const SHOWN: usize = 5; // programs listed at most (more: "and N more")

// what the pointer is over / what a click does
const RETRY: i32 = 1;
const CLOSE: i32 = 2;
const ACT: i32 = 100; // + holder index
const OPEN: i32 = 200; // + holder index

enum Msg { Found(Vec<Holder>), Acted(u32, Result<(), String>) }

struct Dlg {
    wnd: HWND,
    look: Vec<PathBuf>,    // what is looked up
    head: String,          // the line on top
    retry: bool,           // "File in use" (Try again) or "What's using this?"
    holders: Option<Vec<Holder>>, // None while looking
    notes: Vec<(u32, String, bool)>, // per process: what happened (text, is an error)
    busy: Option<u32>,     // the process an action runs on
    acted: bool,           // something was ended: when nothing holds the items any more, try again by itself
    hot: i32,
    done: Option<bool>,    // Some(true) = try again
    rx: Option<mpsc::Receiver<Msg>>,
}

thread_local! { static DLG: RefCell<Option<Dlg>> = const { RefCell::new(None) }; }

/// The items `failed` could not be `verb` ("deleted", "moved", "copied") because something has them open; `look` are the
/// paths to look up (the items, and for a copy or move where they were going). true = try again.
pub unsafe fn ask(owner: HWND, failed: &[PathBuf], look: Vec<PathBuf>, verb: &str) -> bool {
    let head = match failed {
        [one] => format!("\u{201C}{}\u{201D} couldn't be {verb}. It's open in:", name_of(one)),
        many => format!("{} items couldn't be {verb}. They're open in:", many.len()),
    };
    run(owner, "File in use", head, look, true)
}

/// "What's using this?": what has `paths` open, with the same buttons, no retry.
pub unsafe fn show(owner: HWND, paths: &[PathBuf]) {
    let head = match paths {
        [one] => format!("\u{201C}{}\u{201D} is open in:", name_of(one)),
        many => format!("{} items are open in:", many.len()),
    };
    run(owner, "What's using this", head, paths.to_vec(), false);
}

fn name_of(p: &Path) -> String { p.file_name().map_or_else(|| p.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned()) }

unsafe fn run(owner: HWND, title: &str, head: String, look: Vec<PathBuf>, retry: bool) -> bool {
    if DLG.with_borrow(|d| d.is_some()) { return false; } // one at a time
    let style = WS_CAPTION | WS_SYSMENU | WS_POPUP;
    let wnd = CreateWindowExW(WS_EX_DLGMODALFRAME, w(CLASS).as_ptr(), w(title).as_ptr(), style, 0, 0, 0, 0, owner, ptr::null_mut(), GetModuleHandleW(ptr::null()), ptr::null());
    if wnd.is_null() { return false; }
    ui::dark_title(wnd);
    DLG.set(Some(Dlg { wnd, look, head, retry, holders: None, notes: vec![], busy: None, acted: false, hot: -1, done: None, rx: None }));
    lookup();
    fit(owner);
    // modal: the window it belongs to waits
    let owner_on = !owner.is_null() && IsWindowEnabled(owner) != 0;
    if owner_on { EnableWindow(owner, 0); }
    ShowWindow(wnd, SW_SHOW);
    SetForegroundWindow(wnd);
    let mut msg: MSG = std::mem::zeroed();
    loop {
        if DLG.with_borrow(|d| d.as_ref().is_none_or(|d| d.done.is_some())) { break; }
        if GetMessageW(&mut msg, ptr::null_mut(), 0, 0) <= 0 { PostQuitMessage(msg.wParam as i32); break; }
        if msg.message == WM_KEYDOWN && (msg.hwnd == wnd || IsChild(wnd, msg.hwnd) != 0) {
            match msg.wParam as u32 {
                0x1B => { finish(false); continue; }         // Esc
                0x0D => { finish(retry); continue; }         // Enter: the main button
                _ => {}
            }
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    if owner_on { EnableWindow(owner, 1); }
    let d = DLG.take();
    DestroyWindow(wnd);
    if !owner.is_null() { SetForegroundWindow(owner); }
    d.and_then(|d| d.done).unwrap_or(false)
}

fn finish(retry: bool) { DLG.with_borrow_mut(|d| if let Some(d) = d { d.done = Some(retry); }); }

/// Looks up who holds the items again, on a thread.
unsafe fn lookup() {
    let Some((wnd, look)) = DLG.with_borrow(|d| d.as_ref().map(|d| (d.wnd as isize, d.look.clone()))) else { return };
    let (tx, rx) = mpsc::channel();
    DLG.with_borrow_mut(|d| if let Some(d) = d { d.rx = Some(rx); });
    std::thread::spawn(move || {
        let _ = tx.send(Msg::Found(locks::who(&look)));
        PostMessageW(wnd as HWND, WM_DONE, 0, 0);
    });
}

/// Ends or stops holder `i`, on a thread.
unsafe fn act(i: usize) {
    let Some((wnd, h, a)) = DLG.with_borrow(|d| {
        let d = d.as_ref()?;
        if d.busy.is_some() { return None; }
        let h = d.holders.as_ref()?.get(i)?.clone();
        let a = h.action()?;
        Some((d.wnd as isize, h, a))
    }) else { return };
    let (tx, rx) = mpsc::channel();
    DLG.with_borrow_mut(|d| if let Some(d) = d {
        d.busy = Some(h.pid);
        d.notes.retain(|n| n.0 != h.pid);
        d.notes.push((h.pid, if a == Action::Stop { "Stopping\u{2026}" } else { "Ending\u{2026}" }.into(), false));
        d.rx = Some(rx);
    });
    std::thread::spawn(move || {
        let _ = tx.send(Msg::Acted(h.pid, locks::act(a, &h)));
        PostMessageW(wnd as HWND, WM_DONE, 0, 0);
    });
}

/// A thread finished: show what it found or did.
unsafe fn arrived() {
    let Some(m) = DLG.with_borrow(|d| d.as_ref().and_then(|d| d.rx.as_ref()?.try_recv().ok())) else { return };
    match m {
        Msg::Found(v) => {
            let again = DLG.with_borrow_mut(|d| {
                let d = d.as_mut()?;
                let again = d.retry && d.acted && v.is_empty();
                d.holders = Some(v);
                Some(again)
            });
            // what held the items is gone: try again at once, no click needed
            if again == Some(true) { finish(true); return; }
        }
        Msg::Acted(pid, r) => {
            let ok = r.is_ok();
            DLG.with_borrow_mut(|d| if let Some(d) = d {
                d.busy = None;
                d.notes.retain(|n| n.0 != pid);
                match r { Ok(()) => d.acted = true, Err(e) => d.notes.push((pid, e, true)) }
            });
            if ok { lookup(); }
        }
    }
    let owner = DLG.with_borrow(|d| d.as_ref().map(|d| GetWindow(d.wnd, GW_OWNER)));
    if let Some(o) = owner { fit(o); }
}

/// Client height for what is shown.
fn height() -> i32 {
    let rows = DLG.with_borrow(|d| d.as_ref().and_then(|d| d.holders.as_ref().map(|h| h.len())).unwrap_or(0));
    let list = match rows { 0 => sc(CARD), n => sc(CARD) * n.min(SHOWN) as i32 + sc(GAP) * (n.min(SHOWN) as i32 - 1) + if n > SHOWN { sc(24) } else { 0 } };
    sc(HEAD) + list + sc(FOOT)
}

/// Sizes the window to what it shows, centred over `owner` (or the screen).
unsafe fn fit(owner: HWND) {
    let Some(wnd) = DLG.with_borrow(|d| d.as_ref().map(|d| d.wnd)) else { return };
    let mut wr = rc(0, 0, sc(WIDTH), height());
    AdjustWindowRectEx(&mut wr, WS_CAPTION | WS_SYSMENU | WS_POPUP, 0, WS_EX_DLGMODALFRAME);
    let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
    let mut r: RECT = std::mem::zeroed();
    if owner.is_null() || IsWindowVisible(owner) == 0 || IsIconic(owner) != 0 { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut _ as *mut _, 0); } else { GetWindowRect(owner, &mut r); }
    let mut now: RECT = std::mem::zeroed();
    GetWindowRect(wnd, &mut now);
    // once shown it keeps its top (only grows down); the first time it goes in the middle
    let y = if now.bottom > now.top { now.top } else { (r.top + r.bottom - wh) / 2 };
    SetWindowPos(wnd, ptr::null_mut(), (r.left + r.right - ww) / 2, y, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);
    InvalidateRect(wnd, ptr::null(), 0);
}

fn card_rect(i: usize) -> RECT {
    let t = sc(HEAD) + (sc(CARD) + sc(GAP)) * i as i32;
    rc(sc(PAD), t, sc(WIDTH) - sc(PAD), t + sc(CARD))
}

/// A holder's buttons: (action, open folder).
fn card_buttons(i: usize, has_act: bool) -> (Option<RECT>, RECT) {
    let c = card_rect(i);
    let (bh, mid) = (sc(32), (c.top + c.bottom) / 2);
    let open = rc(c.right - sc(12) - sc(36), mid - bh / 2, c.right - sc(12), mid + bh / 2);
    let act = has_act.then(|| rc(open.left - sc(8) - sc(112), mid - bh / 2, open.left - sc(8), mid + bh / 2));
    (act, open)
}

/// The buttons at the bottom right: (Try again, Close).
fn foot_buttons(retry: bool) -> (Option<RECT>, RECT) {
    let (b, r) = (height() - sc(FOOT) + (sc(FOOT) - sc(34)) / 2, sc(WIDTH) - sc(PAD));
    let close = rc(r - sc(96), b, r, b + sc(34));
    (retry.then(|| rc(close.left - sc(8) - sc(112), b, close.left - sc(8), b + sc(34))), close)
}

fn inside(r: &RECT, x: i32, y: i32) -> bool { x >= r.left && x < r.right && y >= r.top && y < r.bottom }

/// What is at client point (x, y).
fn hit(x: i32, y: i32) -> i32 {
    DLG.with_borrow(|d| {
        let Some(d) = d.as_ref() else { return -1 };
        let (retry, close) = foot_buttons(d.retry);
        if retry.is_some_and(|r| inside(&r, x, y)) { return RETRY; }
        if inside(&close, x, y) { return CLOSE; }
        for (i, h) in d.holders.iter().flatten().take(SHOWN).enumerate() {
            let (a, o) = card_buttons(i, h.action().is_some());
            if a.is_some_and(|r| inside(&r, x, y)) && d.busy.is_none() { return ACT + i as i32; }
            if inside(&o, x, y) && h.exe.is_some() { return OPEN + i as i32; }
        }
        -1
    })
}

/// Opens the folder of holder `i`'s program in a new window.
fn open_folder(i: usize) {
    let dir = DLG.with_borrow(|d| d.as_ref().and_then(|d| d.holders.as_ref()?.get(i)?.exe.as_ref()?.parent().map(PathBuf::from)));
    if let Some(dir) = dir { let _ = std::process::Command::new(env::current_exe().unwrap_or_default()).arg(dir).spawn(); }
}

unsafe fn button(dc: HDC, r: RECT, label: &str, hot: bool, main: bool, face: HFONT) {
    let base = if main { pal().accent } else { mix(pal().bg, pal().text, 0.08) };
    fill(dc, r, if hot { mix(base, pal().text, 0.12) } else { base }, sc(10));
    let of = SelectObject(dc, face as HGDIOBJ);
    text(dc, label, r, if main { 0x00FF_FFFF } else { pal().text }, DT_CENTER);
    SelectObject(dc, of);
}

unsafe fn paint(h: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(h, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right.max(1), cr.bottom.max(1)));
    let old = SelectObject(dc, bmp as HGDIOBJ);
    let bg = CreateSolidBrush(pal().bg);
    FillRect(dc, &cr, bg);
    DeleteObject(bg as HGDIOBJ);
    SetBkMode(dc, TRANSPARENT as i32);
    let fonts = [ui::font(15, 500, ui::ui_face()), ui::font(14, 600, ui::ui_face()), ui::font(12, 400, ui::ui_face()), ui::font(14, 400, winrust_common::style::icon_face()), ui::font(13, 500, ui::ui_face())];
    let [f_head, f_name, f_small, f_icon, f_btn] = fonts.map(|f| f as HFONT);
    let of = SelectObject(dc, f_head as HGDIOBJ);
    DLG.with_borrow(|d| {
        let Some(d) = d.as_ref() else { return };
        let (l, r) = (sc(PAD), cr.right - sc(PAD));
        text(dc, &d.head, rc(l + sc(2), 0, r, sc(HEAD) - sc(6)), pal().text, DT_LEFT);
        match &d.holders {
            None => {
                fill(dc, card_rect(0), ui::listbg(), sc(14));
                SelectObject(dc, f_small as HGDIOBJ);
                text(dc, "Looking\u{2026}", inset(card_rect(0)), pal().dim, DT_LEFT);
            }
            Some(v) if v.is_empty() => {
                fill(dc, card_rect(0), ui::listbg(), sc(14));
                SelectObject(dc, f_small as HGDIOBJ);
                let t = if d.acted { "It is free now." } else { "No program was found that has it open." };
                text(dc, t, inset(card_rect(0)), pal().dim, DT_LEFT);
            }
            Some(v) => {
                for (i, hd) in v.iter().take(SHOWN).enumerate() {
                    let c = card_rect(i);
                    fill(dc, c, ui::listbg(), sc(14));
                    let (act, open) = card_buttons(i, hd.action().is_some());
                    let tr = act.map_or(open.left, |a| a.left) - sc(10);
                    let (x, t) = (c.left + sc(16), c.top + sc(10));
                    SelectObject(dc, f_name as HGDIOBJ);
                    text(dc, &hd.title(), rc(x, t, tr, t + sc(20)), pal().text, DT_LEFT);
                    SelectObject(dc, f_small as HGDIOBJ);
                    let mut line = format!("{} \u{00B7} PID {}", hd.exe_name(), hd.pid);
                    if !hd.services.is_empty() { line += &format!(" \u{00B7} service {}", hd.services.iter().map(|s| s.0.as_str()).collect::<Vec<_>>().join(", ")); }
                    text(dc, &line, rc(x, t + sc(20), tr, t + sc(38)), pal().dim, DT_LEFT);
                    // third line: what happened to it, else where its file is (or why there is no button)
                    let note = d.notes.iter().find(|n| n.0 == hd.pid);
                    let (third, ink) = match note {
                        Some((_, s, err)) => (s.clone(), if *err { 0x0050_50E8 } else { pal().accent }),
                        None if hd.critical => ("Part of Windows: it can't be ended".into(), mix(pal().dim, pal().bg, 0.3)),
                        None if hd.services.len() > 1 => ("Several services share this process".into(), mix(pal().dim, pal().bg, 0.3)),
                        None => (hd.exe.as_ref().and_then(|e| e.parent()).map_or(String::new(), |p| p.to_string_lossy().into_owned()), mix(pal().dim, pal().bg, 0.3)),
                    };
                    text(dc, &third, rc(x, t + sc(38), tr, t + sc(56)), ink, DT_LEFT | DT_PATH_ELLIPSIS);
                    if let (Some(a), Some(kind)) = (act, hd.action()) {
                        let busy = d.busy == Some(hd.pid);
                        let label = match (kind, busy) { (_, true) => "\u{2026}", (Action::Stop, _) => "Stop service", (Action::End, _) => "End process" };
                        button(dc, a, label, d.hot == ACT + i as i32, false, f_btn);
                    }
                    if hd.exe.is_some() {
                        if d.hot == OPEN + i as i32 { fill(dc, open, mix(ui::listbg(), pal().text, 0.08), sc(10)); }
                        SelectObject(dc, f_icon as HGDIOBJ);
                        text(dc, "\u{E838}", open, pal().dim, DT_CENTER); // open folder
                    }
                }
                if v.len() > SHOWN {
                    SelectObject(dc, f_small as HGDIOBJ);
                    let y = card_rect(SHOWN - 1).bottom;
                    text(dc, &format!("and {} more", v.len() - SHOWN), rc(l + sc(16), y, r, y + sc(24)), pal().dim, DT_LEFT);
                }
            }
        }
        let (retry, close) = foot_buttons(d.retry);
        if let Some(rr) = retry { button(dc, rr, "Try again", d.hot == RETRY, true, f_btn); }
        button(dc, close, "Close", d.hot == CLOSE, !d.retry, f_btn);
    });
    BitBlt(wdc, 0, 0, cr.right, cr.bottom, dc, 0, 0, SRCCOPY);
    SelectObject(dc, of);
    for f in fonts { DeleteObject(f as HGDIOBJ); }
    SelectObject(dc, old);
    DeleteObject(bmp as HGDIOBJ);
    DeleteDC(dc);
    EndPaint(h, &ps);
}

fn inset(r: RECT) -> RECT { rc(r.left + sc(16), r.top, r.right - sc(16), r.bottom) }

pub unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
    match m {
        WM_PAINT => { paint(h); 0 }
        WM_ERASEBKGND => 1,
        WM_DONE => { arrived(); 0 }
        WM_MOUSEMOVE => {
            let k = hit(x, y);
            let changed = DLG.with_borrow_mut(|d| d.as_mut().is_some_and(|d| std::mem::replace(&mut d.hot, k) != k));
            if changed { InvalidateRect(h, ptr::null(), 0); }
            0
        }
        WM_LBUTTONUP => {
            match hit(x, y) {
                RETRY => finish(true),
                CLOSE => finish(false),
                k if k >= OPEN => open_folder((k - OPEN) as usize),
                k if k >= ACT => act((k - ACT) as usize),
                _ => {}
            }
            InvalidateRect(h, ptr::null(), 0);
            0
        }
        WM_SETCURSOR if (lp & 0xFFFF) as u32 == HTCLIENT => {
            let mut p: POINT = std::mem::zeroed();
            GetCursorPos(&mut p);
            ScreenToClient(h, &mut p);
            SetCursor(LoadCursorW(ptr::null_mut(), if hit(p.x, p.y) > 0 { IDC_HAND } else { IDC_ARROW }));
            1
        }
        WM_CLOSE => { finish(false); 0 }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}
