//! The file list drawn on the GPU (round A of the smoothness work): Direct2D and DirectWrite on a flip-model swap chain,
//! frames paced by the compositor (one per screen refresh while something moves, none when nothing does), scrolling by
//! the pixel with an eased glide. For now it shows the Details view, picks rows, opens them and sorts by the header;
//! the rest of what the list does still goes through the old list (settings switch "Smooth list (preview)" or
//! RX_NEWLIST=1). RX_FPS=1 writes the frame times of both lists to %TEMP%\rx-fps.log.
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use std::{collections::HashMap, sync::atomic::AtomicIsize, time::Instant};
use ui::{mix, pal, sc};
use windows::{
    core::{Interface, PCWSTR},
    Win32::Graphics::{
        Direct2D::{Common::*, *},
        Direct3D::*,
        Direct3D11::*,
        DirectWrite::*,
        Dxgi::{Common::*, *},
        Imaging::*,
    },
    Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
};
use windows_numerics::Vector2;
use windows_sys::Win32::{Graphics::Gdi::HDC, UI::Input::KeyboardAndMouse::{DragDetect, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent}};

pub const CLASS: &str = "RxGList";
const WM_FRAME: u32 = WM_APP + 30; // from the pacing thread: draw the next frame
const HEAD: i32 = 32; // the column titles

// ---------- frame pacing ----------
// A thread waits for the compositor (DwmFlush returns once per screen refresh) and asks for a frame while an animation
// runs. With nothing moving it sleeps on an event: no frames, no CPU.

static WND: AtomicIsize = AtomicIsize::new(0);
static ANIM: AtomicBool = AtomicBool::new(false);
static PENDING: AtomicBool = AtomicBool::new(false);
static WAKE: AtomicIsize = AtomicIsize::new(0);

fn pacer() {
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    let ev = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) } as isize;
    WAKE.store(ev, Relaxed);
    std::thread::spawn(move || loop {
        unsafe {
            WaitForSingleObject(ev as HANDLE, INFINITE);
            windows_sys::Win32::Graphics::Dwm::DwmFlush();
            if ANIM.load(Relaxed) && !PENDING.swap(true, Relaxed) { PostMessageW(WND.load(Relaxed) as HWND, WM_FRAME, 0, 0); }
        }
    });
}

fn animate(on: bool) {
    use windows_sys::Win32::System::Threading::{ResetEvent, SetEvent};
    if ANIM.swap(on, Relaxed) == on { return; }
    let ev = WAKE.load(Relaxed) as HANDLE;
    unsafe { if on { SetEvent(ev); } else { ResetEvent(ev); } }
}

// ---------- frame times (RX_FPS=1) ----------

thread_local! { static FPS: RefCell<(Option<Instant>, Vec<String>)> = const { RefCell::new((None, Vec::new())) }; }

fn fps_on() -> bool { static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new(); *ON.get_or_init(|| env::var_os("RX_FPS").is_some()) }

/// One frame of `who` ("old" / "new") with the list at `px`: a line "who ms px" for the log.
pub fn fps_mark(who: &str, px: f32) {
    if !fps_on() { return; }
    FPS.with_borrow_mut(|(t0, v)| {
        let t = t0.get_or_insert_with(Instant::now).elapsed().as_secs_f64() * 1000.0;
        v.push(format!("{who} {t:.2} {px:.1}"));
    });
}

/// Writes what was noted (at the end of a glide).
pub fn fps_flush() {
    if !fps_on() { return; }
    use std::io::Write;
    let lines = FPS.with_borrow_mut(|(_, v)| std::mem::take(v));
    if lines.is_empty() { return; }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(env::temp_dir().join("rx-fps.log")) { let _ = writeln!(f, "{}", lines.join("\n")); }
}

// ---------- the GPU side ----------

struct Gpu {
    swap: IDXGISwapChain1,
    dc: ID2D1DeviceContext,
    dw: IDWriteFactory,
    wic: IWICImagingFactory,
    brush: ID2D1SolidColorBrush,
    f_name: IDWriteTextFormat,
    f_det: IDWriteTextFormat,
    f_head: IDWriteTextFormat,
    f_head_on: IDWriteTextFormat,
    f_glyph: IDWriteTextFormat, // the tree's arrows
    dash: ID2D1StrokeStyle,     // the selection box's edge
    size: (u32, u32),
    icons: HashMap<(i32, i32), Option<ID2D1Bitmap1>>,
    texts: HashMap<(usize, u8), (i32, String, IDWriteTextLayout)>, // (row, column) -> (width, text, its layout)
}

thread_local! { static GPU: RefCell<Option<Gpu>> = const { RefCell::new(None) }; }

fn color(c: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: (c & 255) as f32 / 255.0, g: (c >> 8 & 255) as f32 / 255.0, b: (c >> 16 & 255) as f32 / 255.0, a: 1.0 }
}

fn rf(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F { D2D_RECT_F { left: l, top: t, right: r, bottom: b } }

unsafe fn text_format(dw: &IDWriteFactory, px: i32, weight: DWRITE_FONT_WEIGHT) -> windows::core::Result<IDWriteTextFormat> {
    let face = w(ui::ui_face());
    let f = dw.CreateTextFormat(PCWSTR(face.as_ptr()), None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, sc(px) as f32, windows::core::w!("en-us"))?;
    f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
    f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
    let sign = dw.CreateEllipsisTrimmingSign(&f)?;
    f.SetTrimming(&trim, &sign)?;
    Ok(f)
}

unsafe fn gpu_new(h: HWND) -> windows::core::Result<Gpu> {
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let (w0, h0) = ((cr.right as u32).max(1), (cr.bottom as u32).max(1));
    let mut dev: Option<ID3D11Device> = None;
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    if D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, Default::default(), flags, None, D3D11_SDK_VERSION, Some(&mut dev), None, None).is_err() {
        D3D11CreateDevice(None, D3D_DRIVER_TYPE_WARP, Default::default(), flags, None, D3D11_SDK_VERSION, Some(&mut dev), None, None)?;
    }
    let dev = dev.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;
    let dxgi: IDXGIDevice = dev.cast()?;
    let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: w0,
        Height: h0,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_NONE,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        ..Default::default()
    };
    let swap = factory.CreateSwapChainForHwnd(&dev, windows::Win32::Foundation::HWND(h as _), &desc, None, None)?;
    let d2f: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
    let dc = d2f.CreateDevice(&dxgi)?.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
    dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE); // opaque target: ClearType like the rest of the window
    let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
    let brush = dc.CreateSolidColorBrush(&color(pal().text), None)?;
    let g = Gpu {
        f_name: text_format(&dw, 13, DWRITE_FONT_WEIGHT_NORMAL)?,
        f_det: text_format(&dw, 12, DWRITE_FONT_WEIGHT_NORMAL)?,
        f_head: text_format(&dw, 13, DWRITE_FONT_WEIGHT_NORMAL)?,
        f_head_on: text_format(&dw, 13, DWRITE_FONT_WEIGHT_MEDIUM)?,
        f_glyph: {
            let face = w(winrust_common::style::icon_face());
            let f = dw.CreateTextFormat(PCWSTR(face.as_ptr()), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, sc(11) as f32, windows::core::w!("en-us"))?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            f
        },
        dash: d2f.CreateStrokeStyle(&D2D1_STROKE_STYLE_PROPERTIES1 { dashStyle: D2D1_DASH_STYLE_DASH, miterLimit: 10.0, ..Default::default() }, None)?.cast()?,
        swap, dc, dw, wic, brush, size: (w0, h0), icons: HashMap::new(), texts: HashMap::new(),
    };
    target(&g)?;
    Ok(g)
}

/// Points the drawing at the swap chain's back buffer (again after a resize).
unsafe fn target(g: &Gpu) -> windows::core::Result<()> {
    let surf: IDXGISurface = g.swap.GetBuffer(0)?;
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
        ..Default::default()
    };
    let bmp = g.dc.CreateBitmapFromDxgiSurface(&surf, Some(&props))?;
    g.dc.SetTarget(&bmp);
    Ok(())
}

unsafe fn resize(g: &mut Gpu, w0: u32, h0: u32) -> windows::core::Result<()> {
    if g.size == (w0, h0) { return Ok(()); }
    g.dc.SetTarget(None);
    g.swap.ResizeBuffers(0, w0.max(1), h0.max(1), DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
    g.size = (w0, h0);
    g.texts.clear();
    target(g)
}

/// A system icon as a GPU bitmap (made once per icon and size).
unsafe fn icon(g: &mut Gpu, idx: i32, s: i32) -> Option<ID2D1Bitmap1> {
    if idx < 0 { return None; }
    if let Some(b) = g.icons.get(&(idx, s)) { return b.clone(); }
    let lists = ui::sys_lists();
    let made = (|| -> Option<ID2D1Bitmap1> {
        let (il, _) = lists.iter().copied().find(|&(il, sz)| il != 0 && sz >= s).or_else(|| lists.iter().copied().rev().find(|&(il, _)| il != 0))?;
        let hi = windows_sys::Win32::UI::Controls::ImageList_GetIcon(il as _, idx, windows_sys::Win32::UI::Controls::ILD_TRANSPARENT);
        if hi.is_null() { return None; }
        let r = (|| -> windows::core::Result<ID2D1Bitmap1> {
            let src = g.wic.CreateBitmapFromHICON(windows::Win32::UI::WindowsAndMessaging::HICON(hi as _))?;
            let conv = g.wic.CreateFormatConverter()?;
            conv.Initialize(&src, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
            g.dc.CreateBitmapFromWicBitmap(&conv, None)
        })();
        DestroyIcon(hi);
        r.ok()
    })();
    g.icons.insert((idx, s), made.clone());
    made
}

// ---------- the list's own state ----------
// What is selected and focused lives in the system list (hidden behind this one): everything that acts on the
// selection (Delete, copy, the menus, Ctrl+A, the status line, the came-from selection) keeps working through it, and
// keys are handed to it. This list draws, scrolls and handles the mouse.

enum Drag {
    None,
    Bar(i32, f32), // the scrollbar thumb: (pointer y, top) at the press
    Col(usize, i32, i32, i32), // a column edge: (edge, pointer x at the press, width of the left column, of the next)
    Box(SelBox),
}

struct SelBox {
    x0: i32, cy0: i32,             // where it started: client x, y in the content (independent of the scroll)
    x: i32, y: i32,                // the pointer now
    active: bool,                  // moved far enough: a box, not a click
    keys: usize,                   // MK_CONTROL / MK_SHIFT at the press
    hit: Option<usize>,            // the row pressed on (beside its name)
    base: Vec<usize>,              // selected before, kept with Ctrl / Shift (sorted)
    range: Option<(usize, usize)>, // the rows it touches now
}

struct View {
    top: f32,              // scrolled this far (px), what is on screen
    goal: f32,             // where the glide is going
    last: Option<Instant>, // the frame before
    hover: Option<usize>,
    drag: Drag,
    dir: PathBuf,
    edit: isize,           // the rename box (a child of the main window), 0 when none
    edit_row: usize,
}

thread_local! { static V: RefCell<View> = RefCell::new(View { top: 0.0, goal: 0.0, last: None, hover: None, drag: Drag::None, dir: PathBuf::new(), edit: 0, edit_row: 0 }); }

pub fn hwnd() -> HWND { WND.load(Relaxed) as HWND }

/// Is this list on screen now?
pub fn active() -> bool { let h = hwnd(); !h.is_null() && unsafe { IsWindowVisible(h) } != 0 }

/// Is the smooth list the one to show (the setting, or RX_NEWLIST=1), for the view shown (Details or Tree)?
pub fn wanted() -> bool {
    !desk() && (env::var_os("RX_NEWLIST").is_some() || prefs_now().smooth_list) && ST.with(|c| c.try_borrow().map_or(false, |s| matches!(s.view, View_::Details | View_::Tree)))
}
use super::View as View_;

fn row_h() -> i32 { ui::row_px() }
fn head_h() -> i32 { sc(HEAD) }
fn lv() -> HWND { ST.with(|c| c.try_borrow().map_or(ptr::null_mut(), |s| s.list)) }
fn rows_n() -> usize { ST.with(|c| c.try_borrow().map_or(0, |s| s.rows.len())) }
fn client() -> RECT { let mut cr: RECT = unsafe { std::mem::zeroed() }; unsafe { GetClientRect(hwnd(), &mut cr) }; cr }

fn is_sel(i: usize) -> bool { put(lv(), LVM_GETITEMSTATE, i, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0 }
fn focused() -> Option<usize> { usize::try_from(put(lv(), LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize)).ok().filter(|&i| i < rows_n()) }
fn set_state(i: usize, mask: u32, state: u32) {
    let it = LVITEMW { stateMask: mask, state, ..unsafe { std::mem::zeroed() } };
    put(lv(), LVM_SETITEMSTATE, i, &it as *const _ as isize);
}
fn clear_sel() { set_state(usize::MAX, LVIS_SELECTED, 0); }
fn select_only(i: usize) {
    clear_sel();
    set_state(i, LVIS_SELECTED | LVIS_FOCUSED, LVIS_SELECTED | LVIS_FOCUSED);
    put(lv(), LVM_SETSELECTIONMARK, 0, i as isize);
}
fn selected_rows() -> Vec<usize> {
    let (mut v, mut i, n) = (vec![], -1isize, rows_n());
    loop {
        i = put(lv(), LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
        if i < 0 || i as usize >= n { break; }
        v.push(i as usize);
    }
    v
}

/// The rows changed (a folder listed, a filter, a sort). A new folder starts at the top, or where its selected row is
/// (the folder you came out of).
pub fn reset() {
    let dir = ST.with(|c| c.try_borrow().map(|s| s.dir.clone()).unwrap_or_default());
    let new_dir = V.with_borrow_mut(|v| { let n = v.dir != dir; if n { v.top = 0.0; v.goal = 0.0; v.dir = dir; } n });
    GPU.with_borrow_mut(|g| if let Some(g) = g { g.texts.clear() });
    clamp();
    if new_dir { if let Some(f) = focused() { show_row_now(f); } }
    invalidate();
}

/// Highest scroll: the rows plus two empty ones at the end (room to right-click the folder).
fn max_top() -> f32 { (((rows_n() + PAD_ROWS) as i32 * row_h()) - (client().bottom - head_h())).max(0) as f32 }

fn clamp() {
    let m = max_top();
    V.with_borrow_mut(|v| { v.goal = v.goal.clamp(0.0, m); v.top = v.top.clamp(0.0, m); });
}

/// Draws once more (no animation running: right away, through the message queue).
pub fn invalidate() {
    let h = hwnd();
    if !h.is_null() && !PENDING.swap(true, Relaxed) { unsafe { PostMessageW(h, WM_FRAME, 0, 0) }; }
}

/// Glide toward `goal`.
fn scroll_to(goal: f32) {
    V.with_borrow_mut(|v| { v.goal = goal.clamp(0.0, max_top()); if v.last.is_none() { v.last = Some(Instant::now()); } });
    animate(true);
    invalidate();
}

/// One step of the glide; true while still moving.
fn step() -> bool {
    V.with_borrow_mut(|v| {
        let now = Instant::now();
        let dt = v.last.map_or(1.0 / 120.0, |l| (now - l).as_secs_f32()).min(0.05);
        v.last = Some(now);
        let d = v.goal - v.top;
        if d.abs() < 0.5 { v.top = v.goal; v.last = None; return false; }
        v.top += d * (1.0 - (-dt * 18.0).exp()); // eased: quick at first, settling softly
        true
    })
}

/// Row `i` into view with a glide (keys) or at once (a folder just opened).
fn ensure_visible(i: usize) {
    let (rh, view) = (row_h() as f32, (client().bottom - head_h()) as f32);
    let y = i as f32 * rh;
    let goal = V.with_borrow(|v| v.goal);
    if y < goal { scroll_to(y); } else if y + rh > goal + view { scroll_to(y + rh - view); }
}

fn show_row_now(i: usize) {
    let (rh, view) = (row_h() as f32, (client().bottom - head_h()) as f32);
    let y = i as f32 * rh;
    let top = V.with_borrow(|v| v.top);
    if y >= top && y + rh <= top + view { return; }
    let t = (y - view / 3.0).clamp(0.0, max_top());
    V.with_borrow_mut(|v| { v.top = t; v.goal = t; });
}

fn row_at(y: i32) -> Option<usize> {
    if y < head_h() { return None; }
    let top = V.with_borrow(|v| v.top.round() as i32);
    let i = (y - head_h() + top).div_euclid(row_h());
    (i >= 0 && (i as usize) < rows_n()).then_some(i as usize)
}

/// The folder row under screen point (x, y), for a drop: (row, its path).
pub fn folder_at(x: i32, y: i32) -> Option<(usize, PathBuf)> {
    let mut p = POINT { x, y };
    unsafe { windows_sys::Win32::Graphics::Gdi::ScreenToClient(hwnd(), &mut p) };
    let i = row_at(p.y)?;
    ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).filter(|r| r.dir).map(|r| (i, s.dir.join(&r.name)))))
}

/// Column edges: the system list's header widths (it fits them to the width), from x = 0.
fn columns() -> Vec<i32> {
    let mut x = vec![0];
    for k in 0..ui::COLS { let wd = put(lv(), LVM_GETCOLUMNWIDTH, k, 0) as i32; x.push(x[k] + wd); }
    x
}

/// Where the name starts in row `i` (after the tree's indent and arrow, and the icon), and the row's depth.
fn name_left(i: usize) -> (i32, i32) {
    let (depth, tree) = ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).map(|r| (r.depth as i32, s.view == View_::Tree)))).unwrap_or((0, false));
    let indent = if tree { sc(TREE_STEP) * depth + sc(TREE_ARROW) } else { 0 };
    (sc(16) + indent + sc(16) + sc(12), indent)
}
const TREE_STEP: i32 = 18;
const TREE_ARROW: i32 = 16;

/// The tree's arrow of row `i`, if (x) is on it.
fn on_arrow(i: usize, x: i32) -> bool {
    let (depth, dir, tree) = ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).map(|r| (r.depth as i32, r.dir, s.view == View_::Tree)))).unwrap_or((0, false, false));
    let ax = sc(10) + sc(TREE_STEP) * depth;
    tree && dir && x >= ax - sc(4) && x < ax + sc(TREE_ARROW) + sc(4)
}

/// Is (x) on row `i`'s icon or name? (A press there picks the item and can drag it; anywhere else starts a box.)
fn on_name(g: &Gpu, i: usize, x: i32) -> bool {
    let Some(name) = ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).map(|r| r.name.rsplit('\\').next().unwrap_or("").to_string()))) else { return false };
    let (l, indent) = name_left(i);
    let s16: Vec<u16> = name.encode_utf16().collect();
    let w = unsafe {
        g.dw.CreateTextLayout(&s16, &g.f_name, 10000.0, row_h() as f32).ok().and_then(|lay| { let mut m = DWRITE_TEXT_METRICS::default(); lay.GetMetrics(&mut m).ok().map(|_| m.widthIncludingTrailingWhitespace as i32) }).unwrap_or(0)
    };
    let x1 = columns()[1];
    x >= sc(10) + indent && x < (l + w + sc(8)).min(x1)
}

// ---------- drawing ----------

unsafe fn render() {
    let h = hwnd();
    if h.is_null() || IsWindowVisible(h) == 0 { return; }
    let cr = client();
    if GPU.with_borrow(|g| g.is_none()) {
        match gpu_new(h) { Ok(g) => GPU.set(Some(g)), Err(_) => return }
    }
    let ok = GPU.with_borrow_mut(|g| {
        let g = g.as_mut().unwrap();
        if resize(g, cr.right as u32, cr.bottom as u32).is_err() { return false; }
        draw(g, cr.right, cr.bottom);
        g.swap.Present(0, DXGI_PRESENT(0)).is_ok()
    });
    if !ok { GPU.set(None); } // the device went away (driver update, GPU reset): made again on the next frame
    fps_mark("new", V.with_borrow(|v| v.top));
}

unsafe fn draw(g: &mut Gpu, cw: i32, ch: i32) {
    let dc = g.dc.clone();
    dc.BeginDraw();
    let bg = ui::listbg();
    dc.Clear(Some(&color(bg)));
    let top = V.with_borrow(|v| v.top.round() as i32);
    let (rh, hh) = (row_h(), head_h());
    let x = columns();
    let focus = GetFocus() == hwnd() || V.with_borrow(|v| v.edit != 0);
    let tree = ST.with(|c| c.try_borrow().map_or(false, |s| s.view == View_::Tree));
    let rows: Vec<(usize, Row)> = ST.with(|c| {
        let Ok(mut st) = c.try_borrow_mut() else { return vec![] };
        let first = (top / rh).max(0) as usize;
        let count = ((ch - hh) / rh + 2) as usize;
        let dir = st.dir.clone();
        let n = st.rows.len();
        (first..(first + count).min(n)).map(|i| {
            let r = &mut st.rows[i];
            if r.icon < 0 { r.icon = icon_of(&dir.join(&r.name), r.dir); }
            (i, r.clone())
        }).collect()
    });
    let dir = ST.with(|c| c.try_borrow().map(|s| s.dir.clone()).unwrap_or_default());
    let (hover, drop) = (V.with_borrow(|v| v.hover), ui::drop_row());
    dc.PushAxisAlignedClip(&rf(0.0, hh as f32, cw as f32, ch as f32), D2D1_ANTIALIAS_MODE_ALIASED);
    let brush = g.brush.clone();
    let paint = |c: u32| brush.SetColor(&color(c));
    let right = (*x.last().unwrap()).min(cw);
    for (i, r) in &rows {
        let y = hh + *i as i32 * rh - top;
        let on = is_sel(*i);
        let hot = hover == Some(*i);
        let fill = match (on, focus) {
            _ if drop == *i as isize => mix(bg, pal().accent, 0.32), // where a drag would go
            (true, true) => mix(bg, pal().accent, if hot { 0.26 } else { 0.20 }),
            (true, false) => mix(bg, pal().text, 0.10),
            _ if hot => mix(bg, pal().text, 0.055),
            _ if i % 2 == 1 => mix(bg, pal().text, 0.035), // every other row a shade lighter
            _ => bg,
        };
        if fill != bg {
            paint(fill);
            let rr = D2D1_ROUNDED_RECT { rect: rf(sc(6) as f32, (y + 1) as f32, (right - sc(6)) as f32, (y + rh - 1) as f32), radiusX: sc(5) as f32, radiusY: sc(5) as f32 };
            dc.FillRoundedRectangle(&rr, &brush);
        }
        let (nl, indent) = name_left(*i);
        if tree && r.dir {
            // the tree's arrow: right when closed, down when open
            let ax = sc(10) + sc(TREE_STEP) * r.depth as i32;
            let t: Vec<u16> = (if r.open { "\u{E70D}" } else { "\u{E76C}" }).encode_utf16().collect();
            if let Ok(lay) = g.dw.CreateTextLayout(&t, &g.f_glyph, sc(TREE_ARROW) as f32, rh as f32) {
                paint(if r.open { pal().text } else { pal().dim });
                dc.DrawTextLayout(Vector2 { X: ax as f32, Y: y as f32 }, &lay, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
        }
        let s = sc(16);
        if let Some(b) = icon(g, r.icon, s) {
            let ix = x[0] + sc(16) + indent;
            dc.DrawBitmap(&b, Some(&rf(ix as f32, (y + (rh - s) / 2) as f32, (ix + s) as f32, (y + (rh + s) / 2) as f32)), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, None, None);
        }
        let shown = r.name.rsplit('\\').next().unwrap_or(&r.name).to_string();
        let dim = if on { mix(pal().dim, pal().text, 0.45) } else { pal().dim };
        let mut cell = |col: u8, t: &str, l: i32, rgt: i32, c: u32, name: bool| {
            let wd = (rgt - l).max(1);
            // laid out once and kept while the text and the width stay (counts change while folders are counted)
            let lay = match g.texts.get(&(*i, col)) {
                Some((w0, t0, l)) if *w0 == wd && t0 == t => l.clone(),
                _ => {
                    let s16: Vec<u16> = t.encode_utf16().collect();
                    let Ok(l) = g.dw.CreateTextLayout(&s16, if name { &g.f_name } else { &g.f_det }, wd as f32, rh as f32) else { return };
                    if g.texts.len() > 4000 { g.texts.clear(); }
                    g.texts.insert((*i, col), (wd, t.to_string(), l.clone()));
                    l
                }
            };
            paint(c);
            dc.DrawTextLayout(Vector2 { X: l as f32, Y: y as f32 }, &lay, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        };
        let editing = V.with_borrow(|v| v.edit != 0 && v.edit_row == *i);
        if !editing { cell(0, &shown, x[0] + nl, x[1] - sc(8), pal().text, true); }
        cell(1, &ui::friendly_date(r.time), x[1] + sc(10), x[2] - sc(8), dim, false);
        let kind = if Path::new(&r.name).is_absolute() {
            // a search hit: where it lies, below the shown folder or in full
            let p = Path::new(&r.name);
            let parent = p.parent().unwrap_or(p);
            match parent.strip_prefix(&dir) { Ok(rel) if rel.as_os_str().is_empty() => "(this folder)".into(), Ok(rel) => rel.to_string_lossy().into_owned(), Err(_) => parent.to_string_lossy().into_owned() }
        } else { type_text(&r.name, r.dir) };
        cell(2, &kind, x[2] + sc(10), x[3] - sc(8), dim, false);
        if r.dir {
            if let Some(s) = PREFS.with_borrow(|p| p.folder_sizes).then(|| sizes::get(&dir.join(&r.name))).flatten() {
                let c = if s.done { dim } else { mix(dim, bg, 0.45) };
                let n = |k: u32| if k == 0 { "\u{2013}".to_string() } else { thousands(k as usize) };
                cell(3, &n(s.dirs), x[3] + sc(10), x[4] - sc(8), c, false);
                cell(4, &n(s.files), x[4] + sc(10), x[5] - sc(8), c, false);
                cell(5, &size_text(s.bytes), x[5] + sc(10), x[6] - sc(8), c, false);
            }
        } else {
            let off = mix(dim, bg, 0.5);
            cell(3, "\u{2013}", x[3] + sc(10), x[4] - sc(8), off, false);
            cell(4, "\u{2013}", x[4] + sc(10), x[5] - sc(8), off, false);
            cell(5, &size_text(r.size), x[5] + sc(10), x[6] - sc(8), dim, false);
        }
    }
    if rows_n() == 0 {
        // an empty folder (or nothing found) says so
        let msg = if ui::searching() || filtering() { "Nothing found" } else { "This folder is empty" };
        let t: Vec<u16> = msg.encode_utf16().collect();
        if let Ok(lay) = g.dw.CreateTextLayout(&t, &g.f_name, cw as f32, sc(40) as f32) {
            let _ = lay.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
            paint(pal().dim);
            dc.DrawTextLayout(Vector2 { X: 0.0, Y: (ch / 3) as f32 }, &lay, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
    }
    // the selection box
    if let Some((x0, y0, x1, y1)) = V.with_borrow(|v| match &v.drag { Drag::Box(b) if b.active => Some((b.x0, b.cy0 - v.top.round() as i32 + hh, b.x, b.y)), _ => None }) {
        let r = rf(x0.min(x1).max(0) as f32 + 0.5, y0.min(y1).max(hh) as f32 + 0.5, x0.max(x1).min(cw) as f32 - 0.5, y0.max(y1).min(ch) as f32 - 0.5);
        paint(pal().accent);
        brush.SetOpacity(0.22);
        dc.FillRectangle(&r, &brush);
        brush.SetOpacity(0.9);
        dc.DrawRectangle(&r, &brush, sc(1).max(1) as f32, &g.dash);
        brush.SetOpacity(1.0);
    }
    dc.PopAxisAlignedClip();
    // the column titles
    paint(pal().bg);
    dc.FillRectangle(&rf(0.0, 0.0, cw as f32, hh as f32), &brush);
    let sorted = SORT_COL.load(Relaxed);
    let arrow = if SORT_ASC.load(Relaxed) { "  \u{2191}" } else { "  \u{2193}" };
    for (k, title) in ["Name", "Date modified", "Type", "Folders", "Files", "Size"].iter().enumerate() {
        let on = k == sorted;
        let t: Vec<u16> = format!("{title}{}", if on { arrow } else { "" }).encode_utf16().collect();
        let l = x[k] + sc(10);
        let wd = (x[k + 1] - sc(12) - l).max(1) as f32;
        if let Ok(lay) = g.dw.CreateTextLayout(&t, if on { &g.f_head_on } else { &g.f_head }, wd, hh as f32) {
            paint(if on { pal().text } else { pal().dim });
            dc.DrawTextLayout(Vector2 { X: l as f32, Y: 0.0 }, &lay, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
        paint(pal().line);
        dc.FillRectangle(&rf((x[k + 1] - 1) as f32, sc(8) as f32, x[k + 1] as f32, (hh - sc(8)) as f32), &brush);
    }
    // a thin scrollbar on the right while there is more than fits
    let m = max_top();
    if m > 0.0 {
        let (t0, len) = thumb(ch, top as f32, m);
        paint(mix(bg, pal().text, 0.28));
        let rr = D2D1_ROUNDED_RECT { rect: rf((cw - sc(7)) as f32, t0, (cw - sc(3)) as f32, t0 + len), radiusX: sc(2) as f32, radiusY: sc(2) as f32 };
        dc.FillRoundedRectangle(&rr, &brush);
    }
    let _ = dc.EndDraw(None, None);
}

/// The scrollbar thumb: (top, length) for a client height and scroll.
fn thumb(ch: i32, top: f32, m: f32) -> (f32, f32) {
    let view = (ch - head_h()) as f32;
    let track = view - sc(8) as f32;
    let len = (track * view / (m + view)).max(sc(24) as f32);
    (head_h() as f32 + sc(4) as f32 + (track - len) * (top / m.max(1.0)), len)
}

// ---------- the selection box ----------

/// Selects the rows the box touches (plus what was selected before, with Ctrl / Shift); only rows that change are told.
fn box_select() {
    let n = rows_n();
    let rh = row_h();
    let top = V.with_borrow(|v| v.top.round() as i32);
    let Some((old, new, base)) = V.with_borrow_mut(|v| {
        let Drag::Box(b) = &mut v.drag else { return None };
        let cy = b.y - head_h() + top;
        let (a, z) = (b.cy0.min(cy).div_euclid(rh), b.cy0.max(cy).div_euclid(rh));
        let new = (n > 0 && z >= 0 && (a as usize) < n).then(|| (a.max(0) as usize, (z as usize).min(n - 1)));
        let old = std::mem::replace(&mut b.range, new);
        Some((old, new, b.base.clone()))
    }) else { return };
    let inside = |r: Option<(usize, usize)>, i: usize| r.is_some_and(|(a, b)| i >= a && i <= b);
    let lo = old.map_or(usize::MAX, |r| r.0).min(new.map_or(usize::MAX, |r| r.0));
    let hi = old.map_or(0, |r| r.1).max(new.map_or(0, |r| r.1));
    if lo > hi { return; }
    for i in lo..=hi {
        let (was, now) = (inside(old, i), inside(new, i));
        if was != now { set_state(i, LVIS_SELECTED, if now || base.binary_search(&i).is_ok() { LVIS_SELECTED } else { 0 }); }
    }
}

// ---------- renaming ----------

thread_local! { static EDIT_FONT: Cell<isize> = const { Cell::new(0) }; }
const WM_EDIT_DONE: u32 = WM_APP + 31; // the rename box is done: wp = 1 keep the new name, 0 drop it

/// Opens row `i`'s name for typing in a box over it (F2, the menu's Rename, a new folder); for a file only the part
/// before the extension is selected, as in Explorer.
pub unsafe fn begin_rename(i: usize) {
    end_rename(false);
    let Some((name, dir)) = ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).map(|r| (r.name.rsplit('\\').next().unwrap_or("").to_string(), r.dir)))) else { return };
    show_row_now(i);
    let main = GetAncestor(hwnd(), GA_ROOT);
    let mut wr: RECT = std::mem::zeroed();
    GetWindowRect(hwnd(), &mut wr);
    let mut p = POINT { x: wr.left, y: wr.top };
    windows_sys::Win32::Graphics::Gdi::ScreenToClient(main, &mut p);
    let top = V.with_borrow(|v| v.top.round() as i32);
    let (nl, _) = name_left(i);
    let x = columns();
    let (rh, eh) = (row_h(), sc(24).min(row_h()));
    let y = p.y + head_h() + i as i32 * rh - top + (rh - eh) / 2;
    let (l, r) = (p.x + x[0] + nl - sc(4), p.x + x[1] - sc(8));
    let e = CreateWindowExW(0, w("EDIT").as_ptr(), w(&name).as_ptr(), WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32, l, y, (r - l).max(sc(80)), eh, main, ptr::null_mut(), GetModuleHandleW(ptr::null()), ptr::null());
    if EDIT_FONT.get() == 0 { EDIT_FONT.set(ui::font(13, 400, ui::ui_face())); }
    SendMessageW(e, WM_SETFONT, EDIT_FONT.get() as usize, 0);
    SendMessageW(e, EM_SETMARGINS, 3, (sc(3) | sc(3) << 16) as isize);
    SetWindowPos(e, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    windows_sys::Win32::UI::Shell::SetWindowSubclass(e, Some(edit_sub), 1, 0);
    let stem = if dir { name.encode_utf16().count() } else { name.rfind('.').filter(|&d| d > 0).map_or(name.encode_utf16().count(), |d| name[..d].encode_utf16().count()) };
    SendMessageW(e, EM_SETSEL, 0, stem as isize);
    SetFocus(e);
    V.with_borrow_mut(|v| { v.edit = e as isize; v.edit_row = i; });
    invalidate();
}

/// Closes the rename box; `keep` renames the item (through the same code as the system list's rename).
unsafe fn end_rename(keep: bool) {
    let (e, i) = V.with_borrow_mut(|v| (std::mem::replace(&mut v.edit, 0) as HWND, v.edit_row));
    if e.is_null() { return; }
    let mut buf = [0u16; 512];
    let n = GetWindowTextW(e, buf.as_mut_ptr(), 512).max(0) as usize;
    let new = String::from_utf16_lossy(&buf[..n]);
    let refocus = GetFocus() == e;
    DestroyWindow(e);
    if refocus { SetFocus(hwnd()); }
    if keep { rename_row(GetAncestor(hwnd(), GA_ROOT), i, &new); }
    invalidate();
}

unsafe extern "system" fn edit_sub(h: HWND, m: u32, wp: WPARAM, lp: LPARAM, _id: usize, _d: usize) -> LRESULT {
    match m {
        WM_KEYDOWN if wp == 0x0D => { PostMessageW(hwnd(), WM_EDIT_DONE, 1, 0); return 0; }
        WM_KEYDOWN if wp == 0x1B => { PostMessageW(hwnd(), WM_EDIT_DONE, 0, 0); return 0; }
        WM_CHAR if wp == 0x0D || wp == 0x1B => return 0, // no beep
        WM_KILLFOCUS => { PostMessageW(hwnd(), WM_EDIT_DONE, 1, 0); }
        _ => {}
    }
    windows_sys::Win32::UI::Shell::DefSubclassProc(h, m, wp, lp)
}

/// The rename box's colours (it is a child of the main window, which asks): the list's text on a lighter shade.
pub unsafe fn edit_colors(dc: HDC, e: HWND) -> Option<LRESULT> {
    if V.with_borrow(|v| v.edit) != e as isize { return None; }
    use windows_sys::Win32::Graphics::Gdi::{CreateSolidBrush, SetBkColor, SetTextColor};
    thread_local! { static B: Cell<isize> = const { Cell::new(0) }; }
    let c = mix(ui::listbg(), pal().text, 0.10);
    SetTextColor(dc, pal().text);
    SetBkColor(dc, c);
    if B.get() == 0 { B.set(CreateSolidBrush(c) as isize); }
    Some(B.get())
}

// ---------- input ----------

pub unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
    let main = GetAncestor(h, GA_ROOT);
    match m {
        WM_FRAME => {
            PENDING.store(false, Relaxed);
            let moving = step();
            if moving && V.with_borrow(|v| matches!(&v.drag, Drag::Box(b) if b.active)) { box_select(); }
            render();
            if !moving { animate(false); fps_flush(); }
            0
        }
        WM_EDIT_DONE => { end_rename(wp == 1); 0 }
        WM_PAINT => {
            let mut ps: windows_sys::Win32::Graphics::Gdi::PAINTSTRUCT = std::mem::zeroed();
            windows_sys::Win32::Graphics::Gdi::BeginPaint(h, &mut ps);
            windows_sys::Win32::Graphics::Gdi::EndPaint(h, &ps);
            render();
            0
        }
        WM_ERASEBKGND => 1,
        WM_SIZE => { clamp(); render(); 0 }
        WM_MOUSEWHEEL => {
            let d = ((wp >> 16) & 0xFFFF) as i16 as i32;
            if wp & 0x0008 != 0 { PostMessageW(main, ui::WM_UI, 25, d.signum() as isize); return 0; } // Ctrl+wheel: zoom
            end_rename(true);
            let mut lines = 3u32;
            SystemParametersInfoW(SPI_GETWHEELSCROLLLINES, 0, &mut lines as *mut _ as *mut _, 0);
            // every notch (and every small touchpad turn) moves the goal by its share of rows, to the pixel
            let goal = V.with_borrow(|v| v.goal) - d as f32 * lines.min(10) as f32 * row_h() as f32 / 120.0;
            scroll_to(goal);
            0
        }
        WM_SETCURSOR => {
            let mut p = POINT { x: 0, y: 0 };
            GetCursorPos(&mut p);
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(h, &mut p);
            if p.y < head_h() && edge_at(p.x).is_some() { SetCursor(LoadCursorW(ptr::null_mut(), IDC_SIZEWE)); return 1; }
            DefWindowProcW(h, m, wp, lp)
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            SetFocus(h);
            let cr = client();
            if m == WM_LBUTTONDOWN && x >= cr.right - sc(10) && y >= head_h() && max_top() > 0.0 {
                SetCapture(h); // the scrollbar, held
                V.with_borrow_mut(|v| v.drag = Drag::Bar(y, v.top));
                return 0;
            }
            if y < head_h() {
                if let Some(k) = edge_at(x) {
                    SetCapture(h); // a column edge, dragged
                    let x0 = columns();
                    V.with_borrow_mut(|v| v.drag = Drag::Col(k, x, x0[k + 1] - x0[k], x0[k + 2] - x0[k + 1]));
                } else if m == WM_LBUTTONDOWN {
                    if let Some(c) = columns().windows(2).position(|e| x >= e[0] && x < e[1]) { sort_click(main, c); }
                }
                return 0;
            }
            let hit = row_at(y);
            if let Some(i) = hit.filter(|&i| on_arrow(i, x)) { PostMessageW(main, ui::WM_UI, 26, i as isize); return 0; } // the tree's arrow
            if m == WM_LBUTTONDBLCLK { if let Some(i) = hit { open_row(main, i); } return 0; }
            let keys = wp & 0x000C; // MK_SHIFT | MK_CONTROL
            let on = hit.filter(|&i| GPU.with_borrow(|g| g.as_ref().is_some_and(|g| on_name(g, i, x))));
            match on {
                Some(i) => {
                    // on a name: picks it (Ctrl adds or takes away, Shift reaches from the last one), and a drag moves it
                    let was = is_sel(i);
                    if keys & 0x0008 != 0 {
                        set_state(i, LVIS_SELECTED | LVIS_FOCUSED, if was { LVIS_FOCUSED } else { LVIS_SELECTED | LVIS_FOCUSED });
                        put(lv(), LVM_SETSELECTIONMARK, 0, i as isize);
                    } else if keys & 0x0004 != 0 {
                        let mark = usize::try_from(put(lv(), LVM_GETSELECTIONMARK, 0, 0)).unwrap_or(i);
                        clear_sel();
                        for k in mark.min(i)..=mark.max(i) { set_state(k, LVIS_SELECTED, LVIS_SELECTED); }
                        set_state(i, LVIS_FOCUSED, LVIS_FOCUSED);
                    } else if !was {
                        select_only(i);
                    }
                    invalidate();
                    if keys == 0 {
                        let mut sp = POINT { x, y };
                        windows_sys::Win32::Graphics::Gdi::ClientToScreen(h, &mut sp);
                        if DragDetect(h, sp) != 0 {
                            let paths = selected_paths();
                            if !paths.is_empty() { let _ = sh::drag(main as _, &paths); }
                        } else if was {
                            select_only(i); // a plain click on one of several selected: that one alone
                            invalidate();
                        }
                    }
                }
                None => {
                    // anywhere else: a selection box once the pointer moves
                    let base = if keys != 0 { selected_rows() } else { vec![] };
                    let top = V.with_borrow(|v| v.top.round() as i32);
                    V.with_borrow_mut(|v| v.drag = Drag::Box(SelBox { x0: x, cy0: y - head_h() + top, x, y, active: false, keys, hit, base, range: None }));
                    SetCapture(h);
                }
            }
            0
        }
        WM_RBUTTONDOWN => {
            SetFocus(h);
            match row_at(y) {
                Some(i) if !is_sel(i) => { select_only(i); invalidate(); }
                None => { clear_sel(); invalidate(); }
                _ => {}
            }
            0
        }
        WM_RBUTTONUP => {
            let paths = selected_paths();
            if paths.is_empty() { bg_menu(main); } else { menu_choice(main, sh::show(main as _, &paths).unwrap_or(sh::Choice::Done), &paths); }
            0
        }
        WM_MOUSEMOVE => {
            let drag = V.with_borrow(|v| match &v.drag { Drag::Bar(y0, t0) => Some((0, *y0, *t0)), Drag::Col(..) => Some((1, 0, 0.0)), Drag::Box(_) => Some((2, 0, 0.0)), Drag::None => None });
            match drag {
                Some((0, y0, t0)) => {
                    let m = max_top();
                    let (_, len) = thumb(client().bottom, 0.0, m);
                    let track = (client().bottom - head_h() - sc(8)) as f32;
                    let t = (t0 + (y - y0) as f32 * m / (track - len).max(1.0)).clamp(0.0, m);
                    V.with_borrow_mut(|v| { v.top = t; v.goal = t; });
                    invalidate();
                }
                Some((1, ..)) => {
                    let Some((k, x0, w0, w1)) = V.with_borrow(|v| if let Drag::Col(k, x0, w0, w1) = v.drag { Some((k, x0, w0, w1)) } else { None }) else { return 0 };
                    // the Name column takes what is left: moving its edge resizes the next one instead
                    let dx = x - x0;
                    if k == 0 { put(lv(), LVM_SETCOLUMNWIDTH, 1, (w1 - dx).max(sc(50)) as isize); } else { put(lv(), LVM_SETCOLUMNWIDTH, k, (w0 + dx).max(sc(50)) as isize); }
                    ui::fit_columns(lv());
                    invalidate();
                }
                Some(_) => {
                    let began = V.with_borrow_mut(|v| {
                        let top = v.top.round() as i32;
                        let Drag::Box(b) = &mut v.drag else { return (false, 0) };
                        (b.x, b.y) = (x, y);
                        let began = !b.active && ((x - b.x0).abs() > sc(4) || (y - (b.cy0 - top + head_h())).abs() > sc(4));
                        if began { b.active = true; }
                        (began, b.keys)
                    });
                    if began.0 && began.1 == 0 { clear_sel(); } // a new box starts from nothing
                    box_select();
                    // past the top or bottom: the list scrolls along
                    if y < head_h() + sc(20) || y > client().bottom - sc(10) { SetTimer(h, 1, 16, None); } else { KillTimer(h, 1); }
                    invalidate();
                }
                None => {
                    let hv = row_at(y);
                    if V.with_borrow(|v| v.hover) != hv {
                        V.with_borrow_mut(|v| v.hover = hv);
                        let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                        TrackMouseEvent(&mut tme);
                        invalidate();
                    }
                }
            }
            0
        }
        WM_TIMER if wp == 1 => {
            let y = V.with_borrow(|v| if let Drag::Box(b) = &v.drag { b.y } else { 0 });
            let d = if y < head_h() + sc(20) { -1.0 } else if y > client().bottom - sc(10) { 1.0 } else { 0.0 };
            if d != 0.0 { let g = V.with_borrow(|v| v.goal); scroll_to(g + d * row_h() as f32 * 0.6); }
            box_select();
            0
        }
        0x02A3 => { V.with_borrow_mut(|v| v.hover = None); invalidate(); 0 } // WM_MOUSELEAVE
        WM_LBUTTONUP => {
            let drag = V.with_borrow_mut(|v| std::mem::replace(&mut v.drag, Drag::None));
            KillTimer(h, 1);
            ReleaseCapture();
            if let Drag::Box(b) = drag {
                if !b.active {
                    // only a click beside a name: that row, as a click on it would; on empty space nothing selected
                    match b.hit {
                        Some(i) if b.keys & 0x0008 != 0 => { let was = is_sel(i); set_state(i, LVIS_SELECTED | LVIS_FOCUSED, if was { LVIS_FOCUSED } else { LVIS_SELECTED | LVIS_FOCUSED }); }
                        Some(i) if b.keys & 0x0004 != 0 => {
                            let mark = usize::try_from(put(lv(), LVM_GETSELECTIONMARK, 0, 0)).unwrap_or(i);
                            clear_sel();
                            for k in mark.min(i)..=mark.max(i) { set_state(k, LVIS_SELECTED, LVIS_SELECTED); }
                        }
                        Some(i) => select_only(i),
                        None if b.keys == 0 => clear_sel(),
                        None => {}
                    }
                }
            }
            invalidate();
            0
        }
        WM_CAPTURECHANGED => {
            if V.with_borrow(|v| !matches!(v.drag, Drag::None)) && lp as HWND != h { V.with_borrow_mut(|v| v.drag = Drag::None); KillTimer(h, 1); invalidate(); }
            0
        }
        WM_SETFOCUS | WM_KILLFOCUS => { invalidate(); 0 }
        // keys go to the system list behind: it moves the focus and the selection and tells the window (shortcuts,
        // type-to-filter, Enter); this list follows with a glide
        WM_KEYDOWN | WM_KEYUP | WM_CHAR => {
            let before = focused();
            SendMessageW(lv(), m, wp, lp);
            if m == WM_KEYDOWN {
                if let Some(f) = focused() { if Some(f) != before || matches!(wp, 0x21..=0x28) { ensure_visible(f); } }
                invalidate();
            }
            0
        }
        WM_GETDLGCODE => (DLGC_WANTARROWS | DLGC_WANTCHARS) as isize,
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

/// The column edge under (x) in the titles (the one right of column k), within a few pixels.
fn edge_at(x: i32) -> Option<usize> {
    let e = columns();
    (0..ui::COLS - 1).find(|&k| (x - e[k + 1]).abs() <= sc(4))
}

fn selected_paths() -> Vec<PathBuf> {
    let sel = selected_rows();
    ST.with(|c| c.try_borrow().map(|s| sel.iter().filter_map(|&i| s.rows.get(i)).map(|r| s.dir.join(&r.name)).collect()).unwrap_or_default())
}

/// Makes the window (hidden until wanted), the pacing thread, and lets it take drops.
pub unsafe fn create(parent: HWND) {
    let h = CreateWindowExW(0, w(CLASS).as_ptr(), ptr::null(), WS_CHILD | WS_CLIPSIBLINGS, 0, 0, 0, 0, parent, ptr::null_mut(), GetModuleHandleW(ptr::null()), ptr::null());
    WND.store(h as isize, Relaxed);
    pacer();
    let _ = sh::register_drop(h as _, parent as isize);
}
