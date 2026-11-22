// topbar: replaces the Windows taskbar with a panel at the bottom of the screen (the name stayed from when it sat on top),
// drawn in the Windows 11 taskbar palette: Start, pinned apps and open windows in the center; tray apps, language, wifi,
// volume, battery and date/time on the right. `--restore` brings the normal taskbar back (used by uninstall).
#![windows_subsystem = "windows"]
#![allow(unsafe_op_in_unsafe_fn)]
use std::{
    cell::{Cell, RefCell},
    mem::{size_of, zeroed},
    os::windows::process::CommandExt,
    path::PathBuf,
    process::Command,
    ptr::{null, null_mut},
    thread,
    sync::Mutex,
    collections::HashMap,
    sync::atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed},
};
#[macro_use]
extern crate winrust_common;
use winrust_common as i18n;
use winrust_common::style::{Style, card, on_accent, pal, rgb, style, ui_face};
mod clip;
mod edit;
mod fly;
mod quick;
use windows::{
    Win32::{
        Media::Audio::{Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender},
        System::Com::{CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile, STGM},
        UI::Shell::{IShellLinkW, ShellLink},
    },
    core::{Interface, PCWSTR},
};
use windows_sys::Win32::{
    Foundation::*,
    Globalization::{GetDateFormatW, GetLocaleInfoW, GetTimeFormatW},
    Graphics::{
        Dwm::DwmGetWindowAttribute,
        Gdi::*,
        GdiPlus::{
            FillModeAlternate, GdipAddPathArc, GdipClosePathFigure, GdipCreateFromHDC, GdipCreatePath, GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath, GdipFillPath, GdipSetPixelOffsetMode,
            GdipSetSmoothingMode, GdiplusStartup, GdiplusStartupInput, GpBrush, GpGraphics, GpPath, GpSolidFill, PixelOffsetModeHalf, SmoothingModeAntiAlias,
            GdipCreatePen1, GdipDeletePen, GdipDrawArc, GdipDrawLine, GdipFillEllipse, GdipSetPenEndCap, GdipSetPenStartCap, GpPen, LineCapRound, UnitPixel,
        },
    },
    NetworkManagement::WiFi::*,
    System::{
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS},
        SystemInformation::{GetLocalTime, GetTickCount64},
        Threading::{AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess},
    },
    UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
};

/// A corner radius in logical px, 0 in the square styles.
fn rr(v: i32) -> i32 {
    if style().square() { 0 } else { sc(v) }
}
/// The bar sits at the top of the screen (tiling) instead of the bottom.
fn at_top() -> bool {
    style() == Style::Tiling
}
/// Gap under the floating dock.
fn dock_gap() -> i32 {
    if style() == Style::Dock { sc(8) } else { 0 }
}
/// Screen x and width of the bar: the whole screen, or the dock in the middle.
unsafe fn bar_x_w() -> (i32, i32) {
    let cx = GetSystemMetrics(SM_CXSCREEN);
    if style() == Style::Dock {
        let w = (cx * 3 / 5).max(sc(720)).min(cx);
        ((cx - w) / 2, w)
    } else {
        (0, cx)
    }
}
/// Screen y of the bar's current top edge, slid away by the show/hide animation.
unsafe fn bar_y() -> i32 {
    if at_top() { -bar_off() } else { bar_top() + bar_off() }
}
/// Screen y of a popup of height `h` next to the bar: above it, or below it when the bar is at the top.
unsafe fn pop_y(h: i32) -> i32 {
    if at_top() { bar_h() + sc(8) } else { bar_top() - sc(8) - h }
}
const GOOD: u32 = winrust_common::style::GOOD;
const BAD: u32 = winrust_common::style::BAD;
const WARN: u32 = winrust_common::style::WARN;
const CALLBACK: u32 = WM_APP + 1;
const WM_MOUSELEAVE: u32 = 0x02A3; // not exported under the enabled features
const NO_WINDOW: u32 = 0x0800_0000;
// Windows 11 taskbar metrics (its XAML: MediumTaskbarButtonExtent 44, ControlLargeCornerRadius 4, RunningIndicator
// Height 3): a 48 px bar, 44 px button slots side by side, a 40x40 highlight inset 2 / 4 px, a 24 px icon in the middle
const BAR_H: i32 = 48;
const BAR_H_THIN: i32 = 40; // the tiling style's status strip at the top
const BTN_W: i32 = 44;
// hit-test results. Buttons (Start, pins, windows) are >= 0 in left-to-right order; tray apps are TAPP + index.
const NONE: i32 = -1;
const SEARCH: i32 = -10;
const CLOCK: i32 = -4;
const LANG: i32 = -6;
const WIFI_I: i32 = -7;
const VOL_I: i32 = -8;
const BAT_I: i32 = -9;
const TAPP: i32 = -1000;
// widths of the right-hand group's blocks, before scaling (sc() is applied where used)
const SYS_CELL: i32 = 34; // one icon in the wifi / volume pill
const BAT_W: i32 = 72; // the battery's icon and percentage in that pill (only with a battery)
const SYS_GRP: i32 = -11; // hover key of the one pill around wifi + volume + battery (they open the same panel)
const WIN_START: u32 = WM_APP + 10; // posted by the keyboard hook: Win key pressed alone
const ALT_TAB: u32 = WM_APP + 11; // posted by the keyboard hook: Alt+Tab (wp 1 = Shift held: backwards)
const ALT_UP: u32 = WM_APP + 12; // posted by the keyboard hook: Alt released while switching (wp 1 = Esc: cancel)
static SWITCHING: AtomicBool = AtomicBool::new(false); // Alt+Tab switcher open (the hook thread reads it)
static FULLSCREEN: AtomicBool = AtomicBool::new(false);
static DESK_HIDE: AtomicBool = AtomicBool::new(false); // the desktop in front and the pointer away from the bar for 2 s
static WIN_ALONE: AtomicBool = AtomicBool::new(false);
static BAR: AtomicIsize = AtomicIsize::new(0); // the bar window, for the hook thread
const VOLKEY: u32 = WM_APP + 14; // posted by the keyboard hook: a volume key (wp 0 up, 1 down, 2 mute)
const SNIP: u32 = WM_APP + 13; // posted by the keyboard hook: PrtSc / Win+Shift+S with no explorer to handle them
const NEXT_LANG: u32 = WM_APP + 15; // posted by the keyboard hook: Win+Space with no explorer to handle it
const STYLE_MENU: u32 = WM_APP + 16; // posted by the empty-spot menu's "Bar style…" (wp: x to open the Style menu at)
static EXPLORER_UP: AtomicBool = AtomicBool::new(true); // explorer's taskbar exists (it handles PrtSc itself then); refreshed by the bar

struct Pin {
    lnk: Vec<u16>,
    target: String, // lower-case exe path, "" if unknown
    icon: HICON,
}
struct Btn {
    pin: Option<usize>,
    wins: Vec<HWND>,
    icon: HICON, // the window's icon for unpinned buttons, looked up on refresh, not every animation frame
}

/// One notification-area icon, as an app registered it through Shell_NotifyIcon (we own the "Shell_TrayWnd" they talk to).
struct TIcon {
    hwnd: HWND, // the app's callback window
    uid: u32,
    guid: [u8; 16], // zero unless the app identifies the icon by GUID
    icon: HICON,    // our own copy
    isz: i32,       // its width in px as the app made it (often the small-icon size of the app's DPI)
    cb: u32,        // uCallbackMessage
    ver: u32,       // NIM_SETVERSION value; >= 4 means the NOTIFYICON_VERSION_4 message format
    hidden: bool,
    tip: String,
    key: String, // owner exe file name, lower-case: what the "keep on the bar" choice is stored under
}
const WM_COPYDATA: u32 = 0x4A;
#[repr(C)]
struct Cds {
    dw: usize,
    cb: u32,
    p: *const u8,
}
static USAGE: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None); // launches per shortcut name, lower-case

fn appdata_file(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join(name)
}
fn uses(name: &str) -> u32 {
    USAGE
        .lock()
        .unwrap()
        .get_or_insert_with(|| std::fs::read_to_string(appdata_file("topbar-usage.txt")).unwrap_or_default().lines().filter_map(|l| l.rsplit_once('\t')).filter_map(|(k, n)| Some((k.to_string(), n.parse().ok()?))).collect())
        .get(name)
        .copied()
        .unwrap_or(0)
}
/// Counts one launch of the shortcut at `path` (by file name, so the same app pinned and in Start counts together).
fn bump(path: &str) {
    bump_name(&std::path::Path::new(path).file_stem().unwrap_or_default().to_string_lossy());
}
/// Counts one launch of the app called `name` (Start search ranks by it).
fn bump_name(name: &str) {
    let name = name.to_lowercase();
    uses(&name); // loads the file first
    let mut g = USAGE.lock().unwrap();
    let m = g.as_mut().unwrap();
    *m.entry(name).or_insert(0) += 1;
    let _ = std::fs::write(appdata_file("topbar-usage.txt"), m.iter().map(|(k, n)| format!("{k}\t{n}\n")).collect::<String>());
}
fn lnk_uses(lnk: &std::path::Path) -> u32 {
    uses(&lnk.file_stem().unwrap_or_default().to_string_lossy().to_lowercase())
}

thread_local! {
    static HOST: Cell<HWND> = const { Cell::new(null_mut()) };
    static TIPWND: Cell<HWND> = const { Cell::new(null_mut()) };
    static SWWND: Cell<HWND> = const { Cell::new(null_mut()) }; // the Alt+Tab switcher
    static SW: RefCell<(Vec<HWND>, usize)> = const { RefCell::new((Vec::new(), 0)) }; // its windows (z-order) and selection
    static TIP: RefCell<String> = const { RefCell::new(String::new()) };
    static PINS: RefCell<Vec<Pin>> = const { RefCell::new(Vec::new()) };
    static BTNS: RefCell<Vec<Btn>> = const { RefCell::new(Vec::new()) }; // [0] is always Start (pin None, no wins)
    static TAPPS: RefCell<Vec<TIcon>> = const { RefCell::new(Vec::new()) };
    static SHOWN: RefCell<Option<Vec<String>>> = const { RefCell::new(None) }; // tray icons kept on the bar (by exe); None = none: every tray icon lives in the chevron flyout
    static TRAYFLY: Cell<HWND> = const { Cell::new(null_mut()) };
    static TF_ANIM: Cell<(u64, i32, i32)> = const { Cell::new((0, 0, 0)) }; // tray flyout: when it opened, its final x, y
    static TF_MENU: Cell<(u64, u32, bool)> = const { Cell::new((0, 0, false)) }; // tray flyout right click: when, the icon owner's pid, its menu seen
    static FHOT: Cell<i32> = const { Cell::new(-1) };
    static HOT: Cell<i32> = const { Cell::new(NONE) };
    static ORDER: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) }; // unpinned windows in the order their buttons were added
    static HV: RefCell<HashMap<i32, (f32, u64)>> = RefCell::new(HashMap::new()); // hover fade per element: (0..1, last update ms)
    static BUSY: Cell<bool> = const { Cell::new(false) }; // a fade is still running: the painter asks for another frame
    static BAR_P: Cell<f32> = const { Cell::new(0.0) }; // bar slide: 0 = off screen, 1 = in place
    static BAR_T: Cell<u64> = const { Cell::new(0) };
    static DESK_T: Cell<u64> = const { Cell::new(0) }; // last moment the bar had a reason to stay on the desktop
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
    static OLD_STATE: Cell<usize> = const { Cell::new(ABS_ALWAYSONTOP as usize) };
    static BAT: Cell<(i32, bool)> = const { Cell::new((-1, false)) }; // (percent or -1 = no battery, on AC)
    static CLOCK_W: Cell<i32> = const { Cell::new(0) }; // the clock block's width as last measured from its text; 0 = not yet
    static VOL: Cell<(i32, bool)> = const { Cell::new((-1, false)) }; // (percent or -1 = unknown, muted)
    static WIFI: Cell<i32> = const { Cell::new(-1) }; // signal quality 0..100, -1 = not on wifi
    static INET: Cell<bool> = const { Cell::new(true) }; // Windows sees a way to the internet (any adapter)
    static TICK: Cell<u32> = const { Cell::new(0) };
    static EP: RefCell<Option<IAudioEndpointVolume>> = const { RefCell::new(None) };
    static TB_MSG: Cell<u32> = const { Cell::new(0) }; // "TaskbarCreated"
    static SH_MSG: Cell<u32> = const { Cell::new(0) }; // "SHELLHOOK": window events for the shell (we are the shell hook window without explorer)
    static EXP_TRAY: Cell<HWND> = const { Cell::new(null_mut()) }; // explorer's taskbar we registered with
    static APP_FG: Cell<HWND> = const { Cell::new(null_mut()) }; // last foreground window that is not ours: the language button acts on it
    static LANG_TRY: Cell<(u32, usize)> = const { Cell::new((0, 0)) }; // (thread, layout before) of the last switch, checked by timer 7
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
fn sc(v: i32) -> i32 {
    (v as f32 * SCALE.get()) as i32
}
fn bar_h() -> i32 {
    sc(if at_top() { BAR_H_THIN } else { BAR_H })
}
/// Screen y of the bar's top edge when it is in place (bottom of the primary monitor, or its top in the tiling style).
unsafe fn bar_top() -> i32 {
    if at_top() { 0 } else { GetSystemMetrics(SM_CYSCREEN) - bar_h() - dock_gap() }
}

// ---------- taskbar / appbar ----------

/// First top-level window of the class that is not our own tray host (explorer's real taskbar).
unsafe fn find_other(class: &str) -> HWND {
    let (c, mut h) = (wide(class), null_mut());
    loop {
        h = FindWindowExW(null_mut(), h, c.as_ptr(), null());
        if h.is_null() || h != HOST.get() {
            return h;
        }
    }
}

unsafe fn tray() -> [HWND; 2] {
    [find_other("Shell_TrayWnd"), find_other("Shell_SecondaryTrayWnd")]
}

/// Tells every app to re-register its tray icon with whichever Shell_TrayWnd is found first.
unsafe fn announce() {
    PostMessageW(HWND_BROADCAST, RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), 0, 0);
}

fn hide_onedrive() -> bool {
    // topbar.cfg sits next to the exe (the launcher writes it); no file or no line = OneDrive hidden
    std::env::current_exe().ok().and_then(|e| std::fs::read_to_string(e.with_file_name("topbar.cfg")).ok()).is_none_or(|c| !c.contains("hide_onedrive=0"))
}

/// WM_COPYDATA (dwData 1) from Shell_NotifyIcon: u32 magic, u32 NIM_*, then the platform-neutral NOTIFYICONDATA
/// (32-bit handles): hWnd@12 uID@16 uFlags@20 callback@24 hIcon@28 dwState@288 mask@292 version@808 guid@944.
unsafe fn tray_msg(d: &[u8]) {
    let u = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let (op, flags, uid) = (u(4), u(20), u(16));
    let h = u(12) as i32 as isize as HWND;
    let guid: [u8; 16] = if flags & 0x20 != 0 { d[944..960].try_into().unwrap() } else { [0; 16] };
    TAPPS.with_borrow_mut(|t| {
        let pos = t.iter().position(|x| x.hwnd == h && x.uid == uid && x.guid == guid);
        match (op, pos) {
            (2, Some(i)) => {
                DestroyIcon(t.remove(i).icon);
            }
            (4, Some(i)) => t[i].ver = u(808),
            (0 | 1, _) => {
                let i = match pos {
                    Some(i) => i,
                    None if op == 1 => return, // modify of an icon we never saw
                    None => {
                        let exe = exe_of(h);
                        let n = exe.rsplit('\\').next().unwrap_or("");
                        if n == "explorer.exe" || n == "topbar.exe" || (n == "onedrive.exe" && hide_onedrive()) {
                            return; // the system's own icons are drawn by the bar itself
                        }
                        t.push(TIcon { hwnd: h, uid, guid, icon: null_mut(), isz: 0, cb: 0, ver: 0, hidden: false, tip: String::new(), key: n.to_string() });
                        t.len() - 1
                    }
                };
                let x = &mut t[i];
                if flags & 1 != 0 {
                    x.cb = u(24);
                }
                if flags & 2 != 0 {
                    let ic = CopyIcon(u(28) as i32 as isize as HICON);
                    if !ic.is_null() {
                        if !x.icon.is_null() {
                            DestroyIcon(x.icon);
                        }
                        x.icon = ic;
                        x.isz = icon_width(ic);
                    }
                }
                if flags & 4 != 0 {
                    let w: Vec<u16> = d[32..288].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
                    x.tip = String::from_utf16_lossy(&w);
                }
                if flags & 8 != 0 && u(292) & 1 != 0 {
                    x.hidden = u(288) & 1 != 0; // NIS_HIDDEN
                }
            }
            _ => {}
        }
    });
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_COPYDATA {
        let c = &*(lp as *const Cds);
        if c.dw == 1 && c.cb >= 964 {
            tray_msg(std::slice::from_raw_parts(c.p, c.cb as usize));
            return 1;
        }
        // SHAppBarMessage (our own bar's work-area reservation, ABM_SETSTATE...) also goes to "Shell_TrayWnd": hand it to explorer's
        // with a timeout: a hung explorer must not freeze the bar
        let mut r = 0usize;
        SendMessageTimeoutW(find_other("Shell_TrayWnd"), msg, wp, lp, SMTO_ABORTIFHUNG, 2000, &mut r);
        return r as LRESULT;
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// Width in px of the bitmap inside `ic` (0 if it cannot be read).
unsafe fn icon_width(ic: HICON) -> i32 {
    let mut ii: ICONINFO = zeroed();
    if GetIconInfo(ic, &mut ii) == 0 {
        return 0;
    }
    let mut bm: BITMAP = zeroed();
    let src = if !ii.hbmColor.is_null() { ii.hbmColor } else { ii.hbmMask };
    GetObjectW(src, size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _);
    for b in [ii.hbmColor, ii.hbmMask] {
        if !b.is_null() {
            DeleteObject(b);
        }
    }
    // a mask-only (monochrome) icon stacks AND and XOR masks: the bitmap is twice as tall, not wider
    bm.bmWidth
}

/// The size to draw a tray icon in the flyout: the app's own size when it is near the slot (sharp, no resampling),
/// otherwise the slot. Stretching e.g. 24 px to 30 px with DrawIconEx doubles every fourth row and column: jagged.
fn fly_icon_px(native: i32) -> i32 {
    if (sc(16)..=sc(24)).contains(&native) { native } else if native > sc(24) { sc(20) } else { sc(16) }
}

/// Posts the same messages explorer would for a click on the icon; the app shows its own window or menu.
unsafe fn tray_click(t: (HWND, u32, u32, u32), left: bool) {
    let (hwnd, uid, cb, ver) = t;
    if cb == 0 {
        return;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    AllowSetForegroundWindow(pid); // so the app can bring its popup menu to the front
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    let post = |ev: u32| {
        if ver >= 4 {
            PostMessageW(hwnd, cb, ((pt.x as u32 & 0xffff) | (pt.y as u32) << 16) as usize, (ev | (uid & 0xffff) << 16) as isize);
        } else {
            PostMessageW(hwnd, cb, uid as usize, ev as isize);
        }
    };
    match (left, ver >= 4) {
        (true, true) => post(0x400), // NIN_SELECT
        (true, false) => [WM_LBUTTONDOWN, WM_LBUTTONUP].into_iter().for_each(post),
        (false, true) => post(WM_CONTEXTMENU),
        (false, false) => [WM_RBUTTONDOWN, WM_RBUTTONUP].into_iter().for_each(post),
    }
}

/// Indices into TAPPS of the icons kept on the bar; the rest live in the chevron flyout.
fn bar_idx() -> Vec<usize> {
    let shown = SHOWN.with_borrow(|s| s.clone());
    TAPPS.with_borrow(|t| t.iter().enumerate().filter(|(_, x)| !x.hidden && shown.as_ref().is_some_and(|s| s.contains(&x.key))).map(|(i, _)| i).collect())
}
/// Bar slots of the tray: the chevron plus the icons kept on the bar.
fn tray_count() -> usize {
    bar_idx().len() + 1
}

/// Indices into TAPPS of the icons only the flyout shows (everything not kept on the bar).
fn over_idx() -> Vec<usize> {
    let bar = bar_idx();
    TAPPS.with_borrow(|t| (0..t.len()).filter(|i| !t[*i].hidden && !bar.contains(i)).collect())
}

fn toggle_shown(key: &str) {
    let mut cur = SHOWN.with_borrow(|s| s.clone()).unwrap_or_else(|| TAPPS.with_borrow(|t| bar_idx().into_iter().map(|i| t[i].key.clone()).collect()));
    match cur.iter().position(|k| k == key) {
        Some(p) => drop(cur.remove(p)),
        None => cur.push(key.to_string()),
    }
    let _ = std::fs::write(appdata_file("topbar-tray.txt"), cur.join("\n"));
    SHOWN.set(Some(cur));
}

// ---------- tray overflow flyout (the chevron): every icon, click / right click work as in the real tray ----------

const FCELL: i32 = 38;

/// (columns, window width, window height) for the current number of icons: just the icon grid with even padding,
/// plus a text line only when there is nothing to show.
fn fly_size() -> (usize, i32, i32) {
    let real = over_idx().len();
    let n = real.max(1);
    let cols = n.min(6);
    let note = if real == 0 { sc(26) } else { 0 };
    (cols, sc(FCELL) * cols as i32 + sc(16), sc(FCELL) * n.div_ceil(cols) as i32 + sc(16) + note)
}

unsafe fn fly_toggle() {
    let w = TRAYFLY.get();
    if IsWindowVisible(w) != 0 {
        ShowWindow(w, SW_HIDE);
        InvalidateRect(BAR.load(Relaxed) as HWND, null(), 0); // the bar's chevron turns with it
        return;
    }
    let (_, fw, fh) = fly_size();
    FHOT.set(-1);
    TF_MENU.set((0, 0, false));
    KillTimer(w, 2);
    let scr = GetSystemMetrics(SM_CXSCREEN);
    let (bx, bw) = bar_x_w();
    let cx = bx + layout(bw).apps.0 + sc(12); // above the chevron
    SetWindowRgn(w, CreateRoundRectRgn(0, 0, fw + 1, fh + 1, rr(12), rr(12)), 1);
    let (x, y) = ((cx - fw / 2).clamp(sc(8), scr - fw - sc(8)), pop_y(fh));
    TF_ANIM.set((now_ms(), x, y));
    SetLayeredWindowAttributes(w, 0, 0, LWA_ALPHA); // tf_step fades it in
    SetWindowPos(w, HWND_TOPMOST, x, y + tf_slide(0.0), fw, fh, SWP_SHOWWINDOW);
    InvalidateRect(BAR.load(Relaxed) as HWND, null(), 0); // the bar's chevron turns with it
    SetTimer(w, 1, 15, None);
    SetForegroundWindow(w); // so that WM_ACTIVATE tells us when the user clicks elsewhere
}

/// How long after a right click in the flyout its app's menu may take to show up (slow apps answer late).
const TF_MENU_WAIT: u64 = 1500;

/// An app's tray menu is on screen: a standard popup menu (class #32768), or a visible captionless popup of the
/// icon's process in front (apps that draw their own menu).
unsafe fn app_menu_up(pid: u32) -> bool {
    let menu = FindWindowW(wide("#32768").as_ptr(), null());
    if !menu.is_null() && IsWindowVisible(menu) != 0 {
        return true;
    }
    let fg = GetForegroundWindow();
    let mut fpid = 0u32;
    GetWindowThreadProcessId(fg, &mut fpid);
    let st = GetWindowLongW(fg, GWL_STYLE) as u32;
    pid != 0 && fpid == pid && IsWindowVisible(fg) != 0 && st & WS_POPUP != 0 && st & WS_CAPTION != WS_CAPTION
}

/// How far the tray flyout still is from its place at opening progress `e` (0..1): it rises toward the bar's far side.
unsafe fn tf_slide(e: f32) -> i32 {
    let d = ((1.0 - e) * sc(14) as f32) as i32;
    if at_top() { -d } else { d }
}

/// Opening progress 0..1 of the tray flyout, eased, and of its icon `n` (each one starts 25 ms after the previous).
unsafe fn tf_progress(n: i32) -> f32 {
    let age = now_ms().saturating_sub(TF_ANIM.get().0) as f32;
    ease(((age - 25.0 * n as f32) / 200.0).clamp(0.0, 1.0))
}

unsafe extern "system" fn fly_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let cell_at = |lp: LPARAM| -> i32 {
        let (x, y) = ((lp & 0xffff) as i16 as i32 - sc(8), ((lp >> 16) & 0xffff) as i16 as i32 - sc(8));
        let (cols, _, _) = fly_size();
        let n = over_idx().len() as i32;
        if x < 0 || y < 0 || x >= sc(FCELL) * cols as i32 {
            return -1;
        }
        let i = (y / sc(FCELL)) * cols as i32 + x / sc(FCELL);
        if i < n { i } else { -1 }
    };
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let wdc = BeginPaint(hwnd, &mut ps);
            let mut r: RECT = zeroed();
            GetClientRect(hwnd, &mut r);
            // every frame is built off screen and copied in one go: drawing straight on the window showed the bare
            // panel for a moment on each animation frame, so the icons flashed while they came in
            let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, r.right, r.bottom));
            let obmp = SelectObject(dc, bmp);
            fill(dc, r, pal().line, sc(12));
            fill(dc, RECT { left: 1, top: 1, right: r.right - 1, bottom: r.bottom - 1 }, pal().bg, sc(12));
            let (cols, _, _) = fly_size();
            let f = tip_font();
            let of = SelectObject(dc, f);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, pal().dim);
            let over = over_idx();
            let mut icons: Vec<(i32, i32, i32, HICON, f32)> = Vec::new(); // icon's top left, size, icon, opening progress
            TAPPS.with_borrow(|t| {
                for (n, ti) in over.iter().map(|&i| &t[i]).enumerate() {
                    let (x, y) = (sc(8) + (n % cols) as i32 * sc(FCELL), sc(8) + (n / cols) as i32 * sc(FCELL));
                    if FHOT.get() == n as i32 {
                        fill(dc, RECT { left: x, top: y, right: x + sc(FCELL), bottom: y + sc(FCELL) }, pal().hover, sc(6));
                    }
                    if !ti.icon.is_null() {
                        let sz = fly_icon_px(ti.isz);
                        let c = sc(FCELL) / 2; // centred in its cell
                        icons.push((x + c - sz / 2, y + c - sz / 2, sz, ti.icon, tf_progress(n as i32)));
                    }
                }
            });
            if icons.iter().all(|i| i.4 >= 1.0) {
                for &(x, y, sz, ic, _) in &icons {
                    DrawIconEx(dc, x, y, ic, sz, sz, 0, null_mut(), DI_NORMAL);
                }
            } else {
                // opening: each icon fades in and rises into place, one after another. Always drawn at its final size:
                // stretching a 16-32 px tray icon to a new size every frame looked choppy. The icons go on a copy of the
                // panel first, then each one's square is blended over the panel at its own progress.
                let (idc, ibmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, r.right, r.bottom));
                let oi = SelectObject(idc, ibmp);
                BitBlt(idc, 0, 0, r.right, r.bottom, dc, 0, 0, SRCCOPY);
                for &(x, y, sz, ic, _) in &icons {
                    DrawIconEx(idc, x, y, ic, sz, sz, 0, null_mut(), DI_NORMAL);
                }
                for &(x, y, sz, _, p) in &icons {
                    if p >= 1.0 {
                        BitBlt(dc, x, y, sz, sz, idc, x, y, SRCCOPY);
                    } else if p > 0.0 {
                        let rise = tf_slide(1.0 - (1.0 - p) * 0.4); // the last 40% of the flyout's own slide, ~6 px
                        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: (255.0 * p) as u8, AlphaFormat: 0 };
                        AlphaBlend(dc, x, y + rise, sz, sz, idc, x, y, sz, sz, blend);
                    }
                }
                SelectObject(idc, oi);
                DeleteObject(ibmp);
                DeleteDC(idc);
            }
            if over.is_empty() {
                // the only text left: nothing in the flyout (its own line, see fly_size)
                let mut tr = RECT { left: sc(10), top: r.bottom - sc(26), right: r.right - sc(10), bottom: r.bottom - sc(4) };
                DrawTextW(dc, wide("No more icons").as_ptr(), -1, &mut tr, DT_LEFT | DT_NOPREFIX | DT_SINGLELINE | DT_END_ELLIPSIS | DT_VCENTER);
            }
            SelectObject(dc, of);
            DeleteObject(f);
            BitBlt(wdc, 0, 0, r.right, r.bottom, dc, 0, 0, SRCCOPY);
            SelectObject(dc, obmp);
            DeleteObject(bmp);
            DeleteDC(dc);
            EndPaint(hwnd, &ps);
            0
        }
        WM_ERASEBKGND => 1, // WM_PAINT covers every pixel
        WM_MOUSEMOVE => {
            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
            if FHOT.replace(cell_at(lp)) != cell_at(lp) {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            FHOT.set(-1);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            let i = cell_at(lp);
            let ti = over_idx().get(i.max(0) as usize).and_then(|&k| TAPPS.with_borrow(|t| t.get(k).map(|x| ((x.hwnd, x.uid, x.cb, x.ver), x.key.clone()))));
            if let (true, Some((t, key))) = (i >= 0, ti) {
                if msg == WM_MBUTTONDOWN {
                    toggle_shown(&key);
                    InvalidateRect(hwnd, null(), 0);
                } else if msg == WM_RBUTTONDOWN {
                    // the flyout stays behind the app's menu (as in Windows 11) and closes once the menu is gone
                    let mut pid = 0u32;
                    GetWindowThreadProcessId(t.0, &mut pid);
                    TF_MENU.set((now_ms(), pid, false));
                    SetTimer(hwnd, 2, 100, None);
                    tray_click(t, false);
                } else {
                    ShowWindow(hwnd, SW_HIDE);
                    InvalidateRect(BAR.load(Relaxed) as HWND, null(), 0); // the bar's chevron turns with it
                    tray_click(t, msg == WM_LBUTTONDOWN);
                }
            }
            0
        }
        WM_ACTIVATE if wp & 0xffff == 0 => {
            // right after a right click the app takes the foreground for its menu: timer 2 decides when to close
            if now_ms().saturating_sub(TF_MENU.get().0) > TF_MENU_WAIT {
                ShowWindow(hwnd, SW_HIDE);
                InvalidateRect(BAR.load(Relaxed) as HWND, null(), 0); // the bar's chevron turns with it
            }
            0
        }
        WM_TIMER if wp == 2 => {
            let (at, pid, seen) = TF_MENU.get();
            if app_menu_up(pid) {
                TF_MENU.set((at, pid, true));
            } else if seen || now_ms().saturating_sub(at) > TF_MENU_WAIT {
                // the menu closed (an item picked, Esc, a click elsewhere) or never came: close unless the user is back on us
                KillTimer(hwnd, 2);
                TF_MENU.set((0, 0, false));
                if GetForegroundWindow() != hwnd {
                    ShowWindow(hwnd, SW_HIDE);
                    InvalidateRect(BAR.load(Relaxed) as HWND, null(), 0);
                }
            }
            0
        }
        WM_TIMER => {
            let (_, x, y) = TF_ANIM.get();
            let e = tf_progress(0);
            SetLayeredWindowAttributes(hwnd, 0, (255.0 * e) as u8, LWA_ALPHA);
            SetWindowPos(hwnd, null_mut(), x, y + tf_slide(e), 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            InvalidateRect(hwnd, null(), 0);
            if tf_progress(over_idx().len() as i32) >= 1.0 {
                KillTimer(hwnd, 1);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------- tray tooltip: a small popup under the icon after the mouse rests on it ----------

unsafe fn tip_font() -> HFONT {
    CreateFontW(-sc(12), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, wide(ui_face()).as_ptr())
}

unsafe extern "system" fn tip_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg != WM_PAINT {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let mut ps: PAINTSTRUCT = zeroed();
    let dc = BeginPaint(hwnd, &mut ps);
    let mut r: RECT = zeroed();
    GetClientRect(hwnd, &mut r);
    fill(dc, r, pal().line, sc(8));
    fill(dc, RECT { left: 1, top: 1, right: r.right - 1, bottom: r.bottom - 1 }, pal().bg, sc(8));
    let f = tip_font();
    let of = SelectObject(dc, f);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, pal().text);
    let mut tr = RECT { left: sc(8), top: sc(5), right: r.right - sc(8), bottom: r.bottom - sc(5) };
    TIP.with_borrow(|s| DrawTextW(dc, wide(s).as_ptr(), -1, &mut tr, DT_LEFT | DT_NOPREFIX | DT_WORDBREAK));
    SelectObject(dc, of);
    DeleteObject(f);
    EndPaint(hwnd, &ps);
    0
}

/// Tooltip text of a bar element ("" = none).
unsafe fn tip_text(h: i32) -> String {
    match h {
        SEARCH => "Search".into(),
        0 => "Start".into(),
        TAPP => "Hidden icons".into(),
        CLOCK => {
            let mut t: SYSTEMTIME = zeroed();
            GetLocalTime(&mut t);
            let mut b = [0u16; 80];
            let n = GetDateFormatW(0x0400, 2, &t, null(), b.as_mut_ptr(), 80).max(1) as usize - 1; // DATE_LONGDATE
            String::from_utf16_lossy(&b[..n])
        }
        LANG => tf!("Язык ввода: {}", "Input language: {}", layout_names(layouts().1).0),
        WIFI_I => match net() {
            Link::Wifi(q) => tf!("Wi‑Fi · сигнал {q}%", "Wi‑Fi · signal {q}%"),
            Link::NoInternet(_) => t!("Wi‑Fi подключён, но интернета нет", "Wi‑Fi connected, no internet").into(),
            Link::Cable => t!("Интернет по кабелю", "Internet over a cable").into(),
            Link::Offline => t!("Нет подключения к сети", "Not connected").into(),
        },
        VOL_I => vol_tip(),
        BAT_I => match BAT.get() {
            (p, ac) if p >= 0 => format!("Battery {p}%{}", if ac { " · plugged in" } else { "" }),
            _ => String::new(),
        },
        t if t > TAPP && t < TAPP + 100 => bar_idx().get((t - TAPP - 1) as usize).and_then(|&i| TAPPS.with_borrow(|t| t.get(i).map(|i| i.tip.clone()))).unwrap_or_default(),
        i if i >= 1 => {
            let (wins, lnk) = BTNS.with_borrow(|b| b.get(i as usize).map(|b| (b.wins.clone(), b.pin.and_then(|p| PINS.with_borrow(|p2| p2.get(p).map(|x| wstr_of(&x.lnk))))))).unwrap_or_default();
            let title = |w: HWND| {
                let mut b = [0u16; 256];
                let n = GetWindowTextW(w, b.as_mut_ptr(), 256).max(0) as usize;
                String::from_utf16_lossy(&b[..n])
            };
            match (wins.first(), lnk) {
                (Some(&w), _) if wins.len() > 1 => format!("{}  (+{})", title(w), wins.len() - 1),
                (Some(&w), _) => title(w),
                (None, Some(l)) => std::path::Path::new(&l).file_stem().unwrap_or_default().to_string_lossy().into(),
                _ => String::new(),
            }
        }
        _ => String::new(),
    }
}

fn vol_tip() -> String {
    match VOL.get() {
        (v, false) if v >= 0 => format!("Volume {v}%"),
        (_, true) => "Muted".into(),
        _ => "Volume".into(),
    }
}

/// Shows `s` in a small rounded popup above the bar, centered on screen x `cx`.
unsafe fn show_tip(s: String, cx: i32) {
    let tw = TIPWND.get();
    if s.is_empty() || tw.is_null() {
        return;
    }
    let (dc, f) = (GetDC(tw), tip_font());
    let of = SelectObject(dc, f);
    let mut r = RECT { left: 0, top: 0, right: sc(280), bottom: 0 };
    DrawTextW(dc, wide(&s).as_ptr(), -1, &mut r, DT_CALCRECT | DT_NOPREFIX | DT_WORDBREAK);
    SelectObject(dc, of);
    DeleteObject(f);
    ReleaseDC(tw, dc);
    let (w, h) = (r.right + sc(16), r.bottom + sc(10));
    TIP.set(s);
    SetWindowRgn(tw, CreateRoundRectRgn(0, 0, w + 1, h + 1, rr(8), rr(8)), 1);
    let x = (cx - w / 2).clamp(sc(4), GetSystemMetrics(SM_CXSCREEN) - w - sc(4));
    SetWindowPos(tw, HWND_TOPMOST, x, if at_top() { bar_h() - bar_off() + sc(6) } else { bar_top() + bar_off() - h - sc(6) }, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    InvalidateRect(tw, null(), 1);
}

// ---------- Alt+Tab switcher: the open windows' icons, most recently used first, the selected one's title below ----------

const SW_CELL: i32 = 76;
const SW_PAD: i32 = 14;
const SW_TITLE: i32 = 30;

unsafe fn alt_tab(back: bool) {
    let sw = SWWND.get();
    if IsWindowVisible(sw) == 0 {
        let mut v: Vec<HWND> = Vec::new();
        EnumWindows(Some(collect), &mut v as *mut _ as LPARAM); // z-order: the active window first, then the one before it
        let n = v.len();
        if n == 0 {
            return SWITCHING.store(false, Relaxed);
        }
        SW.set((v, if n == 1 { 0 } else if back { n - 1 } else { 1 }));
        let scr = GetSystemMetrics(SM_CXSCREEN);
        let w = (n as i32 * sc(SW_CELL)).min(scr - sc(80)) + 2 * sc(SW_PAD);
        let h = sc(SW_PAD) + sc(SW_CELL) + sc(SW_TITLE) + sc(SW_PAD) / 2;
        SetWindowRgn(sw, CreateRoundRectRgn(0, 0, w + 1, h + 1, rr(18), rr(18)), 1);
        SetWindowPos(sw, HWND_TOPMOST, (scr - w) / 2, (GetSystemMetrics(SM_CYSCREEN) - h) / 2, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    } else {
        SW.with_borrow_mut(|(v, i)| *i = if back { (*i + v.len() - 1) % v.len() } else { (*i + 1) % v.len() });
    }
    InvalidateRect(sw, null(), 0);
}

/// Alt released: switch to the selected window (restored if minimized). Esc: just close.
unsafe fn alt_done(cancel: bool) {
    ShowWindow(SWWND.get(), SW_HIDE);
    let (v, i) = SW.take();
    if let Some(&w) = v.get(i).filter(|_| !cancel) {
        if IsIconic(w) != 0 {
            ShowWindow(w, SW_RESTORE);
        }
        force_fg(w);
    }
}

unsafe extern "system" fn sw_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg != WM_PAINT {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let mut ps: PAINTSTRUCT = zeroed();
    let dc = BeginPaint(hwnd, &mut ps);
    let mut r: RECT = zeroed();
    GetClientRect(hwnd, &mut r);
    let (mem, bmp) = (CreateCompatibleDC(dc), CreateCompatibleBitmap(dc, r.right, r.bottom));
    let ob = SelectObject(mem, bmp);
    fill(mem, r, pal().line, sc(18));
    fill(mem, RECT { left: 1, top: 1, right: r.right - 1, bottom: r.bottom - 1 }, pal().bg, sc(18));
    let pad = sc(SW_PAD);
    SW.with_borrow(|(v, sel)| {
        let cell = (r.right - 2 * pad) / v.len().max(1) as i32;
        let isz = sc(36).min(cell - sc(14)).max(sc(12));
        for (k, &w) in v.iter().enumerate() {
            let x = pad + k as i32 * cell;
            if k == *sel {
                let c = RECT { left: x + sc(3), top: pad, right: x + cell - sc(3), bottom: pad + sc(SW_CELL) - sc(6) };
                fill(mem, c, pal().accent, sc(12)); // accent ring around the selected window
                fill(mem, RECT { left: c.left + sc(2), top: c.top + sc(2), right: c.right - sc(2), bottom: c.bottom - sc(2) }, pal().active, sc(10));
            }
            let ic = icon_of(w);
            if !ic.is_null() {
                DrawIconEx(mem, x + (cell - isz) / 2, pad + (sc(SW_CELL) - sc(6) - isz) / 2, ic, isz, isz, 0, null_mut(), DI_NORMAL);
            }
        }
        if let Some(&w) = v.get(*sel) {
            let mut b = [0u16; 256];
            let n = GetWindowTextW(w, b.as_mut_ptr(), 256).max(0) as usize;
            let f = CreateFontW(-sc(14), 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, wide(ui_face()).as_ptr());
            let of = SelectObject(mem, f);
            SetBkMode(mem, TRANSPARENT as i32);
            text(mem, &String::from_utf16_lossy(&b[..n]), RECT { left: pad, top: pad + sc(SW_CELL) - sc(4), right: r.right - pad, bottom: pad + sc(SW_CELL) + sc(SW_TITLE) - sc(4) }, pal().text, DT_CENTER);
            SelectObject(mem, of);
            DeleteObject(f);
        }
    });
    BitBlt(dc, 0, 0, r.right, r.bottom, mem, 0, 0, SRCCOPY);
    SelectObject(mem, ob);
    DeleteObject(bmp);
    DeleteDC(mem);
    EndPaint(hwnd, &ps);
    0
}

unsafe fn hide_tip(bar: HWND) {
    KillTimer(bar, 2);
    ShowWindow(TIPWND.get(), SW_HIDE);
}

unsafe fn set_state(hwnd: HWND, state: usize) {
    let mut ab: APPBARDATA = zeroed();
    ab.cbSize = size_of::<APPBARDATA>() as u32;
    ab.hWnd = hwnd;
    ab.lParam = state as isize;
    SHAppBarMessage(ABM_SETSTATE, &mut ab);
}

/// Hides the real taskbar and sets it to auto-hide so Windows gives its work-area strip back.
unsafe fn hide_taskbar() {
    set_state(tray()[0], ABS_AUTOHIDE as usize);
    for t in tray().into_iter().filter(|t| !t.is_null()) {
        ShowWindow(t, SW_HIDE);
    }
}

fn taskbar_state_file() -> PathBuf {
    appdata_file("topbar-taskbar-state.txt")
}

/// The taskbar mode (always on top / auto-hide bits) the user had before the bar hid it. It lives in a file so that
/// `--restore` (reshell's watchdog after a crash) puts back the user's own mode, not the Windows default.
fn saved_taskbar_state() -> Option<usize> {
    std::fs::read_to_string(taskbar_state_file()).ok()?.trim().parse().ok()
}

fn forget_taskbar_state() {
    let _ = std::fs::remove_file(taskbar_state_file());
}

/// At start: the mode to give back later. A file left by a crashed run wins (explorer's current mode is then our own
/// auto-hide); otherwise the current mode is the user's and is written down. Without explorer's taskbar: the default.
unsafe fn remember_taskbar_state() -> usize {
    if let Some(s) = saved_taskbar_state() {
        return s;
    }
    let tb = tray()[0];
    if tb.is_null() {
        return ABS_ALWAYSONTOP as usize;
    }
    let mut ab: APPBARDATA = zeroed();
    ab.cbSize = size_of::<APPBARDATA>() as u32;
    ab.hWnd = tb;
    let s = SHAppBarMessage(ABM_GETSTATE, &mut ab);
    let _ = std::fs::write(taskbar_state_file(), s.to_string());
    s
}

unsafe fn show_taskbar(state: usize) {
    set_state(tray()[0], state);
    for t in tray().into_iter().filter(|t| !t.is_null()) {
        ShowWindow(t, SW_SHOWNA);
    }
}

// explorer re-shows its taskbar on hover / restart; hide it again
unsafe fn hide_taskbar_if_shown() {
    if tray().into_iter().any(|t| !t.is_null() && IsWindowVisible(t) != 0) {
        hide_taskbar();
    }
}

/// Reserves the bottom strip of the primary monitor (maximized windows stay above it).
unsafe fn place(hwnd: HWND) {
    let (cx, cy) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
    let mut ab: APPBARDATA = zeroed();
    ab.cbSize = size_of::<APPBARDATA>() as u32;
    ab.hWnd = hwnd;
    // the reserved strip: the bar itself plus the dock's gap under it
    // the dock reserves nothing: maximized windows take the whole screen and the dock slides away over them
    let strip = if style() == Style::Dock || i18n::style::autohide() { 0 } else { bar_h() };
    if at_top() {
        ab.uEdge = ABE_TOP;
        ab.rc = RECT { left: 0, top: 0, right: cx, bottom: strip };
        SHAppBarMessage(ABM_QUERYPOS, &mut ab);
        ab.rc.bottom = ab.rc.top + strip;
    } else {
        ab.uEdge = ABE_BOTTOM;
        ab.rc = RECT { left: 0, top: cy - strip, right: cx, bottom: cy };
        SHAppBarMessage(ABM_QUERYPOS, &mut ab);
        ab.rc.top = ab.rc.bottom - strip;
    }
    SHAppBarMessage(ABM_SETPOS, &mut ab);
    // No explorer (rs-shell / rust-explorer): nobody handles appbars, so set the work area ourselves. Without it maximized
    // windows slide under the bar and, covering the whole monitor, count as "fullscreen" and hide the bar.
    if find_other("Shell_TrayWnd").is_null() {
        set_work_area(if at_top() { RECT { left: 0, top: ab.rc.bottom, right: cx, bottom: cy } } else { RECT { left: 0, top: 0, right: cx, bottom: ab.rc.top } });
    }
    let (x, w) = bar_x_w();
    // the dock is a pill; the other styles a plain strip
    SetWindowRgn(hwnd, if style() == Style::Dock { CreateRoundRectRgn(0, 0, w + 1, bar_h() + 1, bar_h(), bar_h()) } else { null_mut() }, 1);
    SetWindowPos(hwnd, HWND_TOPMOST, x, bar_y(), w, bar_h(), SWP_NOACTIVATE);
    InvalidateRect(hwnd, null(), 0);
}

unsafe fn set_work_area(mut want: RECT) {
    let mut cur: RECT = zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut cur as *mut _ as *mut _, 0);
    if (cur.left, cur.top, cur.right, cur.bottom) != (want.left, want.top, want.right, want.bottom) {
        SystemParametersInfoW(SPI_SETWORKAREA, 0, &mut want as *mut _ as *mut _, SPIF_SENDCHANGE);
    }
}

// ---------- data: windows, pins, tray apps, volume, wifi, battery ----------

thread_local! {
    static EXE_CACHE: RefCell<HashMap<usize, (u32, String)>> = RefCell::new(HashMap::new()); // hwnd -> (pid, exe): refresh runs twice a second
}

unsafe fn exe_of(h: HWND) -> String {
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, &mut pid);
    if let Some(e) = EXE_CACHE.with_borrow(|c| c.get(&(h as usize)).filter(|(p, e)| *p == pid && !e.is_empty()).map(|(_, e)| e.clone())) {
        return e;
    }
    let p = OpenProcess(0x1000, 0, pid); // PROCESS_QUERY_LIMITED_INFORMATION
    if p.is_null() {
        return String::new();
    }
    let (mut b, mut n) = ([0u16; 520], 520u32);
    let ok = QueryFullProcessImageNameW(p, 0, b.as_mut_ptr(), &mut n);
    CloseHandle(p);
    let exe = if ok == 0 { String::new() } else { String::from_utf16_lossy(&b[..n as usize]).to_lowercase() };
    EXE_CACHE.with_borrow_mut(|c| {
        if c.len() > 512 {
            c.clear(); // closed windows never leave on their own
        }
        c.insert(h as usize, (pid, exe.clone()));
    });
    exe
}

unsafe extern "system" fn collect(h: HWND, lp: LPARAM) -> i32 {
    let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
    let mut cloaked = 0u32; // UWP windows that are suspended / on another desktop are visible but cloaked
    DwmGetWindowAttribute(h, 14, &mut cloaked as *mut _ as *mut _, 4);
    let mut cls = [0u16; 32];
    let n = GetClassNameW(h, cls.as_mut_ptr(), 32) as usize;
    let ours = String::from_utf16_lossy(&cls[..n]) == "topbar";
    if IsWindowVisible(h) != 0 && GetWindow(h, GW_OWNER).is_null() && cloaked == 0 && !ours && GetWindowTextLengthW(h) > 0 && (ex & WS_EX_TOOLWINDOW == 0 || ex & WS_EX_APPWINDOW != 0) {
        (*(lp as *mut Vec<HWND>)).push(h);
    }
    1
}

/// "%windir%\explorer.exe" -> "C:\Windows\explorer.exe" (unknown variables stay as they are).
fn expand_env(s: &str) -> String {
    let parts: Vec<&str> = s.split('%').collect();
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        match std::env::var(p) {
            // odd pieces sit between two '%': a variable name
            Ok(v) if i % 2 == 1 && i + 1 < parts.len() => out.push_str(&v),
            _ if i % 2 == 1 && i + 1 < parts.len() => out.push_str(&format!("%{p}%")),
            _ if i % 2 == 1 => out.push_str(&format!("%{p}")), // a lone '%' with nothing to close it
            _ => out.push_str(p),
        }
    }
    out
}

/// Target exe of a shortcut (lower-case), "" if it has none (e.g. Store apps).
unsafe fn lnk_target(lnk: &[u16]) -> String {
    (|| -> Option<String> {
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        sl.cast::<IPersistFile>().ok()?.Load(PCWSTR(lnk.as_ptr()), STGM(0)).ok()?;
        let mut b = [0u16; 520];
        sl.GetPath(&mut b, null_mut(), 0).ok()?;
        let path = String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0)?]);
        if !path.is_empty() {
            return Some(path.to_lowercase());
        }
        // shell links without a file target (the File Explorer pin) usually take their icon from the app's exe:
        // "%windir%\explorer.exe,0" -> match the app's windows by that exe
        let mut idx = 0;
        sl.GetIconLocation(&mut b, &mut idx).ok()?;
        let icon = String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0)?]);
        let icon = expand_env(&icon).to_lowercase();
        icon.ends_with(".exe").then_some(icon)
    })()
    .unwrap_or_default()
}

/// The target's own icon: SHGetFileInfo on the .lnk adds the shortcut arrow, so that is only the fallback.
unsafe fn lnk_icon(lnk: &[u16], target: &str) -> HICON {
    let ic = item_icon(&wstr_of(lnk), icon_px());
    if !ic.is_null() {
        return ic;
    }
    let mut icon: HICON = null_mut();
    if !target.is_empty() {
        ExtractIconExW(wide(target).as_ptr(), 0, null_mut(), &mut icon, 1);
    }
    if icon.is_null() {
        let mut sfi: SHFILEINFOW = zeroed();
        SHGetFileInfoW(lnk.as_ptr(), 0, &mut sfi, size_of::<SHFILEINFOW>() as u32, SHGFI_ICON | SHGFI_SMALLICON);
        icon = sfi.hIcon;
    }
    icon
}

/// Size app icons are drawn at (24 logical px, like the Windows 11 taskbar). Icons are loaded at exactly this size:
/// a 32 px icon stretched by DrawIconEx comes out jagged.
fn icon_px() -> i32 {
    sc(24) // as on the Windows 11 taskbar; app icons are loaded at this size (sharp)
}

/// Icon of a file, shortcut (its target's icon, no arrow) or shell item ("shell:AppsFolder\<app id>") rendered by the
/// shell at exactly `px` pixels, the way Explorer gets its icons. Null if the shell has none.
unsafe fn item_icon(path: &str, px: i32) -> HICON {
    use windows::Win32::{
        Foundation::SIZE,
        UI::Shell::{IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY},
    };
    let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide(path).as_ptr()), None) else { return null_mut() };
    let Ok(f) = item.cast::<IShellItemImageFactory>() else { return null_mut() };
    let Ok(bmp) = f.GetImage(SIZE { cx: px, cy: px }, SIIGBF_ICONONLY) else { return null_mut() };
    let color = bmp.0 as HBITMAP;
    let mask = CreateBitmap(px, px, 1, 1, null()); // ignored: the 32-bit colour bitmap carries the alpha
    let mut ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let ic = CreateIconIndirect(&mut ii);
    DeleteObject(mask);
    DeleteObject(color);
    ic
}

thread_local! {
    static EXE_ICONS: RefCell<HashMap<String, HICON>> = RefCell::new(HashMap::new()); // crisp icons of running apps, by exe
}

/// Icon of a running window for its bar button: its exe's icon at the exact size (cached), else what the window reports.
/// Apps run by a generic host (Store apps in ApplicationFrameHost, Java games in javaw, scripts in python...) would all
/// get the host's icon: those keep the window's own icon.
unsafe fn win_icon(w: HWND) -> HICON {
    let exe = exe_of(w);
    const HOSTS: [&str; 8] = ["applicationframehost.exe", "javaw.exe", "java.exe", "pythonw.exe", "python.exe", "mmc.exe", "rundll32.exe", "dllhost.exe"];
    if exe.is_empty() || HOSTS.iter().any(|h| exe.rsplit('\\').next() == Some(*h)) {
        return icon_of(w);
    }
    let ic = EXE_ICONS.with_borrow_mut(|m| *m.entry(exe.clone()).or_insert_with(|| item_icon(&exe, icon_px())));
    if ic.is_null() { icon_of(w) } else { ic }
}

/// "c:\program files\windowsapps\claude_5.1.0.0_x64__pzs8\app\claude.exe" -> ("claude_pzs8", "app\claude.exe"):
/// the package family (name + publisher id, the same across versions) and the path inside the package folder.
fn msix_family(target: &str) -> Option<(String, String)> {
    let l = target.to_lowercase();
    let (_, tail) = l.split_once(r"\windowsapps\")?;
    let (folder, rest) = tail.split_once('\\')?;
    let parts: Vec<&str> = folder.split('_').collect(); // name_version_arch_resourceid_publisherid
    (parts.len() == 5).then(|| (format!("{}_{}", parts[0], parts[4]), rest.to_string()))
}

/// A pin of a Store (MSIX) app points into its versioned package folder, which is gone after every app update
/// ("Windows cannot find ...\Claude_5.11492.5.0_x64__...\claude.exe"). Re-points the .lnk to the installed version.
/// Returns the new target (lower-case), None if the target exists or is not in a package.
unsafe fn fix_msix_pin(lnk: &[u16], target: &str) -> Option<String> {
    use windows_sys::Win32::Storage::Packaging::Appx::{GetPackagePathByFullName, GetPackagesByPackageFamily};
    if target.is_empty() || std::path::Path::new(target).exists() {
        return None;
    }
    let (family, rest) = msix_family(target)?;
    let fam = wide(&family);
    let (mut count, mut len) = (0u32, 0u32);
    GetPackagesByPackageFamily(fam.as_ptr(), &mut count, null_mut(), &mut len, null_mut()); // sizes only
    if count == 0 {
        return None; // the app was uninstalled
    }
    let (mut names, mut buf) = (vec![null_mut::<u16>(); count as usize], vec![0u16; len as usize]);
    if GetPackagesByPackageFamily(fam.as_ptr(), &mut count, names.as_mut_ptr(), &mut len, buf.as_mut_ptr()) != 0 {
        return None;
    }
    let mut path = [0u16; 520];
    let mut plen = path.len() as u32;
    if GetPackagePathByFullName(names[0], &mut plen, path.as_mut_ptr()) != 0 {
        return None;
    }
    let new = std::path::Path::new(&wstr_of(&path)).join(&rest);
    if !new.exists() {
        return None;
    }
    let new = new.to_string_lossy().to_lowercase();
    let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
    let pf = sl.cast::<IPersistFile>().ok()?;
    pf.Load(PCWSTR(lnk.as_ptr()), STGM(0)).ok()?;
    sl.SetPath(PCWSTR(wide(&new).as_ptr())).ok()?;
    pf.Save(PCWSTR(lnk.as_ptr()), true).ok()?;
    Some(new)
}

/// Pinned apps (the .lnk files Windows keeps for the taskbar) with their icons and target exe.
unsafe fn load_pins() {
    let Some(appdata) = std::env::var_os("APPDATA") else { return };
    let dir = PathBuf::from(appdata).join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar");
    let mut files: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk"))).collect();
    files.sort();
    files.sort_by_key(|f| std::cmp::Reverse(lnk_uses(f))); // most used first (stable: ties stay alphabetical)
    for f in files {
        let lnk = wide(&f.to_string_lossy());
        let target = lnk_target(&lnk);
        let target = fix_msix_pin(&lnk, &target).unwrap_or(target);
        let icon = lnk_icon(&lnk, &target);
        PINS.with_borrow_mut(|p| p.push(Pin { lnk, target, icon }));
    }
}

fn refresh_buttons() {
    let mut wins: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(collect), &mut wins as *mut _ as LPARAM) };
    let mut btns = vec![Btn { pin: None, wins: vec![], icon: null_mut() }];
    PINS.with_borrow(|pins| btns.extend((0..pins.len()).map(|i| Btn { pin: Some(i), wins: vec![], icon: null_mut() })));
    let mut loose: Vec<HWND> = Vec::new();
    for w in wins {
        let exe = unsafe { exe_of(w) };
        let pin = PINS.with_borrow(|p| p.iter().position(|p| !p.target.is_empty() && p.target == exe));
        match pin {
            Some(i) => btns[1 + i].wins.push(w),
            None => loose.push(w),
        }
    }
    // EnumWindows lists by z-order, so clicking a window would jump its button to the front: keep the order they first appeared in
    ORDER.with_borrow_mut(|o| {
        o.retain(|w| loose.contains(w));
        o.extend(loose.iter().filter(|w| !o.contains(w)).copied().collect::<Vec<_>>());
        btns.extend(o.iter().map(|&w| Btn { pin: None, wins: vec![w], icon: unsafe { win_icon(w) } }));
    });
    BTNS.set(btns);
}

unsafe fn endpoint() -> Option<IAudioEndpointVolume> {
    EP.with_borrow_mut(|e| {
        if e.is_none() {
            *e = (|| -> windows::core::Result<IAudioEndpointVolume> {
                let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                en.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None)
            })()
            .ok();
        }
        e.clone()
    })
}

unsafe fn read_volume() {
    let v = endpoint().and_then(|e| Some(((e.GetMasterVolumeLevelScalar().ok()? * 100.0).round() as i32, e.GetMute().ok()?.as_bool())));
    if v.is_none() {
        EP.set(None); // device changed: look the endpoint up again next time
    }
    VOL.set(v.unwrap_or((-1, false)));
}

unsafe fn set_volume(v: i32) {
    if let Some(e) = endpoint() {
        let _ = e.SetMasterVolumeLevelScalar(v.clamp(0, 100) as f32 / 100.0, null());
        let _ = e.SetMute(false, null());
        read_volume();
    }
}

unsafe fn add_volume(delta: i32) {
    if let Some(e) = endpoint() {
        let v = (VOL.get().0.max(0) + delta).clamp(0, 100);
        let _ = e.SetMasterVolumeLevelScalar(v as f32 / 100.0, null());
        let _ = e.SetMute(false, null());
        read_volume();
    }
}

unsafe fn toggle_mute() {
    if let Some(e) = endpoint() {
        let _ = e.SetMute(!VOL.get().1, null());
        read_volume();
    }
}

thread_local! {
    static WLAN: Cell<usize> = const { Cell::new(0) }; // WLAN client handle, opened once (opening it talks to wlansvc every time)
}

unsafe fn read_wifi() {
    let mut h: HANDLE = WLAN.get() as HANDLE;
    let mut q = -1;
    if h.is_null() {
        let mut ver = 0u32;
        if WlanOpenHandle(2, null(), &mut ver, &mut h) != 0 {
            h = null_mut();
        }
        WLAN.set(h as usize);
    }
    if !h.is_null() {
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
        if WlanEnumInterfaces(h, null(), &mut list) != 0 {
            WlanCloseHandle(h, null()); // stale handle (service restarted): reopen next time
            WLAN.set(0);
        } else {
            let l = &*list;
            for i in 0..l.dwNumberOfItems as usize {
                let ifc = &*l.InterfaceInfo.as_ptr().add(i);
                let (mut size, mut data) = (0u32, null_mut());
                if ifc.isState == wlan_interface_state_connected && WlanQueryInterface(h, &ifc.InterfaceGuid, wlan_intf_opcode_current_connection, null(), &mut size, &mut data, null_mut()) == 0 {
                    q = (*(data as *const WLAN_CONNECTION_ATTRIBUTES)).wlanAssociationAttributes.wlanSignalQuality as i32;
                    WlanFreeMemory(data);
                }
            }
            WlanFreeMemory(list as *const _);
        }
    }
    WIFI.set(q);
    // Windows' own verdict (the same one its tray icon shows): a connection without internet is not "online"
    use windows_sys::Win32::{NetworkManagement::IpHelper::GetNetworkConnectivityHint, Networking::WinSock::{NL_NETWORK_CONNECTIVITY_HINT, NetworkConnectivityLevelHintConstrainedInternetAccess, NetworkConnectivityLevelHintInternetAccess}};
    let mut hint: NL_NETWORK_CONNECTIVITY_HINT = zeroed();
    // a failed call (old Windows) counts as online: never show "no internet" on a guess
    INET.set(GetNetworkConnectivityHint(&mut hint) != 0 || {
        let lvl = hint.ConnectivityLevel;
        lvl == NetworkConnectivityLevelHintInternetAccess || lvl == NetworkConnectivityLevelHintConstrainedInternetAccess
    });
}

/// What the network icon shows.
#[derive(Clone, Copy, PartialEq)]
enum Link {
    Wifi(i32),      // on wifi with internet, signal 0..100
    NoInternet(i32), // on wifi, but no way out
    Cable,          // internet over ethernet
    Offline,        // no connection at all
}

fn net() -> Link {
    match (WIFI.get(), INET.get()) {
        (q, true) if q >= 0 => Link::Wifi(q),
        (q, false) if q >= 0 => Link::NoInternet(q),
        (_, true) => Link::Cable,
        _ => Link::Offline,
    }
}

/// Wi-Fi arcs lit for a signal of `q` percent: 1..3.
fn arcs_lit(q: i32) -> i32 {
    if q >= 67 { 3 } else if q >= 34 { 2 } else { 1 }
}

/// The Wi-Fi symbol drawn with anti-aliased arcs: `lit` of the 3 arcs in `on`, the rest in `off`, centered on (cx, cy),
/// `s` px tall. `mark`: 0 none, 1 a cross (not connected), 2 an exclamation mark (connected, no internet).
unsafe fn wifi_glyph(dc: HDC, cx: i32, cy: i32, s: i32, lit: i32, on: u32, off: u32, mark: u8) {
    let mut g: *mut GpGraphics = null_mut();
    if GdipCreateFromHDC(dc, &mut g) != 0 || g.is_null() {
        return;
    }
    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    let argb = |c: u32| 0xFF00_0000 | (c & 255) << 16 | (c & 0xFF00) | (c >> 16 & 255);
    let pen = |c: u32, w: f32| {
        let mut p: *mut GpPen = null_mut();
        GdipCreatePen1(argb(c), w, UnitPixel, &mut p);
        GdipSetPenStartCap(p, LineCapRound);
        GdipSetPenEndCap(p, LineCapRound);
        p
    };
    let (sf, w) = (s as f32, (s as f32 * 0.12).max(1.6));
    let (bx, by) = (cx as f32, cy as f32 + sf * 0.36); // the point the arcs radiate from
    for i in 0..3 {
        let r = sf * (0.30 + 0.25 * i as f32);
        let p = pen(if i < lit { on } else { off }, w);
        GdipDrawArc(g, p, bx - r, by - r, 2.0 * r, 2.0 * r, 225.0, 90.0);
        GdipDeletePen(p);
    }
    let mut br: *mut GpSolidFill = null_mut();
    GdipCreateSolidFill(argb(if lit > 0 { on } else { off }), &mut br);
    GdipFillEllipse(g, br as *mut GpBrush, bx - w * 0.9, by - w * 0.9, w * 1.8, w * 1.8);
    GdipDeleteBrush(br as *mut GpBrush);
    // the mark sits at the lower right, on a cut-out of the background so it stays readable over the arcs
    let (mx, my, mr) = (bx + sf * 0.42, by - sf * 0.08, sf * 0.26);
    if mark > 0 {
        let mut bg: *mut GpSolidFill = null_mut();
        GdipCreateSolidFill(argb(pal().bg), &mut bg);
        GdipFillEllipse(g, bg as *mut GpBrush, mx - mr, my - mr, 2.0 * mr, 2.0 * mr);
        GdipDeleteBrush(bg as *mut GpBrush);
        let c = if mark == 1 { on } else { WARN };
        let p = pen(c, (w * 0.8).max(1.4));
        let d = mr * 0.5;
        if mark == 1 {
            GdipDrawLine(g, p, mx - d, my - d, mx + d, my + d);
            GdipDrawLine(g, p, mx + d, my - d, mx - d, my + d);
        } else {
            GdipDrawLine(g, p, mx, my - d * 1.2, mx, my + d * 0.2);
            GdipDrawLine(g, p, mx, my + d * 0.95, mx, my + d * 1.0);
        }
        GdipDeletePen(p);
    }
    GdipDeleteGraphics(g);
}

/// Hover key of the tray chevron's turn.
const TF_KEY: i32 = -13;

/// A chevron of half-width `s` around (cx, cy), drawn with round line ends; `deg` 0 points up, 180 down.
unsafe fn chevron(dc: HDC, cx: i32, cy: i32, s: i32, deg: f32, c: u32) {
    let mut g: *mut GpGraphics = null_mut();
    if GdipCreateFromHDC(dc, &mut g) != 0 || g.is_null() {
        return;
    }
    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    let mut p: *mut GpPen = null_mut();
    GdipCreatePen1(0xFF00_0000 | (c & 255) << 16 | (c & 0xFF00) | (c >> 16 & 255), (s as f32 * 0.32).max(1.4), UnitPixel, &mut p);
    GdipSetPenStartCap(p, LineCapRound);
    GdipSetPenEndCap(p, LineCapRound);
    let (sn, cs) = deg.to_radians().sin_cos();
    let pt = |x: f32, y: f32| (cx as f32 + x * cs - y * sn, cy as f32 + x * sn + y * cs);
    let (sf, h) = (s as f32, s as f32 * 0.5);
    let (a, b, d) = (pt(-sf, h), pt(0.0, -h), pt(sf, h));
    GdipDrawLine(g, p, a.0, a.1, b.0, b.1);
    GdipDrawLine(g, p, b.0, b.1, d.0, d.1);
    GdipDeletePen(p);
    GdipDeleteGraphics(g);
}

/// The network icon of the bar and the panels, for the current state.
unsafe fn net_icon(dc: HDC, cx: i32, cy: i32, s: i32, f_glyph: HFONT) {
    let (on, off) = (pal().text, mix(pal().dim, pal().bg, 0.45));
    match net() {
        Link::Wifi(q) => wifi_glyph(dc, cx, cy, s, arcs_lit(q), on, off, 0),
        Link::NoInternet(q) => wifi_glyph(dc, cx, cy, s, arcs_lit(q), mix(on, pal().bg, 0.3), off, 2),
        Link::Offline => wifi_glyph(dc, cx, cy, s, 0, on, mix(pal().dim, pal().bg, 0.2), 1),
        Link::Cable => {
            SelectObject(dc, f_glyph);
            text(dc, "\u{E839}", RECT { left: cx - s, top: cy - s, right: cx + s, bottom: cy + s }, on, DT_CENTER);
        }
    }
}

/// A minimized window that Windows left as a caption stub on the desktop (it sits at the bottom left with its restore /
/// maximize / close buttons) goes off screen, as explorer's taskbar would have done. Restoring it is unaffected.
unsafe fn hide_stub(w: HWND) {
    let mut r: RECT = zeroed();
    if IsIconic(w) != 0 && GetWindowRect(w, &mut r) != 0 && r.left > -30000 {
        SetWindowPos(w, null_mut(), -32000, -32000, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

fn refresh() {
    refresh_buttons();
    BTNS.with_borrow(|b| b.iter().flat_map(|b| &b.wins).for_each(|&w| unsafe { hide_stub(w) }));
    let n = TICK.get();
    TICK.set(n + 1);
    unsafe {
        let mut p: SYSTEM_POWER_STATUS = zeroed();
        let ok = GetSystemPowerStatus(&mut p) != 0 && p.BatteryFlag & 128 == 0 && p.BatteryLifePercent <= 100;
        BAT.set(if ok { (p.BatteryLifePercent as i32, p.ACLineStatus == 1) } else { (-1, false) });
        read_volume();
        let up = !find_other("Shell_TrayWnd").is_null();
        if EXPLORER_UP.swap(up, Relaxed) && !up {
            place(BAR.load(Relaxed) as HWND); // Explorer (started for Settings) is gone and took the reserved strip with it
        }
        if TAPPS.with_borrow(|t| t.is_empty()) && n % 8 == 4 {
            announce(); // no icons yet (apps started before us, or explorer restarted): ask them to register again
        }
        if n % 6 == 0 {
            read_wifi();
        }
    }
    unsafe {
        // icons of apps that died without NIM_DELETE
        TAPPS.with_borrow_mut(|t| {
            t.retain(|x| {
                let alive = IsWindow(x.hwnd) != 0;
                if !alive {
                    DestroyIcon(x.icon);
                }
                alive
            })
        });
        // explorer re-raises its own Shell_TrayWnd now and then; ours must stay in front so apps keep finding it first
        SetWindowPos(HOST.get(), HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

// ---------- layout / hit-testing ----------

struct Layout {
    x0: i32, // first slot: the search button; Start and the rest follow
    bw: i32,
    gap: i32,
    apps: (i32, i32),
    sys: (i32, i32),
    clock: (i32, i32),
}

/// The right-hand group, right to left with the same gap between blocks: clock (as wide as its text), the wifi /
/// volume (/ battery) pill, the tray (chevron + icons). No language button (Win+Space switches the layout) and no gear
/// (the Style menu is "Bar style…" in the empty-spot right-click menu).
fn layout(w: i32) -> Layout {
    let gap = sc(4);
    let cw = match CLOCK_W.get() {
        0 => sc(90),
        v => v,
    };
    let clock = (w - sc(6) - cw, w - sc(6));
    // without a battery (a desktop) the pill ends right after the volume icon instead of leaving its slot empty
    let pill = sc(2 * SYS_CELL) + if BAT.get().0 >= 0 { sc(BAT_W) } else { 0 };
    let sys = (clock.0 - gap - pill, clock.0 - gap);
    let apps = (sys.0 - gap - sc(24) * tray_count() as i32, sys.0 - gap);
    let margin = (w - apps.0) + sc(8); // keep the button group centered and clear of both sides
    let n = BTNS.with_borrow(|b| b.len()) as i32 + 1; // + the search slot
    // Windows 11: 44 px slots side by side, no gap (the highlight's own inset separates them); narrower when crowded
    let gap = 0;
    let bw = (((w - 2 * margin + gap) / n.max(1)) - gap).clamp(sc(16), sc(BTN_W));
    Layout { x0: row_x0(w, n, bw, gap, margin, sc(6)), bw, gap, apps, sys, clock }
}

/// Left edge of the button row (search, Start, pins, open windows: `n` slots of `bw` + `gap`) on a bar `w` wide.
/// Like Windows 11 the whole row is centred on the bar and re-centres as windows open and close (it used to centre
/// search + Start + pins only, so every open app pushed the row right of centre). Never so far right that it runs into
/// the tray (`margin` from the right edge), never left of `min_x`.
fn row_x0(w: i32, n: i32, bw: i32, gap: i32, margin: i32, min_x: i32) -> i32 {
    let row = n * (bw + gap) - gap;
    ((w - row) / 2).min(w - margin - row).max(min_x)
}

unsafe fn hit(hwnd: HWND, x: i32) -> i32 {
    let mut r: RECT = zeroed();
    GetClientRect(hwnd, &mut r);
    let l = layout(r.right);
    if x >= l.clock.0 {
        return CLOCK;
    }
    if x >= l.sys.1 {
        return NONE; // the gaps between the blocks
    }
    if x >= l.sys.0 {
        let d = x - l.sys.0;
        return if d < sc(SYS_CELL) { WIFI_I } else if d < sc(2 * SYS_CELL) { VOL_I } else { BAT_I };
    }
    if x >= l.apps.1 {
        return NONE;
    }
    if x >= l.apps.0 {
        return TAPP + (x - l.apps.0) / sc(24);
    }
    let n = BTNS.with_borrow(|b| b.len()) as i32 + 1;
    let (d, i) = (x - l.x0, (x - l.x0) / (l.bw + l.gap));
    if d < 0 || d % (l.bw + l.gap) >= l.bw || i >= n { NONE } else if i == 0 { SEARCH } else { i - 1 }
}

// ---------- animation ----------

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| {
        let (x, y) = (((a >> s) & 255) as f32, ((b >> s) & 255) as f32);
        (x + (y - x) * t) as u32
    };
    ch(0) | ch(8) << 8 | ch(16) << 16
}

fn ease(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

unsafe fn now_ms() -> u64 {
    GetTickCount64()
}

/// Eased 0..1 hover value of one element: glides toward `on` (1) / off (0) instead of jumping. Sets BUSY while it moves,
/// the painter then schedules another frame.
unsafe fn hv(key: i32, on: bool) -> f32 {
    let now = now_ms();
    let target = if on { 1.0 } else { 0.0 };
    let v = HV.with_borrow_mut(|m| {
        let e = m.entry(key).or_insert((0.0, now));
        let dt = now.saturating_sub(e.1).min(50) as f32; // a long pause must not make the first frame jump
        e.1 = now;
        e.0 = if e.0 < target { (e.0 + dt / 130.0).min(target) } else { (e.0 - dt / 180.0).max(target) };
        e.0
    });
    if v != target {
        BUSY.set(true);
    }
    ease(v)
}

/// Rounded highlight that fades in and out; `from` is the colour it sits on. A click also flashes it in the accent colour.
unsafe fn glow(dc: HDC, key: i32, on: bool, r: RECT, from: u32, to: u32, rad: i32) {
    let t = hv(key, on);
    let p = press_t(key);
    if t > 0.01 || p > 0.0 {
        fill(dc, r, mix(mix(from, to, t), pal().accent, 0.5 * p), rad);
    }
}

thread_local! {
    static PRESS: Cell<(i32, u64)> = const { Cell::new((i32::MIN, 0)) }; // last clicked element and when
    static SEEN: RefCell<HashMap<isize, u64>> = RefCell::new(HashMap::new()); // when each taskbar button first showed
}

/// Marks `key` as just clicked / dropped on: its highlight flashes and its icon squeezes for a moment.
unsafe fn press(key: i32) {
    PRESS.set((key, now_ms()));
}

/// 1.0 right after the click, easing to 0 over 320 ms (keeps the painter running meanwhile).
unsafe fn press_t(key: i32) -> f32 {
    let (k, t0) = PRESS.get();
    let t = now_ms().saturating_sub(t0) as f32 / 320.0;
    if k != key || t >= 1.0 {
        return 0.0;
    }
    BUSY.set(true);
    (1.0 - t) * (1.0 - t)
}

/// 0..1 over 260 ms after a taskbar button first appears: new windows pop in instead of snapping into place.
unsafe fn appear(key: isize) -> f32 {
    let now = now_ms();
    let t0 = SEEN.with_borrow_mut(|m| {
        if m.len() > 512 {
            m.clear(); // shortcut: closed windows are never pruned one by one; a clear only replays the pop-in
        }
        *m.entry(key).or_insert(now)
    });
    let t = now.saturating_sub(t0) as f32 / 260.0;
    if t >= 1.0 {
        return 1.0;
    }
    BUSY.set(true);
    ease(t)
}

/// How far the bar sits below its place: it rises in at start and drops out of the way (and back) when a fullscreen
/// window takes the screen.
fn bar_off() -> i32 {
    ((1.0 - ease(BAR_P.get())) * (bar_h() + dock_gap()) as f32) as i32
}

unsafe fn bar_anim(hwnd: HWND) {
    BAR_T.set(now_ms());
    SetTimer(hwnd, 5, 15, None);
}

unsafe fn bar_step(hwnd: HWND) {
    // hides for a fullscreen game or video, and with "auto-hide" on while the pointer is away (never otherwise: not over the desktop, not in any style)
    let target = if FULLSCREEN.load(Relaxed) || (i18n::style::autohide() && DESK_HIDE.load(Relaxed)) { 0.0 } else { 1.0 };
    let now = now_ms();
    let dt = now.saturating_sub(BAR_T.replace(now)).min(50) as f32;
    let p = BAR_P.get();
    let p = if p < target { (p + dt / 380.0).min(target) } else { (p - dt / 240.0).max(target) };
    BAR_P.set(p);
    if p > 0.0 && IsWindowVisible(hwnd) == 0 {
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    SetWindowPos(hwnd, HWND_TOPMOST, bar_x_w().0, bar_y(), 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
    if p == target {
        KillTimer(hwnd, 5);
        if p == 0.0 {
            ShowWindow(hwnd, SW_HIDE); // not just moved away: a hidden bar cannot cover a game
        }
    }
}

/// True when the foreground window covers the whole primary monitor (game, F11 video, presentation).
/// A maximized window does not: the bar's reserved strip keeps it below the bar.
unsafe fn fullscreen_now(bar: HWND) -> bool {
    let fg = GetForegroundWindow();
    if fg.is_null() || fg == bar || IsWindowVisible(fg) == 0 || IsIconic(fg) != 0 {
        return false;
    }
    let mut cls = [0u16; 64];
    let n = GetClassNameW(fg, cls.as_mut_ptr(), 64).max(0) as usize;
    if matches!(String::from_utf16_lossy(&cls[..n]).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "RustExplorer") {
        return false; // the desktop itself is full size (rs-shell's desktop is a rust-explorer window; a file manager is never "fullscreen")
    }
    let mut r: RECT = zeroed();
    GetWindowRect(fg, &mut r);
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST), &mut mi);
    let m = mi.rcMonitor;
    (m.left, m.top) == (0, 0) && r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
}

/// True when no app window is on screen (none open, or all minimized: Win+D): the user is looking at the desktop.
unsafe fn desktop_now() -> bool {
    BTNS.with_borrow(|b| b.iter().flat_map(|b| &b.wins).all(|&w| IsIconic(w) != 0))
}

unsafe fn fs_update(hwnd: HWND) {
    let fs = fullscreen_now(hwnd);
    // a fullscreen game keeps the screen: the pointer at the bottom edge does not bring the bar (games park it at the
    // edges all the time); Win opens Start, which brings the bar with it
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    // desktop in front: the bar leaves 2 s after the pointer does and comes back at the bottom edge (an open flyout keeps it)
    let now = now_ms();
    let sh = GetSystemMetrics(SM_CYSCREEN);
    let over = if at_top() { pt.y <= 0 || (pt.y < bar_h() && BAR_P.get() > 0.0) } else { pt.y >= sh - 1 || (pt.y >= sh - bar_h() - dock_gap() && BAR_P.get() > 0.0) };
    if fs || !(i18n::style::autohide() || desktop_now() || dock_covered()) || fly::any_open() || over {
        DESK_T.set(now);
    }
    let desk = !fs && now.saturating_sub(DESK_T.get()) >= 2000;
    if DESK_HIDE.swap(desk, Relaxed) != desk {
        bar_anim(hwnd);
    }
    if FULLSCREEN.swap(fs, Relaxed) != fs {
        if fs {
            fly::close_now();
            hide_tip(hwnd);
        }
        bar_anim(hwnd);
    }
}

/// The dock is in the way of the window in front (a maximized one, or one dragged down under it): it hides like over the
/// desktop and comes back when the pointer touches the bottom edge.
unsafe fn dock_covered() -> bool {
    if style() != Style::Dock {
        return false;
    }
    let fg = GetForegroundWindow();
    if fg.is_null() || IsIconic(fg) != 0 || fg == BAR.load(Relaxed) as HWND || fly::any_open() {
        return false;
    }
    let mut r: RECT = zeroed();
    GetWindowRect(fg, &mut r);
    let (x, w) = bar_x_w();
    let top = GetSystemMetrics(SM_CYSCREEN) - bar_h() - dock_gap();
    r.bottom > top && r.right > x && r.left < x + w
}

/// SetForegroundWindow that also works when the request comes from a keyboard hook (we have not "received input" then):
/// attaching to the foreground thread's input queue lifts the restriction for the moment.
unsafe fn force_fg(hw: HWND) {
    // a mouse click on the bar gives us the right to take the foreground: attaching to another thread's input queue while its
    // button is still down hands the activation back to that window on button-up, and the just-opened flyout closed at once
    if SetForegroundWindow(hw) != 0 && GetForegroundWindow() == hw {
        SetFocus(hw);
        return;
    }
    let me = GetCurrentThreadId();
    let ft = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
    let attach = ft != 0 && ft != me && AttachThreadInput(me, ft, 1) != 0;
    SetForegroundWindow(hw);
    if attach {
        AttachThreadInput(me, ft, 0);
    }
    SetFocus(hw); // detaching drops our thread's keyboard focus; without it the Start search box gets no typing
}

/// Win key on its own opens our Start. Runs on its own thread so a busy bar can never stall the system keyboard.
unsafe extern "system" fn kb_hook(n: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if n >= 0 {
        let k = &*(l as *const KBDLLHOOKSTRUCT);
        let down0 = w == WM_KEYDOWN as usize || w == WM_SYSKEYDOWN as usize;
        // without explorer nobody handles the volume keys, and Surface's power + volume-up arrives as an injected Win+PrtSc
        // (the injected events are skipped below, so these two are taken before that check)
        if !EXPLORER_UP.load(Relaxed) {
            let vol = [(VK_VOLUME_UP, 0), (VK_VOLUME_DOWN, 1), (VK_VOLUME_MUTE, 2)].into_iter().find(|&(vk, _)| k.vkCode == vk as u32);
            if let Some((_, v)) = vol {
                if down0 {
                    PostMessageW(BAR.load(Relaxed) as HWND, VOLKEY, v, 0);
                }
                return 1;
            }
            if k.vkCode == VK_SNAPSHOT as u32 && k.flags & LLKHF_INJECTED != 0 {
                if down0 {
                    PostMessageW(BAR.load(Relaxed) as HWND, SNIP, 1, 0);
                }
                return 1;
            }
        }
        let is_win = k.vkCode == VK_LWIN as u32 || k.vkCode == VK_RWIN as u32;
        if k.flags & LLKHF_INJECTED != 0 && down0 && !is_win {
            // another app's hook took Win+<key> (it ate the key, so we never saw it) and sent its "menu mask" key (0xE8,
            // AutoHotkey style) so the Win release opens no Start: ours must not open either
            WIN_ALONE.store(false, Relaxed);
        }
        if k.flags & LLKHF_INJECTED == 0 {
            // without explorer nobody opens the screen capture: Win+Shift+S is our own region clipper then, and PrtSc (when
            // Windows is set to snip with it; otherwise it copies the screen by itself) does it too. With explorer both stay
            // with Windows or whatever tool the user set up for them (a hook of ours would race theirs).
            let held = |vk: u16| GetAsyncKeyState(vk as i32) < 0;
            let snip_key = !EXPLORER_UP.load(Relaxed) && k.vkCode == VK_SNAPSHOT as u32;
            let win_shift_s =
                !EXPLORER_UP.load(Relaxed) && k.vkCode == b'S' as u32 && down0 && (held(VK_LWIN) || held(VK_RWIN)) && held(VK_SHIFT);
            // Win+Space switches the input language in explorer's switcher; without explorer nobody does: the bar does
            if !EXPLORER_UP.load(Relaxed) && k.vkCode == VK_SPACE as u32 && (held(VK_LWIN) || held(VK_RWIN)) {
                if down0 {
                    WIN_ALONE.store(false, Relaxed);
                    keybd_event(0xE8, 0, 0, 0); // a key between Win down and up: no Start on the release
                    keybd_event(0xE8, 0, KEYEVENTF_KEYUP, 0);
                    PostMessageW(BAR.load(Relaxed) as HWND, NEXT_LANG, 0, 0);
                }
                return 1;
            }
            if snip_key || win_shift_s {
                if down0 {
                    WIN_ALONE.store(false, Relaxed);
                    if win_shift_s {
                        // a key between Win down and up, so Windows does not open its Start menu on the release
                        keybd_event(0xE8, 0, 0, 0);
                        keybd_event(0xE8, 0, KEYEVENTF_KEYUP, 0);
                    }
                    PostMessageW(BAR.load(Relaxed) as HWND, SNIP, snip_key as usize, 0); // wp 1: PrtSc, 0: Win+Shift+S
                }
                return 1;
            }
            let down = w == WM_KEYDOWN as usize || w == WM_SYSKEYDOWN as usize;
            let bar = BAR.load(Relaxed) as HWND;
            // Alt+Tab: our own switcher. Without explorer Windows only flips to the next window blindly, and a fullscreen
            // game often takes the focus straight back.
            if k.vkCode == VK_TAB as u32 && k.flags & LLKHF_ALTDOWN != 0 && GetAsyncKeyState(VK_CONTROL as i32) >= 0 {
                if down {
                    SWITCHING.store(true, Relaxed);
                    PostMessageW(bar, ALT_TAB, (GetAsyncKeyState(VK_SHIFT as i32) < 0) as usize, 0);
                }
                return 1;
            }
            if SWITCHING.load(Relaxed) {
                if k.vkCode == VK_ESCAPE as u32 && down {
                    SWITCHING.store(false, Relaxed);
                    PostMessageW(bar, ALT_UP, 1, 0);
                    return 1;
                }
                if matches!(k.vkCode as u16, VK_LMENU | VK_RMENU | VK_MENU) && !down {
                    SWITCHING.store(false, Relaxed);
                    keys(&[0xE8]); // the target app must not take the lone Alt release as "open the menu bar"
                    PostMessageW(bar, ALT_UP, 0, 0);
                }
            }
            if is_win {
                if down {
                    WIN_ALONE.store(true, Relaxed);
                } else if WIN_ALONE.swap(false, Relaxed) {
                    keys(&[0xE8]); // an unassigned key between press and release: Windows no longer opens its own Start
                    PostMessageW(BAR.load(Relaxed) as HWND, WIN_START, 0, 0);
                }
            } else if down {
                WIN_ALONE.store(false, Relaxed); // Win+something else: a shortcut, not a Start press
            }
        }
    }
    CallNextHookEx(null_mut(), n, w, l)
}

// ---------- drawing ----------

/// Fills `r` (right / bottom exclusive) in colour `c`; `rad` > 0 rounds the corners (rad = corner diameter), anti-aliased
/// through GDI+. Plain GDI for square boxes and when GDI+ is not available.
unsafe fn fill(dc: HDC, r: RECT, c: u32, rad: i32) {
    let rad = if style().square() { 0 } else { rad };
    let (w, h) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let mut g: *mut GpGraphics = null_mut();
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
    let (x, y, d) = (r.left as f32, r.top as f32, (rad as f32).min(w).min(h));
    let mut path: *mut GpPath = null_mut();
    GdipCreatePath(FillModeAlternate, &mut path);
    GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
    GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
    GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
    GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
    GdipClosePathFigure(path);
    let mut br: *mut GpSolidFill = null_mut();
    GdipCreateSolidFill(0xFF00_0000 | (c & 255) << 16 | (c & 0xFF00) | (c >> 16 & 255), &mut br); // COLORREF -> ARGB
    GdipFillPath(g, br as *mut GpBrush, path);
    GdipDeleteBrush(br as *mut GpBrush);
    GdipDeletePath(path);
    GdipDeleteGraphics(g);
}

unsafe fn text(dc: HDC, s: &str, mut r: RECT, c: u32, flags: u32) {
    SetTextColor(dc, c);
    let w = wide(s);
    DrawTextW(dc, w.as_ptr(), -1, &mut r, flags | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
}

unsafe fn icon_of(h: HWND) -> HICON {
    for which in [1usize, 2] {
        // ICON_BIG, ICON_SMALL2
        let mut r = 0usize;
        SendMessageTimeoutW(h, WM_GETICON, which, 0, SMTO_ABORTIFHUNG, 40, &mut r);
        if r != 0 {
            return r as HICON;
        }
    }
    let c = GetClassLongPtrW(h, GCLP_HICON);
    (if c != 0 { c } else { GetClassLongPtrW(h, GCLP_HICONSM) }) as HICON
}

/// The app window the user is typing in: the foreground window, or, while our own bar / panel / menu holds the
/// foreground (after a right click, a closed Start...), the last app window that had it.
unsafe fn app_window() -> HWND {
    let fg = GetForegroundWindow();
    let mut pid = 0u32;
    GetWindowThreadProcessId(fg, &mut pid);
    if !fg.is_null() && pid != std::process::id() {
        APP_FG.set(fg);
        return fg;
    }
    let last = APP_FG.get();
    if IsWindow(last) != 0 { last } else { fg }
}

/// PrtSc without explorer: the whole desktop (all monitors) to the clipboard and to Pictures\Screenshots\<date time>.png.
/// The system's own capture overlay (ms-screenclip:) does not come up without explorer as the shell. Returns the file name.
/// `topbar --shot` does the same from a command line.
unsafe fn screenshot() -> Option<String> {
    let (x, y, w, h) = (GetSystemMetrics(76), GetSystemMetrics(77), GetSystemMetrics(78), GetSystemMetrics(79)); // virtual screen
    if w <= 0 || h <= 0 {
        return None;
    }
    let (scr, mem) = (GetDC(null_mut()), CreateCompatibleDC(GetDC(null_mut())));
    let bmp = CreateCompatibleBitmap(scr, w, h);
    let old = SelectObject(mem, bmp);
    BitBlt(mem, 0, 0, w, h, scr, x, y, SRCCOPY | 0x4000_0000); // CAPTUREBLT: layered windows (the wallpaper, overlays) too
    SelectObject(mem, old);
    DeleteDC(mem);
    ReleaseDC(null_mut(), scr);
    save_bitmap(bmp)
}

/// Saves a screen bitmap as Pictures\Screenshots\Screenshot <date time>.png and puts it on the clipboard (which then owns it).
/// Returns the file name when either worked.
unsafe fn save_bitmap(bmp: HBITMAP) -> Option<String> {
    use windows_sys::Win32::{
        Graphics::GdiPlus::{GdipCreateBitmapFromHBITMAP, GdipDisposeImage, GdipSaveImageToFile, GpBitmap, GpImage},
        System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
    };
    let mut t: SYSTEMTIME = zeroed();
    GetLocalTime(&mut t);
    let name = format!("Screenshot {:04}-{:02}-{:02} {:02}-{:02}-{:02}.png", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
    let dir = PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join(r"Pictures\Screenshots");
    let _ = std::fs::create_dir_all(&dir);
    let mut gb: *mut GpBitmap = null_mut();
    let mut saved = false;
    if GdipCreateBitmapFromHBITMAP(bmp, null_mut(), &mut gb) == 0 && !gb.is_null() {
        let png = windows_sys::core::GUID { data1: 0x557c_f406, data2: 0x1a04, data3: 0x11d3, data4: [0x9a, 0x73, 0x00, 0x00, 0xf8, 0x1e, 0xf3, 0x2e] }; // PNG encoder
        saved = GdipSaveImageToFile(gb as *mut GpImage, wide(&dir.join(&name).to_string_lossy()).as_ptr(), &png, null()) == 0;
        GdipDisposeImage(gb as *mut GpImage);
    }
    let mut clip = false;
    if OpenClipboard(null_mut()) != 0 {
        EmptyClipboard();
        clip = !SetClipboardData(2, bmp as _).is_null(); // CF_BITMAP: the clipboard owns the bitmap from here on
        CloseClipboard();
    }
    if !clip {
        DeleteObject(bmp);
    }
    (saved || clip).then_some(name)
}

/// The clipper saved a region (its file name is left in `clip::DONE`): say so above the bar for a moment.
unsafe fn clip_notice(bar: HWND) {
    let name = clip::DONE.lock().ok().and_then(|mut d| d.take()).unwrap_or_default();
    KillTimer(bar, 2);
    show_tip(tf!("Фрагмент сохранён: {name}\nи скопирован в буфер обмена", "Snip saved: {name}\nand copied to the clipboard"), GetSystemMetrics(SM_CXSCREEN) / 2);
    SetTimer(bar, 6, 2500, None);
}

/// PrtSc pressed: take the screenshot, then say so above the bar for a moment.
unsafe fn screenshot_notice(bar: HWND) {
    let msg = match screenshot() {
        Some(n) => tf!("Скриншот сохранён: {n}\nи скопирован в буфер обмена", "Screenshot saved: {n}\nand copied to the clipboard"),
        None => t!("Не удалось сделать скриншот", "Could not take a screenshot").to_string(),
    };
    KillTimer(bar, 2);
    show_tip(msg, GetSystemMetrics(SM_CXSCREEN) / 2);
    SetTimer(bar, 6, 2500, None); // timer 6 hides the tip
}

/// Next keyboard layout for the app window. Posted to its focused control (that is where Chrome, Office... look for it);
/// timer 7 checks it took and otherwise presses Win+Space, the system switcher (games, terminals ignore the message).
/// The installed keyboard layouts and the one the app in front uses.
unsafe fn layouts() -> (Vec<HKL>, HKL) {
    let mut l: [HKL; 32] = [null_mut(); 32];
    let n = GetKeyboardLayoutList(32, l.as_mut_ptr()).clamp(0, 32) as usize;
    (l[..n].to_vec(), GetKeyboardLayout(GetWindowThreadProcessId(app_window(), null_mut())))
}

/// A layout's language as Windows names it in the user's language ("Русский (Россия)"), and its short code ("RUS").
unsafe fn layout_names(k: HKL) -> (String, String) {
    let id = k as usize as u32 & 0xffff;
    let get = |what: u32| {
        let mut b = [0u16; 96];
        let n = GetLocaleInfoW(id, what, b.as_mut_ptr(), 96).max(1) as usize - 1;
        String::from_utf16_lossy(&b[..n])
    };
    let short = get(3).to_uppercase(); // LOCALE_SABBREVLANGNAME
    (get(2), if short == "ENU" { "ENG".into() } else { short }) // LOCALE_SLOCALIZEDDISPLAYNAME
}

/// The input language picker above the language button.
unsafe fn lang_menu(bar: HWND) {
    let (list, cur) = layouts();
    let mut items: Vec<fly::MItem> = list.iter().map(|&k| {
        let (name, short) = layout_names(k);
        fly::MItem::new(name).on(k == cur).hint(Box::leak(short.into_boxed_str())) // a handful of short strings, kept for the bar's lifetime
    }).collect();
    items.push(fly::MItem::new(t!("Языковые параметры", "Language settings")).sep());
    let head = (null_mut(), "\u{E775}", t!("Язык ввода", "Input language").to_string(), t!("Win+Пробел — следующий", "Win+Space — next one").to_string());
    fly::menu(tip_cx(&bar_layout(bar), LANG), Some(head), items, move |i| match list.get(i) {
        Some(&k) => set_lang(bar, k),
        None => open(&wide("ms-settings:regionlanguage")),
    });
}

unsafe fn next_lang(bar: HWND) {
    let (l, cur) = layouts();
    if l.len() < 2 {
        return; // one layout: nothing to switch to
    }
    set_lang(bar, l[l.iter().position(|&k| k == cur).map_or(0, |i| i + 1) % l.len()]);
}

/// Asks the app in front to switch to layout `next` (the bar's own thread cannot change another app's language).
unsafe fn set_lang(bar: HWND, next: HKL) {
    let w = app_window();
    let tid = GetWindowThreadProcessId(w, null_mut());
    let cur = GetKeyboardLayout(tid);
    let mut gi: GUITHREADINFO = zeroed();
    gi.cbSize = size_of::<GUITHREADINFO>() as u32;
    let target = if GetGUIThreadInfo(tid, &mut gi) != 0 && !gi.hwndFocus.is_null() { gi.hwndFocus } else { w };
    PostMessageW(target, WM_INPUTLANGCHANGEREQUEST, 0, next as isize);
    LANG_TRY.set((tid, cur as usize));
    SetTimer(bar, 7, 250, None);
}

thread_local! {
    static SIG: Cell<(u64, u32)> = const { Cell::new((0, 0)) }; // (hash of the last painted state, ticks since the last repaint)
}

/// Hash of everything the bar paints from polled state.
unsafe fn view_sig() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    BTNS.with_borrow(|b| b.iter().for_each(|b| (b.pin, b.icon as usize, b.wins.iter().map(|&w| (w as usize, IsIconic(w))).collect::<Vec<_>>()).hash(&mut h)));
    (GetForegroundWindow() as usize, BAT.get(), VOL.get(), WIFI.get(), INET.get(), EXPLORER_UP.load(Relaxed), clock_texts()).hash(&mut h);
    TAPPS.with_borrow(|t| t.iter().for_each(|x| (x.hwnd as usize, x.uid, x.icon as usize, x.hidden, &x.tip).hash(&mut h)));
    h.finish()
}

unsafe fn clock_texts() -> (String, String) {
    let mut t: SYSTEMTIME = zeroed();
    GetLocalTime(&mut t);
    let (mut a, mut b) = ([0u16; 40], [0u16; 40]);
    let n = GetTimeFormatW(0x0400, 2, &t, null(), a.as_mut_ptr(), 40).max(1) as usize - 1; // LOCALE_USER_DEFAULT, TIME_NOSECONDS
    let m = GetDateFormatW(0x0400, 1, &t, null(), b.as_mut_ptr(), 40).max(1) as usize - 1; // DATE_SHORTDATE
    (String::from_utf16_lossy(&a[..n]), String::from_utf16_lossy(&b[..m]))
}

unsafe fn paint(hwnd: HWND) {
    let opacity = i18n::style::bar_opacity();
    let layered = sync_layered(hwnd, opacity < 100);
    let mut ps: PAINTSTRUCT = zeroed();
    let wdc = BeginPaint(hwnd, &mut ps);
    let mut cr: RECT = zeroed();
    GetClientRect(hwnd, &mut cr);
    let h = cr.bottom;
    BUSY.set(false);
    if layered {
        paint_layered(hwnd, wdc, cr, opacity);
    } else {
        let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right, h));
        let old = SelectObject(dc, bmp);
        bar_background(dc, cr);
        draw_bar(dc, cr, 5); // CLEARTYPE_QUALITY
        BitBlt(wdc, 0, 0, cr.right, h, dc, 0, 0, SRCCOPY);
        SelectObject(dc, old);
        DeleteObject(bmp);
        DeleteDC(dc);
    }
    if BUSY.get() {
        SetTimer(hwnd, 4, 16, None); // next animation frame
    } else {
        KillTimer(hwnd, 4);
    }
    EndPaint(hwnd, &ps);
}

/// The solid bar's panel: frosted-glass look, a hairline on top, a faint light catch under it, then the panel colour fading in.
unsafe fn bar_background(dc: HDC, cr: RECT) {
    let h = cr.bottom;
    let lit = mix(pal().bg, pal().text, 0.045);
    for y in 0..h {
        fill(dc, RECT { left: 0, top: y, right: cr.right, bottom: y + 1 }, mix(lit, pal().bg, (y as f32 / sc(18) as f32).min(1.0)), 0);
    }
    winrust_common::style::backdrop(dc, cr, sc(100) as f32 / 100.0);
    let edge = if at_top() { h - 1 } else { 0 }; // the hairline faces the windows
    fill(dc, RECT { left: 0, top: edge, right: cr.right, bottom: edge + 1 }, pal().line, 0);
    if !at_top() {
        fill(dc, RECT { left: 0, top: 1, right: cr.right, bottom: 2 }, mix(pal().bg, pal().text, 0.08), 0);
    }
}

/// Turns the bar into a per-pixel-alpha layered window (see-through background) or back into a plain one.
/// Returns whether it is layered now.
unsafe fn sync_layered(hwnd: HWND, want: bool) -> bool {
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    let has = ex & WS_EX_LAYERED as isize != 0;
    if want != has {
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, if want { ex | WS_EX_LAYERED as isize } else { ex & !(WS_EX_LAYERED as isize) });
        if !want {
            InvalidateRect(hwnd, null(), 0); // back to normal painting: the whole bar, not just the dirty part
        }
    }
    want
}

/// A top-down 32-bit DIB of `w` x `h` selected into a new memory DC: (dc, bitmap, old bitmap, pixels as BGRA bytes).
unsafe fn dib(wdc: HDC, w: i32, h: i32) -> (HDC, HBITMAP, HGDIOBJ, *mut u8) {
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h; // top-down
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    let mut bits: *mut core::ffi::c_void = null_mut();
    let bmp = CreateDIBSection(wdc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    let dc = CreateCompatibleDC(wdc);
    let old = SelectObject(dc, bmp);
    (dc, bmp, old, bits as *mut u8)
}

/// The see-through bar. GDI cannot draw alpha, so the bar's content is drawn twice, over black and over white:
/// where the two agree the pixel is solid, where they differ it is see-through by that much (anti-aliased text and
/// icon edges come out right). Under that goes the panel colour at `opacity` percent, and the result goes to the screen
/// through UpdateLayeredWindow.
unsafe fn paint_layered(hwnd: HWND, wdc: HDC, cr: RECT, opacity: u8) {
    let (w, h) = (cr.right, cr.bottom);
    if w <= 0 || h <= 0 {
        return;
    }
    let (bdc, bbmp, bold, bpx) = dib(wdc, w, h);
    let (wtdc, wbmp, wold, wpx) = dib(wdc, w, h);
    if !bpx.is_null() && !wpx.is_null() {
        FillRect(bdc, &cr, GetStockObject(BLACK_BRUSH) as HBRUSH);
        FillRect(wtdc, &cr, GetStockObject(WHITE_BRUSH) as HBRUSH);
        draw_bar(bdc, cr, 4); // ANTIALIASED_QUALITY: ClearType's coloured fringes would not survive the alpha
        draw_bar(wtdc, cr, 4);
        GdiFlush();
        let n = (w * h) as usize * 4;
        let (b, wt) = (std::slice::from_raw_parts_mut(bpx, n), std::slice::from_raw_parts(wpx, n));
        let edge = if at_top() { h - 1 } else { 0 };
        compose(b, wt, w as usize, edge as usize, opacity);
        let (size, src) = (SIZE { cx: w, cy: h }, POINT { x: 0, y: 0 });
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        UpdateLayeredWindow(hwnd, null_mut(), null(), &size, bdc, &src, 0, &blend, ULW_ALPHA);
    }
    for (dc, bmp, old) in [(bdc, bbmp, bold), (wtdc, wbmp, wold)] {
        SelectObject(dc, old);
        DeleteObject(bmp);
        DeleteDC(dc);
    }
}

/// Premultiplied BGRA for the see-through bar, written over `black`: the content (drawn over black and over white)
/// on top of the panel colour at `opacity` percent. Row `edge` gets the hairline colour. The panel never goes fully
/// clear (alpha 1 at least): fully clear pixels of a layered window let clicks through to whatever is behind it.
fn compose(black: &mut [u8], white: &[u8], w: usize, edge: usize, opacity: u8) {
    let ta = (opacity.min(100) as u32 * 255 / 100).max(1);
    let premul = |c: u32| [((c >> 16) & 255) * ta / 255, ((c >> 8) & 255) * ta / 255, (c & 255) * ta / 255]; // COLORREF -> B, G, R
    let (panel, line) = (premul(pal().bg), premul(pal().line));
    for (i, (b, wt)) in black.chunks_exact_mut(4).zip(white.chunks_exact(4)).enumerate() {
        let tint = if w > 0 && i / w == edge { line } else { panel };
        // how much the white showed through, averaged over the channels: 0 = solid, 255 = not drawn on
        let through = (0..3).map(|k| (wt[k] as u32).saturating_sub(b[k] as u32)).sum::<u32>() / 3;
        let a = 255 - through.min(255);
        for k in 0..3 {
            b[k] = ((b[k] as u32).min(a) + tint[k] * (255 - a) / 255) as u8;
        }
        b[3] = (a + ta * (255 - a) / 255) as u8;
    }
}

/// Everything on the bar but its panel: buttons, icons, tray, clock. `quality` is the fonts' GDI quality.
unsafe fn draw_bar(dc: HDC, cr: RECT, quality: u32) {
    let h = cr.bottom;
    SetBkMode(dc, TRANSPARENT as i32);
    let (face, gface) = (wide(ui_face()), wide(winrust_common::style::icon_face()));
    let font = |f: &[u16], px: i32, wt: i32| CreateFontW(-sc(px), 0, 0, 0, wt, 0, 0, 0, 1, 0, 0, quality, 0, f.as_ptr());
    let (f_txt, f_small, f_glyph, f_bold) = (font(&face, 13, 400), font(&face, 12, 400), font(&gface, 16, 400), font(&face, 13, 600));
    let of = SelectObject(dc, f_txt);

    // the clock block is as wide as its text plus the same padding as the other blocks. Measured with every digit as 0
    // (and a two-digit hour) so the group does not shift when the time ticks from 9:59 to 10:00.
    let (tm, dt) = clock_texts();
    let zeros = |s: &str| s.chars().map(|c| if c.is_ascii_digit() { '0' } else { c }).collect::<String>();
    let tw = |f: HFONT, s: &str| {
        SelectObject(dc, f);
        let w = wide(s);
        let mut sz: SIZE = zeroed();
        GetTextExtentPoint32W(dc, w.as_ptr(), w.len() as i32 - 1, &mut sz);
        sz.cx
    };
    let tz = zeros(&tm);
    let tz = if tz.find(':') == Some(1) { format!("0{tz}") } else { tz };
    CLOCK_W.set(tw(f_bold, &tz).max(tw(f_small, &zeros(&dt))) + sc(16));
    SelectObject(dc, f_txt);

    let l = layout(cr.right);
    let fg = GetForegroundWindow();
    let (top, bot) = (sc(4), h - sc(4));
    let mid = (top + bot) / 2;
    let cell = |x: i32, w: i32| RECT { left: x, top, right: x + w, bottom: bot };
    let hot = HOT.get();
    let active_bg = mix(pal().active, pal().accent, 0.12); // the focused app's button: a hint of the accent, not plain grey

    // search / Start / app buttons as on Windows 11: the slot is the full bar height, the highlight inset 2 px at the
    // sides and 4 px at top and bottom (40x40 in a 44 px slot of a 48 px bar), corner radius 4
    let hl = |r: RECT| RECT { left: r.left + sc(2), top: r.top + sc(4), right: r.right - sc(2), bottom: r.bottom - sc(4) };

    // search button (left of Start)
    let sb = RECT { left: l.x0, top: 0, right: l.x0 + l.bw, bottom: h };
    glow(dc, SEARCH, hot == SEARCH, hl(sb), pal().bg, pal().hover, sc(8));
    SelectObject(dc, f_glyph);
    text(dc, "\u{E721}", sb, if hot == SEARCH { pal().text } else { mix(pal().dim, pal().text, 0.5) }, DT_CENTER);

    // Start, pinned apps, open windows (center)
    BTNS.with_borrow(|btns| {
        for (i, b) in btns.iter().enumerate() {
            let x = l.x0 + (i as i32 + 1) * (l.bw + l.gap);
            let r = RECT { left: x, top: 0, right: x + l.bw, bottom: h };
            let act = b.wins.contains(&fg);
            let (th, ta, tp) = (hv(i as i32, hot == i as i32), hv(2000 + i as i32, act), press_t(i as i32));
            if th > 0.01 || ta > 0.01 || tp > 0.0 {
                fill(dc, hl(r), mix(mix(mix(pal().bg, pal().hover, th), active_bg, ta), pal().accent, 0.5 * tp), sc(8));
            }
            if i == 0 {
                // Windows logo in four tiles, lit from the top left; grows a little on hover, squeezes on click
                let t = ((sc(8) as f32) * (1.0 + 0.12 * th) * (1.0 - 0.2 * tp)) as i32;
                let (g, cx) = (sc(1).max(1), (r.left + r.right) / 2);
                let tint = [mix(pal().accent, pal().text, 0.35), mix(pal().accent, pal().text, 0.15), mix(pal().accent, pal().text, 0.15), pal().accent];
                for (k, (dx, dy)) in [(-1, -1), (0, -1), (-1, 0), (0, 0)].into_iter().enumerate() {
                    let (px, py) = (cx + if dx < 0 { -t - g } else { g }, mid + if dy < 0 { -t - g } else { g });
                    fill(dc, RECT { left: px, top: py, right: px + t, bottom: py + t }, tint[k], sc(3));
                }
                continue;
            }
            let ic = match b.pin {
                Some(p) => PINS.with_borrow(|pins| pins.get(p).map_or(null_mut(), |p| p.icon)),
                None => b.icon,
            };
            let key = b.pin.map(|p| -(p as isize) - 1).or_else(|| b.wins.first().map(|&w| w as isize)).unwrap_or(0);
            // squeezes on click, grows in when new; centred in the button like Windows 11 (no hover lift)
            let isz = (icon_px().min(l.bw - sc(6)) as f32 * (1.0 - 0.18 * tp) * (0.55 + 0.45 * appear(key))) as i32;
            if !ic.is_null() {
                DrawIconEx(dc, x + (l.bw - isz) / 2, h / 2 - isz / 2, ic, isz, isz, 0, null_mut(), DI_NORMAL);
            }
            if !b.wins.is_empty() {
                let min = b.wins.iter().all(|&w| IsIconic(w) != 0);
                let pw = sc(6) + (sc(10) as f32 * ta) as i32; // the running-app line: 6 px, 16 px for the active window
                let py = hl(r).bottom - sc(1) - sc(3); // 3 px tall, at the bottom of the highlight
                let col = if act { pal().accent } else if min { mix(pal().dim, pal().bg, 0.45) } else { pal().dim };
                fill(dc, RECT { left: x + (l.bw - pw) / 2, top: py, right: x + (l.bw + pw) / 2, bottom: py + sc(3) }, col, sc(3));
                if b.wins.len() > 1 {
                    // a second, shorter pill: more than one window behind this button
                    fill(dc, RECT { left: x + (l.bw + pw) / 2 + sc(2), top: py, right: x + (l.bw + pw) / 2 + sc(5), bottom: py + sc(3) }, col, sc(3));
                }
            }
        }
    });

    // a hairline between the app buttons and the tray side (not on the clear bar: nothing to divide, it would float alone)
    if i18n::style::bar_opacity() > 0 {
        fill(dc, RECT { left: l.apps.0 - sc(5), top: mid - sc(9), right: l.apps.0 - sc(5) + 1, bottom: mid + sc(9) }, pal().line, 0);
    }

    // tray apps
    glow(dc, TAPP, hot == TAPP, cell(l.apps.0, sc(24)), pal().bg, pal().hover, sc(8));
    SelectObject(dc, f_glyph);
    let open_tf = IsWindowVisible(TRAYFLY.get()) != 0;
    let turn = ease(hv(TF_KEY, open_tf)); // the chevron turns over as the flyout opens, and back
    let cc = cell(l.apps.0, sc(24));
    chevron(dc, (cc.left + cc.right) / 2, mid, sc(5), if at_top() { 180.0 } else { 0.0 } + 180.0 * turn, mix(pal().text, pal().accent, turn));
    SelectObject(dc, f_txt);
    let bar = bar_idx();
    TAPPS.with_borrow(|t| {
        for (k, &ix) in bar.iter().enumerate() {
            let (i, ti) = (k + 1, &t[ix]);
            let x = l.apps.0 + i as i32 * sc(24);
            glow(dc, TAPP + i as i32, hot == TAPP + i as i32, cell(x, sc(24)), pal().bg, pal().hover, sc(8));
            if !ti.icon.is_null() {
                DrawIconEx(dc, x + sc(4), mid - sc(8), ti.icon, sc(16), sc(16), 0, null_mut(), DI_NORMAL);
            }
        }
    });

    // system group: one pill for wifi + volume + battery (like Windows 11: they open one panel)
    let tb = RECT { left: l.sys.0, top, right: l.sys.1, bottom: bot };
    let grp = tb;
    let (tg, pg) = (hv(SYS_GRP, matches!(hot, WIFI_I | VOL_I | BAT_I) || fly::is_open(fly::Kind::Quick)), press_t(WIFI_I).max(press_t(VOL_I)).max(press_t(BAT_I)));
    if tg > 0.01 || pg > 0.0 {
        fill(dc, grp, mix(mix(pal().bg, pal().hover, tg), pal().accent, 0.5 * pg), sc(8));
    }
    // each icon centred in its own cell: equal space around both, and the pill's edges as far from them as they are apart
    let c = sc(SYS_CELL);
    net_icon(dc, tb.left + c / 2, mid, sc(17), f_glyph);
    SelectObject(dc, f_glyph);
    let (vol, muted) = VOL.get();
    let vg = if muted || vol == 0 { "\u{E74F}" } else if vol < 34 { "\u{E993}" } else if vol < 67 { "\u{E994}" } else { "\u{E995}" };
    text(dc, vg, cell(tb.left + c, c), if muted { BAD } else { pal().text }, DT_CENTER);
    let x = tb.left + 2 * c + sc(6);
    let (pct, ac) = BAT.get();
    if pct >= 0 {
        let col = if ac { GOOD } else if pct <= 15 { BAD } else { pal().text };
        let (bw, bh, by) = (sc(22), sc(11), mid - sc(5));
        fill(dc, RECT { left: x, top: by, right: x + bw, bottom: by + bh }, col, sc(4)); // outline
        fill(dc, RECT { left: x + sc(1), top: by + sc(1), right: x + bw - sc(1), bottom: by + bh - sc(1) }, mix(pal().bg, pal().hover, tg), sc(3));
        let fw = ((bw - sc(4)) * pct / 100).max(sc(1));
        fill(dc, RECT { left: x + sc(2), top: by + sc(2), right: x + sc(2) + fw, bottom: by + bh - sc(2) }, col, sc(2));
        fill(dc, RECT { left: x + bw + sc(1), top: by + sc(3), right: x + bw + sc(3), bottom: by + bh - sc(3) }, col, sc(1)); // terminal nub
        SelectObject(dc, f_txt);
        text(dc, &format!("{pct}%"), cell(x + bw + sc(8), sc(40)), pal().text, DT_LEFT);
    }

    // time (semibold) over the date (dimmed)
    let cb = cell(l.clock.0, l.clock.1 - l.clock.0);
    glow(dc, CLOCK, hot == CLOCK || fly::is_open(fly::Kind::Cal), cb, pal().bg, pal().hover, sc(8));
    SelectObject(dc, f_bold);
    text(dc, &tm, RECT { left: cb.left, top: cb.top, right: cb.right, bottom: mid + sc(1) }, pal().text, DT_CENTER);
    SelectObject(dc, f_small);
    text(dc, &dt, RECT { left: cb.left, top: mid - sc(1), right: cb.right, bottom: cb.bottom }, mix(pal().dim, pal().text, 0.35), DT_CENTER);

    SelectObject(dc, of);
    for f in [f_txt, f_small, f_glyph, f_bold] {
        DeleteObject(f);
    }
}

// ---------- input ----------

/// Presses the keys in order, releases them in reverse (e.g. Win+A).
unsafe fn keys(vks: &[u16]) {
    let send = |vk: u16, up: bool| {
        let mut i: INPUT = zeroed();
        i.r#type = INPUT_KEYBOARD;
        i.Anonymous.ki.wVk = vk;
        i.Anonymous.ki.dwFlags = if up { KEYEVENTF_KEYUP } else { 0 };
        SendInput(1, &i, size_of::<INPUT>() as i32);
    };
    vks.iter().for_each(|&v| send(v, false));
    vks.iter().rev().for_each(|&v| send(v, true));
}

/// WM_CLOSE. A window of an elevated app ignores it from our normal-rights process (UIPI), so then
/// ask for admin once (UAC) and let an elevated taskkill send the close request.
unsafe fn close_window(w: HWND) {
    if PostMessageW(w, WM_CLOSE, 0, 0) == 0 {
        let mut pid = 0u32;
        GetWindowThreadProcessId(w, &mut pid);
        ShellExecuteW(null_mut(), wide("runas").as_ptr(), wide("taskkill.exe").as_ptr(), wide(&format!("/PID {pid}")).as_ptr(), null(), SW_HIDE);
    }
}

/// Opens a file, shortcut or URI through explorer, which starts it with the desktop user's normal token: the bar itself
/// may run elevated (see the launcher), and everything started from it must not inherit that.
unsafe fn open(path: &[u16]) {
    let n = path.iter().position(|&c| c == 0).unwrap_or(path.len());
    let p = String::from_utf16_lossy(&path[..n]);
    if p.to_lowercase().ends_with(".lnk") {
        bump(&p);
    }
    if p.to_lowercase().starts_with("ms-settings:") {
        return winrust_common::settings::open(&p); // needs Explorer as shell: it is started for the time Settings is open
    }
    // A Store (MSIX) app from the Apps folder: "explorer.exe shell:AppsFolder\<AUMID>" fails with "Class not registered"
    // when explorer is not the running shell (rs-shell). Then the activation service starts it directly and needs no
    // explorer (packaged desktop apps like 1Password run fine). Only then: activated from this elevated bar, a UWP app
    // (Calculator) hangs on its splash screen, so with explorer as the shell it goes the usual way below.
    // On its own thread: activation waits until the app has started.
    let aumid = p.get(..17).filter(|h| h.eq_ignore_ascii_case(r"shell:AppsFolder\")).and(p.get(17..)).filter(|a| a.contains('!'));
    if let Some(aumid) = aumid.filter(|_| !EXPLORER_UP.load(Relaxed)) {
        let (aumid, p) = (aumid.to_string(), p.clone());
        let t = thread::spawn(move || unsafe {
            if !activate_app(&aumid) {
                open_via_explorer(&p);
            }
        });
        if OPEN_WAIT.load(Relaxed) {
            let _ = t.join(); // `topbar --open`: the process must not end before the launch did
        }
        return;
    }
    open_via_explorer(&p);
}

static OPEN_WAIT: AtomicBool = AtomicBool::new(false);

/// Always through explorer.exe: it starts the target with the desktop user's normal token. ShellExecuteW from here would
/// inherit this bar's admin rights, and an elevated app cannot receive files dragged in from normal windows (UIPI), and
/// PrtSc / Win+Shift+S (ms-screenclip:) do not work when started elevated. With an argument and no explorer shell
/// running, explorer.exe hands the target to its COM host and does not become a shell or raise a taskbar.
unsafe fn open_via_explorer(p: &str) {
    if Command::new("explorer.exe").arg(p).spawn().is_err() {
        ShellExecuteW(null_mut(), wide("open").as_ptr(), wide(p).as_ptr(), null(), null(), SW_SHOWNORMAL);
    }
}

/// Starts a packaged app by its AppUserModelID ("Agilebits.1Password_amwd9z03whsfe!Agilebits.OnePassword") through the
/// activation service. false = it could not (then the caller tries explorer). Classic UWP apps (Calculator...) still need
/// explorer's shell host and fail here without it; packaged desktop apps (1Password, Claude...) start fine.
unsafe fn activate_app(aumid: &str) -> bool {
    use windows::Win32::{
        System::Com::CLSCTX_LOCAL_SERVER,
        UI::Shell::{AO_NONE, ApplicationActivationManager, IApplicationActivationManager},
    };
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let Ok(m) = CoCreateInstance::<_, IApplicationActivationManager>(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER) else { return false };
    let id = wide(aumid);
    m.ActivateApplication(PCWSTR(id.as_ptr()), PCWSTR::null(), AO_NONE).is_ok()
}

/// Starts the disk cleaner (rs-du, installed next to the task manager) elevated: it reads the raw volume.
/// Right-click menu of an app button, like the Windows taskbar's jump list: pin / unpin, close, end process.
fn wstr_of(w: &[u16]) -> String {
    String::from_utf16_lossy(&w[..w.iter().position(|&c| c == 0).unwrap_or(w.len())])
}

/// "1 окно", "3 окна", "5 окон".
fn windows_ru(n: usize) -> String {
    if en!() {
        return format!("{n} {}", if n == 1 { "window" } else { "windows" });
    }
    let w = match (n % 10, n % 100) {
        (1, x) if x != 11 => "окно",
        (2..=4, x) if !(12..=14).contains(&x) => "окна",
        _ => "окон",
    };
    format!("{n} {w}")
}

/// Right-click menu of an app button, like the Windows 11 jump list: the app on top, its windows (when there are several),
/// open / pin / Start, then close and end process in red.
unsafe fn app_menu(hwnd: HWND, h: i32) {
    #[derive(Clone, Copy)]
    enum A {
        Win(HWND),
        Open,
        Pin,
        Start,
        Close,
        Kill,
    }
    let (wins, pin, bicon) = BTNS.with_borrow(|b| b.get(h as usize).map(|b| (b.wins.clone(), b.pin, b.icon)).unwrap_or((vec![], None, null_mut())));
    let exe = wins.first().map(|&w| exe_of(w)).unwrap_or_default();
    let (lnk, picon) = pin.and_then(|p| PINS.with_borrow(|p2| p2.get(p).map(|x| (wstr_of(&x.lnk), x.icon)))).unwrap_or_default();
    let launch = if lnk.is_empty() { exe.clone() } else { lnk.clone() };
    let stem = std::path::Path::new(&launch).file_stem().unwrap_or_default().to_string_lossy().to_string();
    let name: String = stem.chars().take(1).flat_map(char::to_uppercase).chain(stem.chars().skip(1)).collect();
    let sub = match (wins.len(), pin.is_some()) {
        (0, _) => t!("Закреплено · не запущено", "Pinned · not running").to_string(),
        (n, true) => tf!("Закреплено · {}", "Pinned · {}", windows_ru(n)),
        (n, false) => tf!("Запущено · {}", "Running · {}", windows_ru(n)),
    };
    let icon = if picon.is_null() { bicon } else { picon };
    let mut items: Vec<(fly::MItem, A)> = Vec::new();
    if wins.len() > 1 {
        for &w in wins.iter().take(6) {
            let mut b = [0u16; 256];
            let n = GetWindowTextW(w, b.as_mut_ptr(), 256).max(0) as usize;
            items.push((fly::MItem { icon: icon_of(w), ..fly::MItem::new(String::from_utf16_lossy(&b[..n])) }, A::Win(w)));
        }
    }
    if !launch.is_empty() {
        let it = fly::MItem::new(if wins.is_empty() { t!("Открыть", "Open") } else { t!("Новое окно", "New window") });
        items.push((if items.is_empty() { it } else { it.sep() }, A::Open));
    }
    if pin.is_some() {
        items.push((fly::MItem::new(t!("Открепить от панели", "Unpin from the bar")), A::Pin));
    } else if !exe.is_empty() {
        items.push((fly::MItem::new(t!("Закрепить на панели", "Pin to the bar")), A::Pin));
    }
    if !launch.is_empty() {
        let s = fly::start_pinned(&launch);
        items.push((fly::MItem::new(if s { t!("Убрать из Пуска", "Remove from Start") } else { t!("Добавить в Пуск", "Add to Start") }), A::Start));
    }
    if !wins.is_empty() {
        items.push((fly::MItem::new(if wins.len() > 1 { t!("Закрыть все окна", "Close all windows") } else { t!("Закрыть окно", "Close window") }).hint("Alt+F4").sep(), A::Close));
        items.push((fly::MItem::new(t!("Завершить процесс", "End process")).danger(), A::Kill));
    }
    let acts: Vec<A> = items.iter().map(|x| x.1).collect();
    let items = items.into_iter().map(|x| x.0).collect();
    fly::menu(tip_cx(&bar_layout(hwnd), h), Some((icon, "", name, sub)), items, move |i| {
        match acts.get(i).copied() {
            Some(A::Win(w)) => {
                if IsIconic(w) != 0 {
                    ShowWindow(w, SW_RESTORE);
                }
                force_fg(w);
            }
            Some(A::Open) => open(&wide(&launch)),
            Some(A::Pin) => {
                // by path, not by the index seen when the menu opened: PINS may have changed meanwhile
                match PINS.with_borrow(|p| p.iter().position(|x| !lnk.is_empty() && wstr_of(&x.lnk) == lnk)) {
                    Some(p) => unpin(p),
                    None if lnk.is_empty() => pin_exe(&exe),
                    None => {}
                }
                refresh_buttons(); // the buttons hold indices into PINS: a stale one would index past the end on the next paint
            }
            Some(A::Start) => fly::start_toggle(&launch),
            Some(A::Close) => wins.iter().for_each(|&w| close_window(w)),
            Some(A::Kill) => {
                let mut pids: Vec<u32> = wins
                    .iter()
                    .map(|&w| {
                        let mut pid = 0u32;
                        GetWindowThreadProcessId(w, &mut pid);
                        pid
                    })
                    .collect();
                pids.sort_unstable();
                pids.dedup(); // several windows of one app: one kill (and at most one UAC prompt), not one per window
                for pid in pids {
                    let p = OpenProcess(1, 0, pid); // PROCESS_TERMINATE; works on elevated apps when the bar itself runs elevated
                    if p.is_null() || TerminateProcess(p, 1) == 0 {
                        ShellExecuteW(null_mut(), wide("runas").as_ptr(), wide("taskkill.exe").as_ptr(), wide(&format!("/F /T /PID {pid}")).as_ptr(), null(), SW_HIDE);
                    }
                    if !p.is_null() {
                        CloseHandle(p);
                    }
                }
            }
            None => {}
        }
        InvalidateRect(BAR.load(Relaxed) as HWND, null(), 0);
    });
}

fn pin_dir() -> PathBuf {
    appdata_file(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar")
}

unsafe fn unpin(i: usize) {
    let Some(p) = PINS.with_borrow_mut(|p| (i < p.len()).then(|| p.remove(i))) else { return };
    let n = p.lnk.iter().position(|&c| c == 0).unwrap_or(p.lnk.len());
    let _ = std::fs::remove_file(String::from_utf16_lossy(&p.lnk[..n]));
    DestroyIcon(p.icon);
}

/// Pins a running app: a shortcut to its exe in the folder where Windows keeps taskbar pins.
/// A dropped .lnk pins its target. Already pinned (same target) = nothing.
unsafe fn pin_exe(path: &str) {
    // targets are compared with exe_of(), which is lower-case
    let exe = if path.to_lowercase().ends_with(".lnk") { lnk_target(&wide(path)) } else { path.to_lowercase() };
    let exe = exe.as_str();
    if exe.is_empty() || PINS.with_borrow(|p| p.iter().any(|p| p.target == exe)) {
        return;
    }
    let stem = std::path::Path::new(exe).file_stem().unwrap_or_default().to_string_lossy().to_string();
    let name: String = stem.chars().enumerate().map(|(i, c)| if i == 0 { c.to_ascii_uppercase() } else { c }).collect();
    let _ = std::fs::create_dir_all(pin_dir());
    let lnk = wide(&pin_dir().join(format!("{name}.lnk")).to_string_lossy());
    let ok = (|| -> Option<()> {
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        sl.SetPath(PCWSTR(wide(exe).as_ptr())).ok()?;
        sl.cast::<IPersistFile>().ok()?.Save(PCWSTR(lnk.as_ptr()), true).ok()
    })();
    if ok.is_some() {
        let icon = lnk_icon(&lnk, exe);
        PINS.with_borrow_mut(|p| p.push(Pin { lnk, target: exe.to_string(), icon }));
    }
}

/// Files dropped on the bar: on the Start button they are added to Start (the panel opens and the new row pulses),
/// anywhere else they are pinned to the bar.
unsafe fn on_drop(hwnd: HWND, hd: HDROP) {
    let mut pt: POINT = zeroed();
    DragQueryPoint(hd, &mut pt);
    let n = DragQueryFileW(hd, u32::MAX, null_mut(), 0);
    let files: Vec<String> = (0..n)
        .map(|i| {
            let len = DragQueryFileW(hd, i, null_mut(), 0) as usize;
            let mut b = vec![0u16; len + 1];
            DragQueryFileW(hd, i, b.as_mut_ptr(), b.len() as u32);
            String::from_utf16_lossy(&b[..len])
        })
        .collect();
    DragFinish(hd);
    let h = hit(hwnd, pt.x);
    press(if h == NONE { 0 } else { h });
    if h == 0 {
        let added: Vec<&String> = files.iter().filter(|f| fly::start_pin(f)).collect();
        let name = |p: &str| std::path::Path::new(p).file_stem().unwrap_or_default().to_string_lossy().to_string();
        if !fly::is_open(fly::Kind::Start) {
            fly::toggle(fly::Kind::Start);
        }
        fly::note(&match added.len() {
            0 => "Already in Start".to_string(),
            1 => format!("Added to Start: {}", name(added[0])),
            n => format!("Added {n} items to Start"),
        });
    } else {
        files.iter().for_each(|f| pin_exe(f));
        refresh_buttons();
    }
    InvalidateRect(hwnd, null(), 0);
}

/// The junk remover (purge, installed on request): lists what it would delete and asks for YES in its own elevated console.
pub(crate) fn purge_installed() -> bool {
    std::env::var_os("ProgramFiles").is_some_and(|pf| PathBuf::from(pf).join(r"purge\purge.exe").is_file())
}

unsafe fn open_purge() {
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        let exe = PathBuf::from(pf).join(r"purge\purge.exe");
        ShellExecuteW(null_mut(), wide("runas").as_ptr(), wide(&exe.to_string_lossy()).as_ptr(), wide("--ask").as_ptr(), null(), SW_SHOWNORMAL);
    }
}

unsafe fn open_cleaner() {
    let Some(pf) = std::env::var_os("ProgramFiles") else { return };
    let exe = PathBuf::from(pf).join(r"rtaskmgr\rs-du.exe");
    if exe.is_file() {
        ShellExecuteW(null_mut(), wide("runas").as_ptr(), wide(&exe.to_string_lossy()).as_ptr(), wide("C: --clean --ask").as_ptr(), null(), SW_SHOWNORMAL);
    }
}

unsafe extern "system" fn find_launcher(h: HWND, lp: LPARAM) -> i32 {
    let mut cls = [0u16; 32];
    let n = GetClassNameW(h, cls.as_mut_ptr(), 32).max(0) as usize;
    if String::from_utf16_lossy(&cls[..n]) == "rs_launcher" {
        *(lp as *mut HWND) = h;
        return 0;
    }
    1
}

/// Asks the running rs-launcher to show itself by sending the message its Alt+Space hotkey produces.
/// Faking the key presses did not reliably reach it from this (elevated) process. false = it is not running.
unsafe fn open_launcher() -> bool {
    let mut h: HWND = null_mut();
    EnumWindows(Some(find_launcher), &mut h as *mut _ as LPARAM);
    if h.is_null() {
        return false;
    }
    PostMessageW(h, WM_HOTKEY, 1, (MOD_ALT | MOD_NOREPEAT) as isize | ((VK_SPACE as isize) << 16));
    true
}

/// The Style menu ("Bar style…" in the empty-spot right-click menu), opened at `cx`: the UI style (saved for the other
/// WinRust programs), auto-hide, the panel's opacity and putting the windows side by side.
unsafe fn style_menu(cx: i32) {
    let bar = BAR.load(Relaxed) as HWND;
    let names = [t!("Обычный", "Normal"), t!("Терминал", "Terminal"), t!("Плавающий док", "Floating dock"), t!("Тайлинг", "Tiling"), t!("Белая дорога", "White road")];
    let mut items: Vec<fly::MItem> = i18n::style::ALL.iter().zip(names).map(|(&s, n)| fly::MItem::new(n).on(s == style())).collect();
    items.push(fly::MItem::new(t!("Разложить окна плиткой", "Tile the windows")).sep());
    items.push(fly::MItem::new(t!("Скрывать панель автоматически", "Hide the bar automatically")).on(i18n::style::autohide()));
    // the panel behind the buttons: solid, translucent or clear (the wallpaper shows through; good on OLED screens)
    let op = i18n::style::bar_opacity();
    items.push(fly::MItem::new(t!("Сплошная панель", "Solid bar")).on(op == 100).sep());
    items.push(fly::MItem::new(t!("Полупрозрачная панель", "Translucent bar")).on((1..100).contains(&op)));
    items.push(fly::MItem::new(t!("Прозрачная панель", "Clear bar")).on(op == 0));
    let head = (null_mut(), "\u{E713}", t!("Стиль", "Style").to_string(), t!("Вид панели и программ WinRust", "Look of the bar and the WinRust programs").to_string());
    let (tile, hide) = (i18n::style::ALL.len(), i18n::style::ALL.len() + 1);
    fly::menu(cx, Some(head), items, move |i| match i18n::style::ALL.get(i) {
        Some(&s) => {
            i18n::style::set_style(s);
            place(bar);
        }
        None if i == tile => tile_windows(),
        None if i == hide => {
            i18n::style::set_autohide(!i18n::style::autohide());
            place(bar); // the reserved strip comes or goes with it
            bar_anim(bar);
        }
        None => {
            // a hand-set translucency (bar-opacity.txt) stays when Translucent is picked again
            let pct = match i - hide {
                1 => 100,
                2 if (1..100).contains(&op) => op,
                2 => 50,
                _ => 0,
            };
            i18n::style::set_bar_opacity(pct);
            InvalidateRect(bar, null(), 0);
        }
    });
}

/// Puts the open, not minimized windows side by side in the work area.
unsafe fn tile_windows() {
    let wins: Vec<HWND> = BTNS.with_borrow(|b| b.iter().flat_map(|b| b.wins.iter().copied()).filter(|&w| IsIconic(w) == 0).collect());
    if wins.is_empty() {
        return;
    }
    let mut wa: RECT = zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut _ as *mut _, 0);
    for &w in &wins {
        ShowWindow(w, SW_RESTORE); // a maximized window ignores tiling
    }
    TileWindows(null_mut(), MDITILE_VERTICAL, &wa, wins.len() as u32, wins.as_ptr());
}

unsafe fn click(h: i32, left: bool, mid: bool) {
    match h {
        SEARCH if left => {
            // our search (rs-launcher listens for Alt+Space); without it the Start panel's own search
            if std::env::var_os("ProgramFiles").is_some_and(|p| PathBuf::from(p).join(r"rs-search\rs-launcher.exe").is_file()) {
                if !open_launcher() {
                    keys(&[VK_MENU, VK_SPACE]);
                }
            } else {
                fly::toggle(fly::Kind::Start);
            }
        }
        // left: the language picker; right: straight to the next one (Win+Space is explorer's, gone under rs-shell)
        LANG if left => lang_menu(BAR.load(Relaxed) as HWND),
        LANG => next_lang(BAR.load(Relaxed) as HWND),
        WIFI_I => fly::toggle(if left { fly::Kind::Quick } else { fly::Kind::Wifi }), // right click: the network list
        VOL_I | BAT_I => fly::toggle(fly::Kind::Quick),
        CLOCK if left => fly::toggle(fly::Kind::Cal),
        TAPP..=-900 => {
            let slot = (h - TAPP) as usize;
            if slot == 0 {
                if left {
                    fly_toggle();
                }
            } else if mid {
                if let Some(k) = bar_idx().get(slot - 1).and_then(|&i| TAPPS.with_borrow(|t| t.get(i).map(|x| x.key.clone()))) {
                    toggle_shown(&k); // back into the flyout
                }
            } else {
                let ti = bar_idx().get(slot - 1).and_then(|&i| TAPPS.with_borrow(|t| t.get(i).map(|x| (x.hwnd, x.uid, x.cb, x.ver))));
                ti.into_iter().for_each(|ti| tray_click(ti, left));
            }
        }
        0 => {
            if left {
                fly::toggle(fly::Kind::Start);
            }
        }
        1.. => {
            let (wins, lnk) = BTNS.with_borrow(|b| b.get(h as usize).map(|b| (b.wins.clone(), b.pin.and_then(|p| PINS.with_borrow(|p2| p2.get(p).map(|x| x.lnk.clone()))))).unwrap_or_default());
            match (wins.first(), left) {
                // not running: launch the pinned app (a Store app may have updated since the pins were loaded)
                (None, true) => lnk.iter().for_each(|l| {
                    PINS.with_borrow_mut(|p| p.iter_mut().filter(|x| x.lnk == *l).for_each(|x| x.target = fix_msix_pin(l, &x.target).unwrap_or(std::mem::take(&mut x.target))));
                    open(l)
                }),
                (Some(&w), false) => close_window(w), // right / middle click
                (Some(&w), true) => {
                    // like the Windows taskbar: the active app's button minimizes it, a minimized one restores, any other raises
                    let fg = GetForegroundWindow();
                    if wins.contains(&fg) && IsIconic(fg) == 0 {
                        ShowWindow(fg, SW_MINIMIZE);
                    } else {
                        if IsIconic(w) != 0 {
                            ShowWindow(w, SW_RESTORE);
                        }
                        SetForegroundWindow(w);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

#[repr(C)]
struct ShellHookInfo {
    hwnd: HWND,
    rc: RECT,
}

/// HSHELL_GETMINRECT: a window is minimizing / restoring and asks where its taskbar button is: the shrink animation
/// flies there instead of to the bottom left corner of the screen.
unsafe fn min_rect(info: *mut ShellHookInfo, bar: HWND) {
    let (w, l) = ((*info).hwnd, bar_layout(bar));
    let exe = exe_of(w);
    let i = BTNS.with_borrow(|b| b.iter().position(|b| b.wins.contains(&w)).or_else(|| b.iter().position(|b| !exe.is_empty() && b.wins.first().is_some_and(|&x| exe_of(x) == exe))));
    let x = match i {
        Some(i) => l.x0 + (i as i32 + 1) * (l.bw + l.gap),
        None => (l.x0 + l.apps.0) / 2, // no button (yet): the middle of the bar
    };
    let (top, x) = (bar_top() + sc(4), x + bar_x_w().0);
    (*info).rc = RECT { left: x, top, right: x + l.bw, bottom: top + bar_h() - sc(8) };
}

/// Screen x of the middle of a bar element, for its tooltip and menus.
unsafe fn tip_cx(l: &Layout, h: i32) -> i32 {
    bar_x_w().0 + match h {
        SEARCH => l.x0 + l.bw / 2,
        CLOCK => (l.clock.0 + l.clock.1) / 2,
        LANG => l.sys.0 + sc(21),
        WIFI_I => l.sys.0 + sc(SYS_CELL) / 2,
        VOL_I => l.sys.0 + sc(SYS_CELL) * 3 / 2,
        BAT_I => l.sys.0 + sc(2 * SYS_CELL + BAT_W / 2),
        t if (TAPP..TAPP + 100).contains(&t) => l.apps.0 + (t - TAPP) * sc(24) + sc(12),
        i => l.x0 + (i + 1) * (l.bw + l.gap) + l.bw / 2,
    }
}

unsafe fn bar_layout(hwnd: HWND) -> Layout {
    let mut r: RECT = zeroed();
    GetClientRect(hwnd, &mut r);
    layout(r.right)
}

/// Registers the bar as an appbar with explorer (again after explorer restarts: its list starts empty).
unsafe fn appbar_new(hwnd: HWND) {
    let mut ab: APPBARDATA = zeroed();
    ab.cbSize = size_of::<APPBARDATA>() as u32;
    ab.hWnd = hwnd;
    ab.uCallbackMessage = CALLBACK;
    SHAppBarMessage(ABM_NEW, &mut ab);
    EXP_TRAY.set(tray()[0]);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg != 0 && msg == SH_MSG.get() {
        if wp == 5 && lp != 0 {
            min_rect(lp as *mut ShellHookInfo, hwnd); // HSHELL_GETMINRECT
            return 1;
        }
        return 0;
    }
    if msg != 0 && msg == TB_MSG.get() {
        // also our own announce(): act only when explorer's taskbar is a new window (explorer restarted / came back)
        let t = tray()[0];
        if !t.is_null() && t != EXP_TRAY.get() {
            appbar_new(hwnd);
            hide_taskbar();
            place(hwnd);
        }
        return 0;
    }
    match msg {
        WM_PAINT => paint(hwnd),
        WM_ERASEBKGND => return 1,
        WM_TIMER if wp == 2 => {
            KillTimer(hwnd, 2);
            let h = HOT.get();
            if h != NONE && !fly::any_open() {
                show_tip(tip_text(h), tip_cx(&bar_layout(hwnd), h));
            }
        }
        WM_TIMER if wp == 6 => hide_tip(hwnd), // the volume readout after a wheel turn
        WM_TIMER if wp == 3 => fs_update(hwnd),
        WM_TIMER if wp == 7 => {
            KillTimer(hwnd, 7);
            let (tid, before) = LANG_TRY.get();
            if GetKeyboardLayout(tid) as usize == before {
                let w = app_window();
                if GetForegroundWindow() != w {
                    force_fg(w); // Win+Space switches the window in front: put the app back there first
                }
                keys(&[VK_LWIN, VK_SPACE]); // the app ignored the request: the system switcher always works (with explorer)
            }
            InvalidateRect(hwnd, null(), 0); // the label shows the new language right away
        }
        WM_TIMER if wp == 4 => {
            InvalidateRect(hwnd, null(), 0);
        }
        WM_TIMER if wp == 5 => bar_step(hwnd),
        WIN_START => fly::toggle(fly::Kind::Start),
        ALT_TAB => alt_tab(wp == 1),
        ALT_UP => alt_done(wp == 1),
        SNIP if wp == 1 => screenshot_notice(hwnd),
        SNIP if wp == 2 => clip_notice(hwnd),
        SNIP => clip::start(),
        STYLE_MENU => style_menu(wp as i32),
        NEXT_LANG => {
            // Win+Space without explorer: the next input language, named in a tip over the bar (there is no button for it)
            let (list, cur) = layouts();
            if list.len() > 1 {
                let next = list[list.iter().position(|&k| k == cur).map_or(0, |i| i + 1) % list.len()];
                next_lang(hwnd);
                KillTimer(hwnd, 2);
                show_tip(layout_names(next).0, tip_cx(&bar_layout(hwnd), CLOCK));
                SetTimer(hwnd, 6, 1200, None);
            }
        }
        VOLKEY => {
            if wp == 2 { toggle_mute() } else { add_volume(if wp == 0 { 4 } else { -4 }) }
            KillTimer(hwnd, 2);
            show_tip(vol_tip(), tip_cx(&bar_layout(hwnd), VOL_I));
            SetTimer(hwnd, 6, 1200, None);
            InvalidateRect(hwnd, null(), 0);
        }
        WM_DROPFILES => on_drop(hwnd, wp as HDROP),
        WM_TIMER => {
            hide_taskbar_if_shown();
            refresh();
            // repaint (GDI+, the costly part) only when something shown changed; every 10th tick anyway as a safety net
            let (last, quiet) = SIG.get();
            let s = view_sig();
            if s != last || quiet >= 10 {
                SIG.set((s, 0));
                InvalidateRect(hwnd, null(), 0);
            } else {
                SIG.set((s, quiet + 1));
            }
        }
        WM_MOUSEMOVE => {
            let h = hit(hwnd, (lp & 0xffff) as i16 as i32);
            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
            if HOT.replace(h) != h {
                hide_tip(hwnd);
                if h != NONE {
                    SetTimer(hwnd, 2, 450, None);
                }
                InvalidateRect(hwnd, null(), 0);
            }
        }
        WM_MOUSELEAVE => {
            hide_tip(hwnd);
            HOT.set(-1);
            InvalidateRect(hwnd, null(), 0);
        }
        WM_MOUSEWHEEL => {
            add_volume(if (wp >> 16) as i16 > 0 { 2 } else { -2 }); // wheel anywhere on the bar = volume
            press(VOL_I); // the speaker flashes so it is clear what the wheel changed
            KillTimer(hwnd, 2);
            if !fly::any_open() {
                show_tip(vol_tip(), tip_cx(&bar_layout(hwnd), VOL_I)); // and the level shows above it for a moment
                SetTimer(hwnd, 6, 1200, None);
            }
            InvalidateRect(hwnd, null(), 0);
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            let x = (lp & 0xffff) as i16 as i32;
            let h = hit(hwnd, x);
            hide_tip(hwnd);
            if h != NONE {
                press(h);
            }
            if h == NONE && msg == WM_RBUTTONDOWN {
                fly::context_menu(x); // right click on an empty spot
            } else if h >= 1 && msg == WM_RBUTTONDOWN {
                app_menu(hwnd, h);
            } else {
                click(h, msg == WM_LBUTTONDOWN, msg == WM_MBUTTONDOWN);
            }
            InvalidateRect(hwnd, null(), 0);
        }
        CALLBACK => {
            if wp == ABN_FULLSCREENAPP as usize {
                fs_update(hwnd);
            } else if wp == ABN_POSCHANGED as usize {
                place(hwnd);
            }
        }
        WM_DISPLAYCHANGE => place(hwnd),
        WM_DESTROY => {
            let mut ab: APPBARDATA = zeroed();
            ab.cbSize = size_of::<APPBARDATA>() as u32;
            ab.hWnd = hwnd;
            SHAppBarMessage(ABM_REMOVE, &mut ab);
            if find_other("Shell_TrayWnd").is_null() {
                set_work_area(RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) }); // ours: give the strip back
            }
            show_taskbar(OLD_STATE.get());
            forget_taskbar_state();
            DestroyWindow(HOST.replace(null_mut()));
            announce(); // icons go back to explorer's tray
            PostQuitMessage(0);
        }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}

fn main() {
    unsafe {
        if std::env::args().any(|a| a == "--restore") {
            show_taskbar(saved_taskbar_state().unwrap_or(ABS_ALWAYSONTOP as usize));
            forget_taskbar_state();
            if find_other("Shell_TrayWnd").is_null() {
                set_work_area(RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) }); // a killed bar's reserved strip
            }
            announce(); // a killed bar took its tray icons with it; apps re-register with explorer
            return;
        }
        // `topbar --open <target>`: runs the bar's own launch code once and exits, to test how the (elevated) bar starts
        // things, e.g. `topbar --open "shell:AppsFolder\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"` from an admin shell
        if let Some(t) = std::env::args().skip_while(|a| a != "--open").nth(1) {
            EXPLORER_UP.store(!FindWindowW(wide("Shell_TrayWnd").as_ptr(), null()).is_null(), Relaxed);
            OPEN_WAIT.store(true, Relaxed);
            open(&wide(&t));
            return;
        }
        if std::env::args().any(|a| a == "--shot") {
            SetProcessDPIAware();
            let (mut tok, mut inp): (usize, GdiplusStartupInput) = (0, zeroed());
            inp.GdiplusVersion = 1;
            GdiplusStartup(&mut tok, &inp, null_mut());
            screenshot();
            return;
        }
        // `topbar --preview quick|wifi`: one flyout on its own (no bar, no hooks), to look at a style change; the process is closed by hand
        if let Some(p) = std::env::args().skip_while(|a| a != "--preview").nth(1) {
            SetProcessDPIAware();
            let (mut tok, mut inp): (usize, GdiplusStartupInput) = (0, zeroed());
            inp.GdiplusVersion = 1;
            GdiplusStartup(&mut tok, &inp, null_mut());
            fly::init(GetModuleHandleW(null()));
            fly::PREVIEW.store(true, Relaxed);
            fly::toggle(if p == "wifi" { fly::Kind::Wifi } else { fly::Kind::Quick });
            let mut msg: MSG = zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            return;
        }
        // Atomic single-instance guard (FindWindow alone raced when two launchers started us at once); the handle lives until exit.
        let m = windows_sys::Win32::System::Threading::CreateMutexW(null(), 0, wide("Local\\topbar-single").as_ptr());
        if m.is_null() || GetLastError() == ERROR_ALREADY_EXISTS || !FindWindowW(wide("topbar").as_ptr(), null()).is_null() {
            return; // already running
        }
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // Without explorer nobody hides minimized windows: they sit as small caption stubs at the bottom left of the screen.
        // ARW_HIDE moves them off screen (this session only: fWinIni 0 does not touch the saved setting).
        let mut mm: MINIMIZEDMETRICS = zeroed();
        mm.cbSize = size_of::<MINIMIZEDMETRICS>() as u32;
        if SystemParametersInfoW(SPI_GETMINIMIZEDMETRICS, mm.cbSize, &mut mm as *mut _ as *mut _, 0) != 0 {
            mm.iArrange |= ARW_HIDE as i32;
            SystemParametersInfoW(SPI_SETMINIMIZEDMETRICS, mm.cbSize, &mut mm as *mut _ as *mut _, 0);
        }
        let (mut gtok, mut gin): (usize, GdiplusStartupInput) = (0, zeroed());
        gin.GdiplusVersion = 1;
        GdiplusStartup(&mut gtok, &gin, null_mut()); // anti-aliased shapes; fill() falls back to GDI if this failed
        TB_MSG.set(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()));
        SH_MSG.set(RegisterWindowMessageW(wide("SHELLHOOK").as_ptr()));
        SetProcessDPIAware();
        let dc = GetDC(null_mut());
        SCALE.set(GetDeviceCaps(dc, LOGPIXELSX as i32) as f32 / 96.0);
        ReleaseDC(null_mut(), dc);
        SHOWN.set(std::fs::read_to_string(appdata_file("topbar-tray.txt")).ok().map(|s| s.lines().map(str::to_string).collect()));
        load_pins();
        fly::init(GetModuleHandleW(null()));
        thread::spawn(fly::load_apps);
        let hi = GetModuleHandleW(null());
        let class = wide("topbar");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hi, hCursor: LoadCursorW(null_mut(), IDC_ARROW), lpszClassName: class.as_ptr(), ..zeroed() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, GetSystemMetrics(SM_CXSCREEN), bar_h(), null_mut(), null_mut(), hi, null());
        // Our own Shell_TrayWnd (hidden, topmost, so apps find it before explorer's): apps register their tray icons here.
        let tc = wide("Shell_TrayWnd");
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(tray_proc), hInstance: hi, lpszClassName: tc.as_ptr(), ..zeroed() });
        HOST.set(CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, tc.as_ptr(), null(), WS_POPUP, 0, 0, 0, 0, null_mut(), null_mut(), hi, null()));
        // The bar may run elevated while apps and explorer do not; UIPI would drop their messages to us (icon data, appbar callbacks).
        ChangeWindowMessageFilterEx(HOST.get(), WM_COPYDATA, MSGFLT_ALLOW, null_mut());
        ChangeWindowMessageFilterEx(hwnd, CALLBACK, MSGFLT_ALLOW, null_mut());
        ChangeWindowMessageFilterEx(hwnd, TB_MSG.get(), MSGFLT_ALLOW, null_mut()); // explorer restarted (it runs unelevated)
        // files dragged from a normal-rights app onto the (elevated) bar: Start button = add to Start, elsewhere = pin
        DragAcceptFiles(hwnd, 1);
        for m in [WM_DROPFILES, 0x49 /* WM_COPYGLOBALDATA */, WM_COPYDATA] {
            ChangeWindowMessageFilterEx(hwnd, m, MSGFLT_ALLOW, null_mut());
        }
        let fc = wide("topbar_trayfly");
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(fly_proc), hInstance: hi, hCursor: LoadCursorW(null_mut(), IDC_ARROW), lpszClassName: fc.as_ptr(), ..zeroed() });
        TRAYFLY.set(CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED, fc.as_ptr(), null(), WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), hi, null()));
        let tipc = wide("topbar_tip");
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(tip_proc), hInstance: hi, lpszClassName: tipc.as_ptr(), ..zeroed() });
        TIPWND.set(CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, tipc.as_ptr(), null(), WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), hi, null()));
        let swc = wide("topbar_alttab");
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(sw_proc), hInstance: hi, lpszClassName: swc.as_ptr(), ..zeroed() });
        SWWND.set(CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, swc.as_ptr(), null(), WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), hi, null()));
        OLD_STATE.set(remember_taskbar_state());
        appbar_new(hwnd);
        // Without explorer nobody is the "taskman" window, so Windows treats minimized windows as loose icons and leaves
        // them as caption stubs on the desktop. Becoming that window (what explorer's taskbar does) makes them hide properly
        // and gets us HSHELL_GETMINRECT. Only when explorer's taskbar is absent: with it running its own registration stays.
        if find_other("Shell_TrayWnd").is_null() {
            if let Some(f) = GetProcAddress(GetModuleHandleW(wide("user32.dll").as_ptr()), b"SetTaskmanWindow\0".as_ptr()) {
                std::mem::transmute::<_, unsafe extern "system" fn(HWND) -> i32>(f)(hwnd);
            }
            RegisterShellHookWindow(hwnd);
        }
        hide_taskbar();
        place(hwnd);
        refresh();
        announce(); // apps already running re-add their icons to us
        SetTimer(hwnd, 1, 500, None);
        BAR.store(hwnd as isize, Relaxed);
        bar_anim(hwnd); // the bar slides in (it starts hidden above the screen)
        SetTimer(hwnd, 3, 250, None); // fullscreen watch
        thread::spawn(|| {
            if !SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_hook), GetModuleHandleW(null()), 0).is_null() {
                let mut m: MSG = zeroed();
                while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {}
            }
        });
        let mut m: MSG = zeroed();
        while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn network_icon_state() {
        use super::{INET, Link, WIFI, arcs_lit, net};
        let at = |q: i32, inet: bool| {
            WIFI.set(q);
            INET.set(inet);
            net()
        };
        assert!(at(80, true) == Link::Wifi(80));
        assert!(at(80, false) == Link::NoInternet(80)); // wifi without a way out: the warning icon
        assert!(at(-1, true) == Link::Cable);
        assert!(at(-1, false) == Link::Offline); // nothing at all: the crossed-out wifi icon
        assert_eq!((arcs_lit(0), arcs_lit(40), arcs_lit(100)), (1, 2, 3));
    }

    #[test]
    fn app_row_is_centred_and_recentres() {
        let x0 = |n| super::row_x0(2560, n, 44, 0, 300, 6);
        for n in [3, 9, 10, 15] {
            let row = n * 44;
            assert_eq!(x0(n) + row / 2, 2560 / 2, "row of {n} centred on the bar"); // even widths: exact
        }
        assert!(x0(10) < x0(9)); // one more app: the row grows to both sides, Start moves left
        // too many to centre: the row stops short of the tray (300 px from the right edge)
        assert_eq!(x0(50) + 50 * 44, 2560 - 300);
        // even more: never past the left edge
        assert_eq!(super::row_x0(800, 50, 44, 0, 300, 6), 6);
    }

    #[test]
    fn mix_blends() {
        assert_eq!(super::mix(super::rgb(0, 0, 0), super::rgb(200, 100, 50), 0.0), super::rgb(0, 0, 0));
        assert_eq!(super::mix(super::rgb(0, 0, 0), super::rgb(200, 100, 50), 1.0), super::rgb(200, 100, 50));
        assert_eq!(super::mix(super::rgb(0, 0, 0), super::rgb(200, 100, 50), 0.5), super::rgb(100, 50, 25));
    }

    #[test]
    fn msix_family_from_package_path() {
        let f = super::msix_family(r"C:\Program Files\WindowsApps\Claude_5.11492.5.0_x64__pzs8sxrjxfjjc\app\claude.exe");
        assert_eq!(f, Some(("claude_pzs8sxrjxfjjc".into(), r"app\claude.exe".into())));
        assert_eq!(super::msix_family(r"C:\Program Files\Steam\steam.exe"), None);
    }

    #[test]
    fn stale_claude_pin_resolves_to_installed_version() {
        // real system check: skipped where the Claude package is not installed (CI)
        let old = r"c:\program files\windowsapps\claude_5.11492.5.0_x64__pzs8sxrjxfjjc\app\claude.exe";
        let Some((fam, _)) = super::msix_family(old) else { panic!() };
        let mut n = 0u32;
        let mut len = 0u32;
        unsafe { windows_sys::Win32::Storage::Packaging::Appx::GetPackagesByPackageFamily(super::wide(&fam).as_ptr(), &mut n, std::ptr::null_mut(), &mut len, std::ptr::null_mut()) };
        if n == 0 {
            return;
        }
        let tmp = std::env::temp_dir().join("topbar-msix-test.lnk");
        let _ = std::fs::remove_file(&tmp);
        unsafe {
            use windows::Win32::System::Com::*;
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let sl: windows::Win32::UI::Shell::IShellLinkW = CoCreateInstance(&windows::Win32::UI::Shell::ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            sl.SetPath(windows::core::PCWSTR(super::wide(old).as_ptr())).unwrap();
            windows::core::Interface::cast::<IPersistFile>(&sl).unwrap().Save(windows::core::PCWSTR(super::wide(&tmp.to_string_lossy()).as_ptr()), true).unwrap();
            let lnk = super::wide(&tmp.to_string_lossy());
            let new = super::fix_msix_pin(&lnk, old).expect("re-pointed");
            assert!(std::path::Path::new(&new).exists() && new.ends_with(r"\app\claude.exe"), "{new}");
            assert_eq!(super::lnk_target(&lnk), new);
        }
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod env_tests {
    #[test]
    fn expands_known_variables_only() {
        let windir = std::env::var("windir").unwrap();
        assert_eq!(super::expand_env(r"%windir%\explorer.exe"), format!(r"{windir}\explorer.exe"));
        assert_eq!(super::expand_env(r"%no_such_var_x%\a.exe"), r"%no_such_var_x%\a.exe");
        assert_eq!(super::expand_env("100% sure"), "100% sure");
    }
}
