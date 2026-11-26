//! The file list drawn on the GPU: Direct2D and DirectWrite on a flip-model swap chain, frames paced by the compositor
//! (one per screen refresh while something moves, none when nothing does), scrolling by the pixel with an eased glide,
//! zooming the pictures smoothly. Every view: Details and Tree (rows with columns), List and Icons (a grid).
//! What is selected lives in the system list behind it (see the state section); this list draws, scrolls and handles
//! the mouse. If the GPU cannot be used, the system list shows instead. RX_OLDLIST=1 forces the system list;
//! RX_FPS=1 writes the frame times of both to %TEMP%\rx-fps.log.
//!
//! The same frame clock drives the side pane's and the title bar's animations (`frames`).
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use std::{collections::HashMap, sync::atomic::{AtomicIsize, AtomicU32}, time::Instant};
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
pub const WM_FRAME: u32 = WM_APP + 30; // from the frame clock: draw the next frame (to every window that asked)
const HEAD: i32 = 32; // the column titles

// ---------- the frame clock ----------
// A thread waits for the compositor (DwmFlush returns once per screen refresh) and posts WM_FRAME to each window that
// asked for frames (an animation running). With nothing moving it sleeps on an event: no frames, no CPU.

pub const SLOT_LIST: usize = 0;
pub const SLOT_NAV: usize = 1;
pub const SLOT_CHROME: usize = 2;
static SLOT_WND: [AtomicIsize; 3] = [AtomicIsize::new(0), AtomicIsize::new(0), AtomicIsize::new(0)];
static ON: AtomicU32 = AtomicU32::new(0); // slots that want frames
static PEND: AtomicU32 = AtomicU32::new(0); // slots with a frame posted, not yet handled
static WAKE: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

fn wake_event() -> HANDLE {
    *WAKE.get_or_init(|| {
        use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
        let ev = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) } as isize;
        std::thread::spawn(move || unsafe {
            // the screen's vertical blank (not the compositor: it only wakes when something on screen changes); when
            // that cannot be waited for (no display, a remote session), a sleep of one refresh at the screen's rate
            let output = CreateDXGIFactory1::<IDXGIFactory1>().and_then(|f| f.EnumAdapters1(0)).and_then(|a| a.EnumOutputs(0)).ok();
            let hz = {
                let mut dm: windows_sys::Win32::Graphics::Gdi::DEVMODEW = std::mem::zeroed();
                dm.dmSize = size_of::<windows_sys::Win32::Graphics::Gdi::DEVMODEW>() as u16;
                if windows_sys::Win32::Graphics::Gdi::EnumDisplaySettingsW(ptr::null(), windows_sys::Win32::Graphics::Gdi::ENUM_CURRENT_SETTINGS, &mut dm) != 0 && dm.dmDisplayFrequency > 1 { dm.dmDisplayFrequency } else { 60 }
            };
            let period = std::time::Duration::from_secs_f64(1.0 / hz as f64);
            loop {
                WaitForSingleObject(ev as HANDLE, INFINITE);
                let t = Instant::now();
                let waited = output.as_ref().is_some_and(|o| o.WaitForVBlank().is_ok());
                if !waited || t.elapsed() < std::time::Duration::from_micros(500) {
                    if let Some(rest) = period.checked_sub(t.elapsed()) { std::thread::sleep(rest); }
                }
                let on = ON.load(Relaxed);
                for s in 0..3 {
                    let bit = 1 << s;
                    if on & bit != 0 && PEND.fetch_or(bit, Relaxed) & bit == 0 { PostMessageW(SLOT_WND[s].load(Relaxed) as HWND, WM_FRAME, 0, 0); }
                }
            }
        });
        ev
    }) as HANDLE
}

/// Window `h` (in `slot`) wants a frame at every screen refresh (`on`), or no more.
pub fn frames(slot: usize, h: HWND, on: bool) {
    use windows_sys::Win32::System::Threading::{ResetEvent, SetEvent};
    SLOT_WND[slot].store(h as isize, Relaxed);
    let bit = 1 << slot;
    let before = if on { ON.fetch_or(bit, Relaxed) } else { ON.fetch_and(!bit, Relaxed) };
    let after = if on { before | bit } else { before & !bit };
    if (before == 0) != (after == 0) { unsafe { if after != 0 { SetEvent(wake_event()); } else { ResetEvent(wake_event()); } } }
}

/// The frame posted to `slot` is being handled (the next refresh may post another).
pub fn frame_done(slot: usize) { PEND.fetch_and(!(1 << slot), Relaxed); }

fn animate(on: bool) { frames(SLOT_LIST, hwnd(), on); }

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
    f_label: IDWriteTextFormat, // names under the pictures: up to two lines, centred
    f_glyph: IDWriteTextFormat, // the tree's arrows
    dash: ID2D1StrokeStyle,     // the selection box's edge
    size: (u32, u32),
    icons: HashMap<(i32, i32), Option<ID2D1Bitmap1>>,              // (system icon, image list size)
    pics: HashMap<(PathBuf, i32), ID2D1Bitmap1>,                    // pictures (thumbs.rs), by file and their width
    texts: HashMap<(usize, u8), (i32, String, IDWriteTextLayout)>, // (item, column) -> (width, text, its layout)
    fade: Option<(ID2D1Bitmap1, Instant, f32)>, // the frame before a change of view, fading out: (it, since, +1 bigger / -1 smaller / 0)
    glyphs: HashMap<String, (IDWriteTextLayout, f32)>, // single digits and marks of the rolling counts, with their widths
}

thread_local! { static GPU: RefCell<Option<Gpu>> = const { RefCell::new(None) }; }
static FAILED: AtomicBool = AtomicBool::new(false); // no GPU drawing here: the system list shows

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
    let f_label = text_format(&dw, 12, DWRITE_FONT_WEIGHT_NORMAL)?;
    f_label.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK)?;
    f_label.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
    f_label.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
    let g = Gpu {
        f_name: text_format(&dw, 13, DWRITE_FONT_WEIGHT_NORMAL)?,
        f_det: text_format(&dw, 12, DWRITE_FONT_WEIGHT_NORMAL)?,
        f_head: text_format(&dw, 13, DWRITE_FONT_WEIGHT_NORMAL)?,
        f_head_on: text_format(&dw, 13, DWRITE_FONT_WEIGHT_MEDIUM)?,
        f_label,
        f_glyph: {
            let face = w(winrust_common::style::icon_face());
            let f = dw.CreateTextFormat(PCWSTR(face.as_ptr()), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, sc(11) as f32, windows::core::w!("en-us"))?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            f
        },
        dash: d2f.CreateStrokeStyle(&D2D1_STROKE_STYLE_PROPERTIES1 { dashStyle: D2D1_DASH_STYLE_DASH, miterLimit: 10.0, ..Default::default() }, None)?.cast()?,
        swap, dc, dw, wic, brush, size: (w0, h0), icons: HashMap::new(), pics: HashMap::new(), texts: HashMap::new(), fade: None, glyphs: HashMap::new(),
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
    g.fade = None; // the old frame no longer fits
    target(g)
}

/// A system icon as a GPU bitmap, from the smallest image list at least `s` big (made once per icon and list).
unsafe fn icon(g: &mut Gpu, idx: i32, s: i32) -> Option<ID2D1Bitmap1> {
    if idx < 0 { return None; }
    let lists = ui::sys_lists();
    let (il, size) = lists.iter().copied().find(|&(il, sz)| il != 0 && sz >= s).or_else(|| lists.iter().copied().rev().find(|&(il, _)| il != 0))?;
    if let Some(b) = g.icons.get(&(idx, size)) { return b.clone(); }
    let hi = windows_sys::Win32::UI::Controls::ImageList_GetIcon(il as _, idx, windows_sys::Win32::UI::Controls::ILD_TRANSPARENT);
    let made = if hi.is_null() { None } else {
        let r = (|| -> windows::core::Result<ID2D1Bitmap1> {
            let src = g.wic.CreateBitmapFromHICON(windows::Win32::UI::WindowsAndMessaging::HICON(hi as _))?;
            let conv = g.wic.CreateFormatConverter()?;
            conv.Initialize(&src, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
            g.dc.CreateBitmapFromWicBitmap(&conv, None)
        })();
        DestroyIcon(hi);
        r.ok()
    };
    g.icons.insert((idx, size), made.clone());
    made
}

/// The picture of `p` (thumbs.rs) as a GPU bitmap, for drawing `px` big; None until it is here.
unsafe fn picture(g: &mut Gpu, p: &Path, px: i32) -> Option<ID2D1Bitmap1> {
    let (w0, h0, data) = thumbs::pixels(p, px)?;
    let key = (p.to_path_buf(), w0);
    if let Some(b) = g.pics.get(&key) { return Some(b.clone()); }
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
        ..Default::default()
    };
    let b = g.dc.CreateBitmap(D2D_SIZE_U { width: w0 as u32, height: h0 as u32 }, Some(data as *const _), (w0 * 4) as u32, &props).ok()?;
    if g.pics.len() > 600 { g.pics.clear(); }
    g.pics.insert(key, b.clone());
    Some(b)
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
    x0: i32, cy0: i32,  // where it started: client x, y in the content (independent of the scroll)
    x: i32, y: i32,     // the pointer now
    active: bool,       // moved far enough: a box, not a click
    keys: usize,        // MK_CONTROL / MK_SHIFT at the press
    hit: Option<usize>, // the item pressed on (beside its name)
    base: Vec<usize>,   // selected before, kept with Ctrl / Shift (sorted)
    prev: Vec<usize>,   // the items the box touched last time (sorted)
}

struct View {
    top: f32,              // scrolled this far (px), what is on screen
    goal: f32,             // where the glide is going
    from: f32,             // where it started
    t0: Option<Instant>,   // when it started (None: not gliding)
    zoom: f32,             // picture size on screen now (logical px), gliding to the zoom step's
    zfrom: f32,            // the size the zoom glide started from
    zt0: Option<Instant>,  // when it started
    hover: Option<usize>,
    drag: Drag,
    dir: PathBuf,
    edit: isize,           // the rename box (a child of the main window), 0 when none
    edit_row: usize,
}

thread_local! { static V: RefCell<View> = RefCell::new(View { top: 0.0, goal: 0.0, from: 0.0, t0: None, zoom: 0.0, zfrom: 0.0, zt0: None, hover: None, drag: Drag::None, dir: PathBuf::new(), edit: 0, edit_row: 0 }); }

pub fn hwnd() -> HWND { SLOT_WND[SLOT_LIST].load(Relaxed) as HWND }

/// Is this list on screen now?
pub fn active() -> bool { let h = hwnd(); !h.is_null() && unsafe { IsWindowVisible(h) } != 0 }

/// Should this list show (folder windows, a GPU to draw with; RX_OLDLIST=1 keeps the system list)?
pub fn wanted() -> bool { !desk() && !FAILED.load(Relaxed) && env::var_os("RX_OLDLIST").is_none() && !hwnd().is_null() }
use super::View as View_;

#[derive(Clone, Copy, PartialEq)]
enum Mode { Rows, Icons, List }

fn mode() -> Mode {
    match ST.with(|c| c.try_borrow().map_or(View_::Details, |s| s.view)) { View_::Icons => Mode::Icons, View_::List => Mode::List, _ => Mode::Rows }
}

fn row_h() -> i32 { ui::row_px() }
fn head_h() -> i32 { if mode() == Mode::Rows { sc(HEAD) } else { 0 } }
fn lv() -> HWND { ST.with(|c| c.try_borrow().map_or(ptr::null_mut(), |s| s.list)) }
fn rows_n() -> usize { ST.with(|c| c.try_borrow().map_or(0, |s| s.rows.len())) }
fn client() -> RECT { let mut cr: RECT = unsafe { std::mem::zeroed() }; unsafe { GetClientRect(hwnd(), &mut cr) }; cr }

/// The pictures' size on screen now (physical px).
fn pic_px() -> i32 { let z = V.with_borrow(|v| v.zoom); px_of(if z > 0.0 { z } else { zoom_target() }) }

/// Physical px of a picture size `z` (logical px).
fn px_of(z: f32) -> i32 { (z * sc(100) as f32 / 100.0).round() as i32 }

/// The grid of the List and Icons views: columns, cell size, margins.
#[derive(Clone, Copy)]
struct Grid { cols: usize, cw: i32, ch: i32, x0: i32, y0: i32 }

fn grid() -> Grid { grid_at(pic_px()) }

/// The grid with pictures `px` big (physical px).
fn grid_at(px: i32) -> Grid {
    let w = client().right;
    let (min_w, ch) = match mode() {
        Mode::Icons => ((px + sc(24)).max(sc(92)), px + sc(52)),
        _ => (sc(260), row_h()),
    };
    let cols = ((w - sc(12)) / min_w).max(1) as usize;
    Grid { cols, cw: ((w - sc(12)) / cols as i32).max(1), ch, x0: sc(6), y0: sc(6) }
}

/// Item `i`'s box in the content (y from the top of the content, before scrolling).
fn item_box(i: usize) -> (i32, i32, i32, i32) {
    match mode() {
        Mode::Rows => (0, i as i32 * row_h(), client().right, row_h()),
        _ => { let g = grid(); (g.x0 + (i % g.cols) as i32 * g.cw, g.y0 + (i / g.cols) as i32 * g.ch, g.cw, g.ch) }
    }
}

fn content_h() -> i32 {
    let n = rows_n();
    match mode() {
        Mode::Rows => (n + PAD_ROWS) as i32 * row_h(),
        _ => { let g = grid(); g.y0 + n.div_ceil(g.cols) as i32 * g.ch + sc(40) }
    }
}

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
fn select_range(a: usize, b: usize) {
    clear_sel();
    for k in a.min(b)..=a.max(b) { set_state(k, LVIS_SELECTED, LVIS_SELECTED); }
    set_state(b, LVIS_FOCUSED, LVIS_FOCUSED);
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

/// The rows changed (a folder listed, a filter, a sort, a new view). A new folder starts at the top, or where its
/// selected item is (the folder you came out of).
pub fn reset() {
    let dir = ST.with(|c| c.try_borrow().map(|s| s.dir.clone()).unwrap_or_default());
    let new_dir = V.with_borrow_mut(|v| { let n = v.dir != dir; if n { v.top = 0.0; v.goal = 0.0; v.t0 = None; v.dir = dir; } n });
    GPU.with_borrow_mut(|g| if let Some(g) = g { g.texts.clear(); if new_dir { g.pics.clear(); } });
    if new_dir { COUNTS.with_borrow_mut(|c| c.clear()); }
    clamp();
    if new_dir { if let Some(f) = focused() { show_now(f); } }
    invalidate();
}

/// The zoom step changed: pictures glide to the new size (between two picture sizes) or jump (another view).
pub fn zoom(glide: bool) {
    let target = zoom_target();
    V.with_borrow_mut(|v| {
        if !glide || v.zoom == 0.0 { v.zoom = target; v.zt0 = None; } else { v.zfrom = v.zoom; v.zt0 = Some(Instant::now()); }
    });
    // the layouts check their width themselves: only another view lays every text out again
    if !glide { GPU.with_borrow_mut(|g| if let Some(g) = g { g.texts.clear() }); }
    clamp();
    if let Some(f) = focused() { show_now(f); }
    animate(true);
    invalidate();
}

/// How long a glide takes, from its last push (the wheel, a key, a zoom step) to rest.
const SCROLL_S: f32 = 0.17;
const ZOOM_S: f32 = 0.16;
const FADE_S: f32 = 0.18;

/// Ease out (cubic): fast at first, settling softly, exactly there at 1.
fn ease(u: f32) -> f32 { let u = u.clamp(0.0, 1.0); 1.0 - (1.0 - u).powi(3) }

/// Highest scroll.
fn max_top() -> f32 { (content_h() - (client().bottom - head_h())).max(0) as f32 }

fn clamp() {
    let m = max_top();
    V.with_borrow_mut(|v| { v.goal = v.goal.clamp(0.0, m); v.top = v.top.clamp(0.0, m); v.from = v.from.clamp(0.0, m); });
}

/// Draws once more (no animation running: right away, through the message queue).
pub fn invalidate() {
    let h = hwnd();
    if !h.is_null() && PEND.fetch_or(1 << SLOT_LIST, Relaxed) & (1 << SLOT_LIST) == 0 { unsafe { PostMessageW(h, WM_FRAME, 0, 0) }; }
}

/// Glide toward `goal`.
fn scroll_to(goal: f32) {
    let m = max_top(); // before the state is borrowed: the grid reads the zoom from it
    // a new glide from where the list is now: it always ends SCROLL_S after the last push, however far it goes, so
    // the list never runs on after the wheel has stopped
    V.with_borrow_mut(|v| { v.goal = goal.clamp(0.0, m); v.from = v.top; v.t0 = Some(Instant::now()); });
    animate(true);
    invalidate();
}

/// One step of the glides (scroll, picture size, the fade between views); true while still moving. Each is worked out
/// from the time it started, not from the frame before: a late frame never slows them down.
fn step() -> bool {
    let target = zoom_target();
    let now = Instant::now();
    let icons = mode() == Mode::Icons;
    let z1 = V.with_borrow(|v| match v.zt0 {
        Some(t0) if icons && v.zoom != target => {
            let u = (now - t0).as_secs_f32() / ZOOM_S;
            Some(if u >= 1.0 { target } else { v.zfrom + (target - v.zfrom) * ease(u) })
        }
        _ => None,
    });
    if let Some(z1) = z1 {
        // the pictures grow or shrink; the item at the top stays where it is on screen. When the number of columns
        // changes, the items move to other places: the frame before fades into the new one instead of jumping.
        if grid_at(px_of(z1)).cols != grid().cols { snapshot(0.0); }
        let anchor = item_at_content(sc(8), V.with_borrow(|v| v.top) as i32 + sc(8));
        let before = anchor.map(|i| item_box(i).1 as f32 - V.with_borrow(|v| v.top));
        V.with_borrow_mut(|v| { v.zoom = z1; if z1 == target { v.zt0 = None; } });
        if let (Some(i), Some(off)) = (anchor, before) {
            let t = (item_box(i).1 as f32 - off).clamp(0.0, max_top());
            V.with_borrow_mut(|v| { v.top = t; v.goal = t; v.t0 = None; });
        }
    } else {
        V.with_borrow_mut(|v| { v.zoom = target; v.zt0 = None; });
    }
    let fading = GPU.with_borrow(|g| g.as_ref().is_some_and(|g| g.fade.is_some())) || COUNTING.get();
    V.with_borrow_mut(|v| {
        if let Some(t0) = v.t0 {
            let u = (now - t0).as_secs_f32() / SCROLL_S;
            if u >= 1.0 { v.top = v.goal; v.t0 = None; } else { v.top = v.from + (v.goal - v.from) * ease(u); }
        } else {
            v.top = v.goal;
        }
        v.t0.is_some() || v.zt0.is_some() || fading
    })
}

/// Item `i` into view with a glide (keys).
fn ensure_visible(i: usize) {
    let (_, y, _, h) = item_box(i);
    let view = (client().bottom - head_h()) as f32;
    let goal = V.with_borrow(|v| v.goal);
    let (y, h) = (y as f32, h as f32);
    if y < goal { scroll_to(y - if mode() == Mode::Rows { 0.0 } else { sc(6) as f32 }); } else if y + h > goal + view { scroll_to(y + h - view + if mode() == Mode::Rows { 0.0 } else { sc(6) as f32 }); }
}

/// Item `i` into view at once (a folder just opened, a rename box, a new zoom).
fn show_now(i: usize) {
    let (_, y, _, h) = item_box(i);
    let view = (client().bottom - head_h()) as f32;
    let top = V.with_borrow(|v| v.top);
    if y as f32 >= top && (y + h) as f32 <= top + view { return; }
    let t = (y as f32 - view / 3.0).clamp(0.0, max_top());
    V.with_borrow_mut(|v| { v.top = t; v.goal = t; v.t0 = None; });
}

/// The item at content point (x, y) (y before scrolling).
fn item_at_content(x: i32, y: i32) -> Option<usize> {
    let n = rows_n();
    let i = match mode() {
        Mode::Rows => y.div_euclid(row_h()),
        _ => {
            let g = grid();
            let (c, r) = ((x - g.x0).div_euclid(g.cw), (y - g.y0).div_euclid(g.ch));
            if c < 0 || c as usize >= g.cols || r < 0 { return None; }
            r * g.cols as i32 + c
        }
    };
    (i >= 0 && (i as usize) < n).then_some(i as usize)
}

/// The item at client point (x, y).
fn item_at(x: i32, y: i32) -> Option<usize> {
    if y < head_h() { return None; }
    let top = V.with_borrow(|v| v.top.round() as i32);
    item_at_content(x, y - head_h() + top)
}

/// The folder under screen point (x, y), for a drop: (item, its path).
pub fn folder_at(x: i32, y: i32) -> Option<(usize, PathBuf)> {
    let mut p = POINT { x, y };
    unsafe { windows_sys::Win32::Graphics::Gdi::ScreenToClient(hwnd(), &mut p) };
    let i = item_at(p.x, p.y)?;
    ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).filter(|r| r.dir).map(|r| (i, s.dir.join(&r.name)))))
}

/// Column edges: the system list's header widths (it fits them to the width), from x = 0.
fn columns() -> Vec<i32> {
    let mut x = vec![0];
    for k in 0..ui::COLS { let wd = put(lv(), LVM_GETCOLUMNWIDTH, k, 0) as i32; x.push(x[k] + wd); }
    x
}

const TREE_STEP: i32 = 18;
const TREE_ARROW: i32 = 16;

/// Where the name starts in row `i` (after the tree's indent and arrow, and the icon), and the indent.
fn name_left(i: usize) -> (i32, i32) {
    let (depth, tree) = ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).map(|r| (r.depth as i32, s.view == View_::Tree)))).unwrap_or((0, false));
    let indent = if tree { sc(TREE_STEP) * depth + sc(TREE_ARROW) } else { 0 };
    (sc(16) + indent + sc(16) + sc(12), indent)
}

/// The tree's arrow of row `i`, if (x) is on it.
fn on_arrow(i: usize, x: i32) -> bool {
    let (depth, dir, tree) = ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).map(|r| (r.depth as i32, r.dir, s.view == View_::Tree)))).unwrap_or((0, false, false));
    let ax = sc(10) + sc(TREE_STEP) * depth;
    tree && dir && x >= ax - sc(4) && x < ax + sc(TREE_ARROW) + sc(4)
}

fn text_width(g: &Gpu, s: &str, f: &IDWriteTextFormat) -> i32 {
    let s16: Vec<u16> = s.encode_utf16().collect();
    unsafe { g.dw.CreateTextLayout(&s16, f, 10000.0, 100.0).ok().and_then(|lay| { let mut m = DWRITE_TEXT_METRICS::default(); lay.GetMetrics(&mut m).ok().map(|_| m.widthIncludingTrailingWhitespace as i32) }).unwrap_or(0) }
}

/// Is client point (x, y) on item `i`'s picture or name? (A press there picks the item and can drag it; anywhere
/// else starts a box.)
fn on_name(g: &Gpu, i: usize, x: i32, y: i32) -> bool {
    let Some(name) = ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).map(|r| r.name.rsplit('\\').next().unwrap_or("").to_string()))) else { return false };
    let top = V.with_borrow(|v| v.top.round() as i32);
    let (bx, by, bw, bh) = item_box(i);
    let (bx, by) = (bx, by - top + head_h());
    match mode() {
        Mode::Rows => {
            let (l, indent) = name_left(i);
            x >= sc(10) + indent && x < (l + text_width(g, &name, &g.f_name) + sc(8)).min(columns()[1])
        }
        Mode::Icons => {
            let px = pic_px();
            let lw = text_width(g, &name, &g.f_label).min(bw - sc(12));
            let in_pic = x >= bx + (bw - px) / 2 && x < bx + (bw + px) / 2 && y >= by + sc(6) && y < by + sc(6) + px;
            let in_label = x >= bx + (bw - lw) / 2 - sc(4) && x < bx + (bw + lw) / 2 + sc(4) && y >= by + px + sc(10) && y < by + bh;
            in_pic || in_label
        }
        Mode::List => x >= bx + sc(4) && x < bx + sc(36) + text_width(g, &name, &g.f_name).min(bw - sc(44)) && y >= by && y < by + bh,
    }
}

// ---------- drawing ----------

unsafe fn render() {
    let h = hwnd();
    if h.is_null() || IsWindowVisible(h) == 0 { return; }
    let cr = client();
    if GPU.with_borrow(|g| g.is_none()) {
        match gpu_new(h) {
            Ok(g) => GPU.set(Some(g)),
            Err(_) => { FAILED.store(true, Relaxed); ui::sync_lists(); return; } // no GPU drawing: the system list takes over
        }
    }
    let ok = GPU.with_borrow_mut(|g| {
        let g = g.as_mut().unwrap();
        if resize(g, cr.right as u32, cr.bottom as u32).is_err() { return false; }
        compose(g, cr.right, cr.bottom);
        g.swap.Present(0, DXGI_PRESENT(0)).is_ok()
    });
    if !ok { GPU.set(None); invalidate(); } // the device went away (driver update, GPU reset): made again next frame
    if COUNTING.get() { animate(true); } // counts on their way: the next frames too
    fps_mark("new", V.with_borrow(|v| v.top));
}

/// Scale `k` around the middle of a `cw` x `ch` frame.
fn scale_mid(k: f32, cw: i32, ch: i32) -> windows_numerics::Matrix3x2 {
    windows_numerics::Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: cw as f32 / 2.0 * (1.0 - k), M32: ch as f32 / 2.0 * (1.0 - k) }
}

/// The frame: the list, and while the view changes the frame before over it, fading out. Zooming in, the old frame
/// grows a little as it goes and the new one grows into place (the other way round zooming out).
unsafe fn compose(g: &mut Gpu, cw: i32, ch: i32) {
    let fade = g.fade.as_ref().map(|(b, t0, d)| (b.clone(), ease(t0.elapsed().as_secs_f32() / FADE_S), *d));
    let Some((old, e, d)) = fade.filter(|f| f.1 < 1.0) else {
        g.fade = None;
        draw(g, cw, ch);
        return;
    };
    let dc = g.dc.clone();
    const GROW: f32 = 0.045;
    dc.SetTransform(&scale_mid(1.0 - GROW * d * (1.0 - e), cw, ch));
    draw(g, cw, ch);
    dc.BeginDraw();
    dc.SetTransform(&scale_mid(1.0 + GROW * d * e, cw, ch));
    dc.DrawBitmap(&old, Some(&rf(0.0, 0.0, cw as f32, ch as f32)), 1.0 - e, D2D1_INTERPOLATION_MODE_LINEAR, None, None);
    dc.SetTransform(&windows_numerics::Matrix3x2::identity());
    let _ = dc.EndDraw(None, None);
}

/// Keeps what is on screen now, to fade it into what comes next (a change of view: call before the change). `d`: +1
/// the view grows (Ctrl+wheel up), -1 it shrinks, 0 neither.
pub fn snapshot(d: f32) {
    if !active() { return; }
    let cr = client();
    unsafe {
        GPU.with_borrow_mut(|g| {
            let Some(g) = g.as_mut() else { return };
            if g.size != (cr.right as u32, cr.bottom as u32) || cr.right <= 0 || cr.bottom <= 0 { return; }
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
                ..Default::default()
            };
            let Ok(bmp) = g.dc.CreateBitmap(D2D_SIZE_U { width: g.size.0, height: g.size.1 }, None, 0, &props) else { return };
            // drawn again into a picture of its own (the swap chain's buffers cannot be read back), fade and all
            g.dc.SetTarget(&bmp);
            compose(g, cr.right, cr.bottom);
            if target(g).is_err() { return; }
            g.fade = Some((bmp, Instant::now(), d));
        });
    }
    animate(true);
    invalidate();
}

/// The text layout of `text` for item `key` (item, column), kept while the text and the width stay: made again only
/// for items scrolled into view or a new width (laying text out is the dearest part of a frame).
unsafe fn layout(g: &mut Gpu, key: (usize, u8), text: &str, wd: i32, ht: f32, fmt: &IDWriteTextFormat) -> Option<IDWriteTextLayout> {
    if let Some((w0, t0, l)) = g.texts.get(&key) && *w0 == wd && t0 == text { return Some(l.clone()); }
    let s16: Vec<u16> = text.encode_utf16().collect();
    let l = g.dw.CreateTextLayout(&s16, fmt, wd.max(1) as f32, ht).ok()?;
    if g.texts.len() > 4000 { g.texts.clear(); }
    g.texts.insert(key, (wd, text.to_string(), l.clone()));
    Some(l)
}

/// What an item looks like: its fill (selection, the pointer, a drop, every other row) or None for the plain background.
fn look(i: usize, on: bool, hot: bool, focus: bool, drop: isize, zebra: bool, bg: u32) -> u32 {
    match (on, focus) {
        _ if drop == i as isize => mix(bg, pal().accent, 0.32), // where a drag would go
        (true, true) => mix(bg, pal().accent, if hot { 0.26 } else { 0.20 }),
        (true, false) => mix(bg, pal().text, 0.10),
        _ if hot => mix(bg, pal().text, 0.055),
        _ if zebra && i % 2 == 1 => mix(bg, pal().text, 0.035), // every other row a shade lighter
        _ => bg,
    }
}

unsafe fn draw(g: &mut Gpu, cw: i32, ch: i32) {
    COUNTING.set(false); // set again by a count still on its way
    let dc = g.dc.clone();
    dc.BeginDraw();
    let bg = ui::listbg();
    dc.Clear(Some(&color(bg)));
    let md = mode();
    let top = V.with_borrow(|v| v.top.round() as i32);
    let hh = head_h();
    let focus = GetFocus() == hwnd() || V.with_borrow(|v| v.edit != 0);
    // the items in view, their icons asked for once
    let (first, last) = match md {
        Mode::Rows => { let rh = row_h(); ((top / rh).max(0) as usize, ((top + ch) / rh + 1).max(0) as usize) }
        _ => { let gr = grid(); (((top - gr.y0).max(0) / gr.ch) as usize * gr.cols, ((top + ch - gr.y0) / gr.ch + 1).max(0) as usize * gr.cols) }
    };
    let items: Vec<(usize, Row)> = ST.with(|c| {
        let Ok(mut st) = c.try_borrow_mut() else { return vec![] };
        let dir = st.dir.clone();
        let n = st.rows.len();
        (first.min(n)..last.min(n)).map(|i| {
            let r = &mut st.rows[i];
            if r.icon < 0 { r.icon = icon_of(&dir.join(&r.name), r.dir); }
            (i, r.clone())
        }).collect()
    });
    let dir = ST.with(|c| c.try_borrow().map(|s| s.dir.clone()).unwrap_or_default());
    let (hover, drop) = (V.with_borrow(|v| v.hover), ui::drop_row());
    let edit_row = V.with_borrow(|v| (v.edit != 0).then_some(v.edit_row));
    dc.PushAxisAlignedClip(&rf(0.0, hh as f32, cw as f32, ch as f32), D2D1_ANTIALIAS_MODE_ALIASED);
    let brush = g.brush.clone();
    let paint = |c: u32| brush.SetColor(&color(c));
    match md {
        Mode::Rows => draw_rows(g, &dc, &brush, &items, &dir, cw, top, hh, focus, hover, drop, edit_row, bg),
        _ => {
            for (i, r) in &items {
                let (bx, by, bw, bh) = item_box(*i);
                let (bx, by) = (bx, by - top + hh);
                let on = is_sel(*i);
                let fill = look(*i, on, hover == Some(*i), focus, drop, false, bg);
                let shown = r.name.rsplit('\\').next().unwrap_or(&r.name).to_string();
                if md == Mode::Icons {
                    let px = pic_px();
                    if fill != bg {
                        paint(fill);
                        dc.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: rf((bx + sc(2)) as f32, (by + sc(2)) as f32, (bx + bw - sc(2)) as f32, (by + bh - sc(2)) as f32), radiusX: sc(8) as f32, radiusY: sc(8) as f32 }, &brush);
                    }
                    let (px0, py0) = (bx + (bw - px) / 2, by + sc(6));
                    // the picture fitted into its square, or the system icon as big until the picture is here
                    let full = dir.join(&r.name);
                    let (bmp, w0, h0) = match picture(g, &full, px) {
                        Some(b) => { let s = b.GetPixelSize(); (Some(b), s.width as i32, s.height as i32) }
                        None => { let b = icon(g, r.icon, px); let s = b.as_ref().map_or(Default::default(), |b| b.GetPixelSize()); (b, s.width as i32, s.height as i32) }
                    };
                    if let Some(b) = bmp {
                        let m = w0.max(h0).max(1) as f32;
                        let k = (px as f32 / m).min(if (m as i32) < px / 2 { 1.0 } else { f32::MAX }); // tiny icons stay sharp
                        let (dw, dh) = (w0 as f32 * k, h0 as f32 * k);
                        let (x, y) = (px0 as f32 + (px as f32 - dw) / 2.0, py0 as f32 + (px as f32 - dh) / 2.0);
                        dc.DrawBitmap(&b, Some(&rf(x.round(), y.round(), (x + dw).round(), (y + dh).round())), 1.0, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, None, None);
                    }
                    if edit_row != Some(*i) {
                        let f = g.f_label.clone();
                        if let Some(l) = layout(g, (*i, 9), &shown, bw - sc(12), sc(34) as f32, &f) {
                            paint(if on { pal().text } else { mix(pal().dim, pal().text, 0.75) });
                            dc.DrawTextLayout(Vector2 { X: (bx + sc(6)) as f32, Y: (py0 + px + sc(6)) as f32 }, &l, &brush, D2D1_DRAW_TEXT_OPTIONS_CLIP);
                        }
                    }
                } else {
                    if fill != bg {
                        paint(fill);
                        dc.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: rf((bx + sc(2)) as f32, (by + 1) as f32, (bx + bw - sc(4)) as f32, (by + bh - 1) as f32), radiusX: sc(5) as f32, radiusY: sc(5) as f32 }, &brush);
                    }
                    let s = sc(16);
                    if let Some(b) = icon(g, r.icon, s) {
                        dc.DrawBitmap(&b, Some(&rf((bx + sc(8)) as f32, (by + (bh - s) / 2) as f32, (bx + sc(8) + s) as f32, (by + (bh + s) / 2) as f32)), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, None, None);
                    }
                    if edit_row != Some(*i) {
                        let f = g.f_name.clone();
                        if let Some(l) = layout(g, (*i, 10), &shown, (bw - sc(44)).max(1), bh as f32, &f) {
                            paint(pal().text);
                            dc.DrawTextLayout(Vector2 { X: (bx + sc(8) + s + sc(8)) as f32, Y: by as f32 }, &l, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                        }
                    }
                }
            }
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
    if md == Mode::Rows { draw_titles(g, &dc, &brush, cw, hh); }
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

/// The Details and Tree rows.
#[allow(clippy::too_many_arguments)]
unsafe fn draw_rows(g: &mut Gpu, dc: &ID2D1DeviceContext, brush: &ID2D1SolidColorBrush, items: &[(usize, Row)], dir: &Path, cw: i32, top: i32, hh: i32, focus: bool, hover: Option<usize>, drop: isize, edit_row: Option<usize>, bg: u32) {
    let paint = |c: u32| brush.SetColor(&color(c));
    let rh = row_h();
    let x = columns();
    let tree = ST.with(|c| c.try_borrow().map_or(false, |s| s.view == View_::Tree));
    let right = (*x.last().unwrap()).min(cw);
    for (i, r) in items {
        let y = hh + *i as i32 * rh - top;
        let on = is_sel(*i);
        let fill = look(*i, on, hover == Some(*i), focus, drop, true, bg);
        if fill != bg {
            paint(fill);
            let rr = D2D1_ROUNDED_RECT { rect: rf(sc(6) as f32, (y + 1) as f32, (right - sc(6)) as f32, (y + rh - 1) as f32), radiusX: sc(5) as f32, radiusY: sc(5) as f32 };
            dc.FillRoundedRectangle(&rr, brush);
        }
        let (nl, indent) = name_left(*i);
        if tree && r.dir {
            // the tree's arrow: right when closed, down when open
            let ax = sc(10) + sc(TREE_STEP) * r.depth as i32;
            let t: Vec<u16> = (if r.open { "\u{E70D}" } else { "\u{E76C}" }).encode_utf16().collect();
            if let Ok(lay) = g.dw.CreateTextLayout(&t, &g.f_glyph, sc(TREE_ARROW) as f32, rh as f32) {
                paint(if r.open { pal().text } else { pal().dim });
                dc.DrawTextLayout(Vector2 { X: ax as f32, Y: y as f32 }, &lay, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
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
            // laid out once and kept while the text and the width stay (counts change while folders are counted)
            let f = if name { g.f_name.clone() } else { g.f_det.clone() };
            let Some(lay) = layout(g, (*i, col), t, (rgt - l).max(1), rh as f32, &f) else { return };
            paint(c);
            dc.DrawTextLayout(Vector2 { X: l as f32, Y: y as f32 }, &lay, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        };
        if edit_row != Some(*i) { cell(0, &shown, x[0] + nl, x[1] - sc(8), pal().text, true); }
        cell(1, &ui::friendly_date(r.time), x[1] + sc(10), x[2] - sc(8), dim, false);
        let kind = if Path::new(&r.name).is_absolute() {
            // a search hit: where it lies, below the shown folder or in full
            let p = Path::new(&r.name);
            let parent = p.parent().unwrap_or(p);
            match parent.strip_prefix(dir) { Ok(rel) if rel.as_os_str().is_empty() => "(this folder)".into(), Ok(rel) => rel.to_string_lossy().into_owned(), Err(_) => parent.to_string_lossy().into_owned() }
        } else { type_text(&r.name, r.dir) };
        cell(2, &kind, x[2] + sc(10), x[3] - sc(8), dim, false);
        if r.dir {
            let full = dir.join(&r.name);
            if let Some(s) = PREFS.with_borrow(|p| p.folder_sizes).then(|| sizes::get(&full)).flatten() {
                let c = if s.done { dim } else { mix(dim, bg, 0.45) };
                // while a total grows the numbers count up to it, each digit rolling into the next like a meter;
                // at rest they are plain text again
                let (now, moving) = tween(&full, [s.dirs as f64, s.files as f64, s.bytes as f64]);
                if moving {
                    COUNTING.set(true);
                    for (j, col) in [(0usize, 3usize), (1, 4), (2, 5)] {
                        let (v, dec, sep, unit) = if j < 2 { (now[j], 0, true, "") } else { size_unit(now[2]) };
                        meter(g, dc, brush, v, dec, sep, unit, x[col] + sc(10), y, rh, x[col + 1] - sc(8), c);
                    }
                } else {
                    let n = |k: u32| if k == 0 { "\u{2013}".to_string() } else { thousands(k as usize) };
                    cell(3, &n(s.dirs), x[3] + sc(10), x[4] - sc(8), c, false);
                    cell(4, &n(s.files), x[4] + sc(10), x[5] - sc(8), c, false);
                    cell(5, &size_text(s.bytes), x[5] + sc(10), x[6] - sc(8), c, false);
                }
            }
        } else {
            let off = mix(dim, bg, 0.5);
            cell(3, "\u{2013}", x[3] + sc(10), x[4] - sc(8), off, false);
            cell(4, "\u{2013}", x[4] + sc(10), x[5] - sc(8), off, false);
            cell(5, &size_text(r.size), x[5] + sc(10), x[6] - sc(8), dim, false);
        }
    }
}

// ---------- the rolling counts ----------

/// A folder's totals as shown: they follow the real ones, counting up (or down) to them.
struct Tween { shown: [f64; 3], at: Instant, live: bool }

thread_local! {
    static COUNTS: RefCell<HashMap<PathBuf, Tween>> = RefCell::new(HashMap::new());
    static COUNTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; // some count still on its way: frames go on
}

/// The totals of folder `p` to show this frame on their way to `target` (folders, files, bytes), and whether they are
/// still moving. Seen for the first time, they show as they are (nothing counts up when a folder opens).
fn tween(p: &Path, target: [f64; 3]) -> ([f64; 3], bool) {
    COUNTS.with_borrow_mut(|m| {
        let now = Instant::now();
        let t = m.entry(p.to_path_buf()).or_insert(Tween { shown: target, at: now, live: false });
        if t.shown == target { t.live = false; return (target, false); }
        // a part of the way each frame, by the time since the frame before (one frame's when it just started)
        let dt = if t.live { (now - t.at).as_secs_f64().min(0.05) } else { 1.0 / 120.0 };
        t.at = now;
        t.live = true;
        let k = 1.0 - (-dt / 0.14).exp();
        for j in 0..3 {
            let d = target[j] - t.shown[j];
            if d.abs() <= 0.5 + target[j].abs() * 2e-5 { t.shown[j] = target[j]; } else { t.shown[j] += d * k; }
        }
        (t.shown, true)
    })
}

/// Bytes as the size column shows them: (value in its unit, decimals, thousands marks, unit), like size_text.
fn size_unit(b: f64) -> (f64, u32, bool, &'static str) {
    const MB: f64 = (1u64 << 20) as f64;
    if b < MB { (b / 1024.0, 0, false, " KB") } else if b < MB * 1024.0 { (b / MB, 1, false, " MB") } else { (b / (MB * 1024.0), 2, false, " GB") }
}

/// One digit or mark of the rolling counts, laid out once: (layout, width).
unsafe fn glyph(g: &mut Gpu, t: &str, band: f32) -> Option<(IDWriteTextLayout, f32)> {
    if let Some(v) = g.glyphs.get(t) { return Some(v.clone()); }
    let s16: Vec<u16> = t.encode_utf16().collect();
    let l = g.dw.CreateTextLayout(&s16, &g.f_det, sc(200) as f32, band).ok()?;
    let mut m = DWRITE_TEXT_METRICS::default();
    l.GetMetrics(&mut m).ok()?;
    g.glyphs.insert(t.to_string(), (l.clone(), m.widthIncludingTrailingWhitespace));
    Some((l, m.widthIncludingTrailingWhitespace))
}

/// Draws `x` like a meter at the left of cell (l .. right) of the row at `y`: each digit rolls up into the next one as
/// the value passes it (the last digit always turning, the others only while the digits below them go from 9 to 0).
#[allow(clippy::too_many_arguments)]
unsafe fn meter(g: &mut Gpu, dc: &ID2D1DeviceContext, brush: &ID2D1SolidColorBrush, x: f64, dec: u32, sep: bool, unit: &str, l: i32, y: i32, rh: i32, right: i32, c: u32) {
    const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    let band = (sc(12) as f32 * 1.45).ceil();
    let top = y as f32 + (rh as f32 - band) / 2.0;
    let n = x.max(0.0) * 10f64.powi(dec as i32);
    let mut count = dec + 1; // digits drawn: at least one before the point
    while count < 19 && 10f64.powi(count as i32) <= n.floor() { count += 1; }
    brush.SetColor(&color(c));
    dc.PushAxisAlignedClip(&rf(l as f32, top, right as f32, top + band), D2D1_ANTIALIAS_MODE_ALIASED);
    let mut at = l as f32;
    let mut put = |g: &mut Gpu, t: &str, dy: f32, adv: bool| {
        if let Some((lay, w)) = glyph(g, t, band) {
            dc.DrawTextLayout(Vector2 { X: at, Y: top + dy }, &lay, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            if adv { at += w; }
        }
    };
    for k in (0..count).rev() {
        let p = 10f64.powi(k as i32);
        let d = ((n / p).floor() as u64 % 10) as usize;
        let roll = if k == 0 { n.fract() } else { ((n % p) - (p - 1.0)).clamp(0.0, 1.0) } as f32;
        let roll = roll * roll * (3.0 - 2.0 * roll); // eased: each turn settles on its digit
        if roll > 0.0 { put(g, DIGITS[(d + 1) % 10], (1.0 - roll) * band, false); }
        put(g, DIGITS[d], -roll * band, true);
        if dec > 0 && k == dec { put(g, ".", 0.0, true); } else if sep && k > dec && (k - dec).is_multiple_of(3) { put(g, ",", 0.0, true); }
    }
    if !unit.is_empty() { put(g, unit, 0.0, true); }
    dc.PopAxisAlignedClip();
}

/// The column titles, the sorted one with its arrow.
unsafe fn draw_titles(g: &Gpu, dc: &ID2D1DeviceContext, brush: &ID2D1SolidColorBrush, cw: i32, hh: i32) {
    let paint = |c: u32| brush.SetColor(&color(c));
    let x = columns();
    paint(pal().bg);
    dc.FillRectangle(&rf(0.0, 0.0, cw as f32, hh as f32), brush);
    let sorted = SORT_COL.load(Relaxed);
    let arrow = if SORT_ASC.load(Relaxed) { "  \u{2191}" } else { "  \u{2193}" };
    for (k, title) in ["Name", "Date modified", "Type", "Folders", "Files", "Size"].iter().enumerate() {
        let on = k == sorted;
        let t: Vec<u16> = format!("{title}{}", if on { arrow } else { "" }).encode_utf16().collect();
        let l = x[k] + sc(10);
        let wd = (x[k + 1] - sc(12) - l).max(1) as f32;
        if let Ok(lay) = g.dw.CreateTextLayout(&t, if on { &g.f_head_on } else { &g.f_head }, wd, hh as f32) {
            paint(if on { pal().text } else { pal().dim });
            dc.DrawTextLayout(Vector2 { X: l as f32, Y: 0.0 }, &lay, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
        paint(pal().line);
        dc.FillRectangle(&rf((x[k + 1] - 1) as f32, sc(8) as f32, x[k + 1] as f32, (hh - sc(8)) as f32), brush);
    }
}

/// The scrollbar thumb: (top, length) for a client height and scroll.
fn thumb(ch: i32, top: f32, m: f32) -> (f32, f32) {
    let view = (ch - head_h()) as f32;
    let track = view - sc(8) as f32;
    let len = (track * view / (m + view)).max(sc(24) as f32);
    (head_h() as f32 + sc(4) as f32 + (track - len) * (top / m.max(1.0)), len)
}

// ---------- the selection box ----------

/// Selects what the box touches (plus what was selected before, with Ctrl / Shift); only items that change are told.
fn box_select() {
    let n = rows_n();
    let top = V.with_borrow(|v| v.top.round() as i32);
    let md = mode();
    let g = grid(); // before the state is borrowed: the grid reads the zoom from it
    let Some((prev, want, base)) = V.with_borrow_mut(|v| {
        let Drag::Box(b) = &mut v.drag else { return None };
        let cy = b.y - head_h() + top;
        let (y0, y1) = (b.cy0.min(cy), b.cy0.max(cy));
        let mut want = vec![];
        match md {
            Mode::Rows => {
                let rh = row_h();
                let (a, z) = (y0.div_euclid(rh).max(0) as usize, y1.div_euclid(rh));
                if z >= 0 { want.extend(a..=(z as usize).min(n.saturating_sub(1))); }
            }
            _ => {
                let (x0, x1) = (b.x0.min(b.x), b.x0.max(b.x));
                let (c0, c1) = ((x0 - g.x0).div_euclid(g.cw).max(0), (x1 - g.x0).div_euclid(g.cw).min(g.cols as i32 - 1));
                let (r0, r1) = ((y0 - g.y0).div_euclid(g.ch).max(0), (y1 - g.y0).div_euclid(g.ch));
                for r in r0..=r1 { for c in c0..=c1 { let i = (r * g.cols as i32 + c) as usize; if i < n { want.push(i); } } }
            }
        }
        if n == 0 { want.clear(); }
        let prev = std::mem::replace(&mut b.prev, want.clone());
        Some((prev, want, b.base.clone()))
    }) else { return };
    // off: in the last box, not in this one; on: in this one, not in the last
    for &i in &prev { if want.binary_search(&i).is_err() { set_state(i, LVIS_SELECTED, if base.binary_search(&i).is_ok() { LVIS_SELECTED } else { 0 }); } }
    for &i in &want { if prev.binary_search(&i).is_err() { set_state(i, LVIS_SELECTED, LVIS_SELECTED); } }
}

// ---------- renaming ----------

thread_local! { static EDIT_FONT: std::cell::Cell<isize> = const { std::cell::Cell::new(0) }; }
const WM_EDIT_DONE: u32 = WM_APP + 31; // the rename box is done: wp = 1 keep the new name, 0 drop it

/// Opens item `i`'s name for typing in a box over it (F2, the menu's Rename, a new folder); for a file only the part
/// before the extension is selected, as in Explorer.
pub unsafe fn begin_rename(i: usize) {
    end_rename(false);
    let Some((name, dir)) = ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).map(|r| (r.name.rsplit('\\').next().unwrap_or("").to_string(), r.dir)))) else { return };
    show_now(i);
    let main = GetAncestor(hwnd(), GA_ROOT);
    let mut wr: RECT = std::mem::zeroed();
    GetWindowRect(hwnd(), &mut wr);
    let mut p = POINT { x: wr.left, y: wr.top };
    windows_sys::Win32::Graphics::Gdi::ScreenToClient(main, &mut p);
    let top = V.with_borrow(|v| v.top.round() as i32);
    let (bx, by, bw, bh) = item_box(i);
    let eh = sc(24);
    let (l, r, y) = match mode() {
        Mode::Rows => { let (nl, _) = name_left(i); let x = columns(); (x[0] + nl - sc(4), x[1] - sc(8), by + (bh - eh.min(bh)) / 2) }
        Mode::Icons => (bx + sc(4), bx + bw - sc(4), by + pic_px() + sc(8)),
        Mode::List => (bx + sc(30), bx + bw - sc(6), by + (bh - eh.min(bh)) / 2),
    };
    let (l, r, y) = (p.x + l, p.x + r, p.y + head_h() + y - top);
    let e = CreateWindowExW(0, w("EDIT").as_ptr(), w(&name).as_ptr(), WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32 | if mode() == Mode::Icons { ES_CENTER as u32 } else { 0 }, l, y, (r - l).max(sc(80)), eh.min(bh.max(eh)), main, ptr::null_mut(), GetModuleHandleW(ptr::null()), ptr::null());
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
    thread_local! { static B: std::cell::Cell<isize> = const { std::cell::Cell::new(0) }; }
    let c = mix(ui::listbg(), pal().text, 0.10);
    SetTextColor(dc, pal().text);
    SetBkColor(dc, c);
    if B.get() == 0 { B.set(CreateSolidBrush(c) as isize); }
    Some(B.get())
}

// ---------- input ----------

/// Arrow keys in the grid views, here (the system list behind lays its icons out its own way): moves the focus a cell
/// or a row (Shift reaches from the last picked one) and glides it into view.
unsafe fn grid_key(vk: u32) -> bool {
    let n = rows_n();
    if n == 0 { return false; }
    let g = grid();
    let rows_in_view = ((client().bottom / g.ch).max(1) as usize) * g.cols;
    let cur = focused().unwrap_or(0) as isize;
    let to = match vk {
        0x25 => cur - 1, 0x27 => cur + 1, 0x26 => cur - g.cols as isize, 0x28 => cur + g.cols as isize,
        0x21 => cur - rows_in_view as isize, 0x22 => cur + rows_in_view as isize, 0x24 => 0, 0x23 => n as isize - 1,
        _ => return false,
    }.clamp(0, n as isize - 1) as usize;
    if GetKeyState(VK_SHIFT as i32) < 0 {
        let mark = usize::try_from(put(lv(), LVM_GETSELECTIONMARK, 0, 0)).unwrap_or(cur as usize);
        select_range(mark, to);
    } else {
        select_only(to);
    }
    ensure_visible(to);
    invalidate();
    true
}

pub unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
    let main = GetAncestor(h, GA_ROOT);
    match m {
        WM_FRAME => {
            frame_done(SLOT_LIST);
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
        WM_SIZE => { clamp(); GPU.with_borrow_mut(|g| if let Some(g) = g { g.texts.clear() }); render(); 0 }
        WM_MOUSEWHEEL => {
            let d = ((wp >> 16) & 0xFFFF) as i16 as i32;
            if wp & 0x0008 != 0 {
                // Ctrl+wheel: zoom, one step per notch (a touchpad's or a fine wheel's small turns add up)
                let steps = ui::ctrl_wheel_steps(d);
                if steps != 0 { PostMessageW(main, ui::WM_UI, 25, steps as isize); }
                return 0;
            }
            end_rename(true);
            let mut lines = 3u32;
            SystemParametersInfoW(SPI_GETWHEELSCROLLLINES, 0, &mut lines as *mut _ as *mut _, 0);
            // every notch (and every small touchpad turn) moves the goal by its share of rows, to the pixel
            let unit = if mode() == Mode::Icons { grid().ch as f32 / 2.0 } else { row_h() as f32 };
            let goal = V.with_borrow(|v| v.goal) - d as f32 * lines.min(10) as f32 * unit / 120.0;
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
            let hit = item_at(x, y);
            if let Some(i) = hit.filter(|&i| mode() == Mode::Rows && on_arrow(i, x)) { PostMessageW(main, ui::WM_UI, 26, i as isize); return 0; } // the tree's arrow
            if m == WM_LBUTTONDBLCLK { if let Some(i) = hit { open_row(main, i); } return 0; }
            let keys = wp & 0x000C; // MK_SHIFT | MK_CONTROL
            let on = hit.filter(|&i| GPU.with_borrow(|g| g.as_ref().is_some_and(|g| on_name(g, i, x, y))));
            match on {
                Some(i) => {
                    // on a name or picture: picks it (Ctrl adds or takes away, Shift reaches from the last one), and a
                    // drag moves it
                    let was = is_sel(i);
                    if keys & 0x0008 != 0 {
                        set_state(i, LVIS_SELECTED | LVIS_FOCUSED, if was { LVIS_FOCUSED } else { LVIS_SELECTED | LVIS_FOCUSED });
                        put(lv(), LVM_SETSELECTIONMARK, 0, i as isize);
                    } else if keys & 0x0004 != 0 {
                        select_range(usize::try_from(put(lv(), LVM_GETSELECTIONMARK, 0, 0)).unwrap_or(i), i);
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
                    V.with_borrow_mut(|v| v.drag = Drag::Box(SelBox { x0: x, cy0: y - head_h() + top, x, y, active: false, keys, hit, base, prev: vec![] }));
                    SetCapture(h);
                }
            }
            0
        }
        WM_RBUTTONDOWN => {
            SetFocus(h);
            match item_at(x, y) {
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
        WM_MBUTTONDOWN => {
            // the middle button: a folder in a new tab
            if let Some((_, p)) = item_at(x, y).and_then(|i| ST.with(|c| c.try_borrow().ok().and_then(|s| s.rows.get(i).filter(|r| r.dir).map(|r| (i, s.dir.join(&r.name)))))) { open_in_new_tab(main, p); }
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
                    V.with_borrow_mut(|v| { v.top = t; v.goal = t; v.t0 = None; });
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
                    let hv = item_at(x, y);
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
                    // only a click beside a name: that item, as a click on it would; on empty space nothing selected
                    match b.hit {
                        Some(i) if b.keys & 0x0008 != 0 => { let was = is_sel(i); set_state(i, LVIS_SELECTED | LVIS_FOCUSED, if was { LVIS_FOCUSED } else { LVIS_SELECTED | LVIS_FOCUSED }); }
                        Some(i) if b.keys & 0x0004 != 0 => select_range(usize::try_from(put(lv(), LVM_GETSELECTIONMARK, 0, 0)).unwrap_or(i), i),
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
        // type-to-filter, Enter); this list follows with a glide. In the grid views the arrows are handled here.
        WM_KEYDOWN | WM_KEYUP | WM_CHAR => {
            if m == WM_KEYDOWN && mode() != Mode::Rows && GetKeyState(VK_CONTROL as i32) >= 0 && grid_key(wp as u32) { return 0; }
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
    if mode() != Mode::Rows { return None; }
    let e = columns();
    (0..ui::COLS - 1).find(|&k| (x - e[k + 1]).abs() <= sc(4))
}

fn selected_paths() -> Vec<PathBuf> {
    let sel = selected_rows();
    ST.with(|c| c.try_borrow().map(|s| sel.iter().filter_map(|&i| s.rows.get(i)).map(|r| s.dir.join(&r.name)).collect()).unwrap_or_default())
}

/// Makes the window (shown by sync_lists) and lets it take drops.
pub unsafe fn create(parent: HWND) {
    let h = CreateWindowExW(0, w(CLASS).as_ptr(), ptr::null(), WS_CHILD | WS_CLIPSIBLINGS, 0, 0, 0, 0, parent, ptr::null_mut(), GetModuleHandleW(ptr::null()), ptr::null());
    SLOT_WND[SLOT_LIST].store(h as isize, Relaxed);
    let _ = sh::register_drop(h as _, parent as isize);
}
