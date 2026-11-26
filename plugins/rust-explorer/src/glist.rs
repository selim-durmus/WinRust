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
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};

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

#[derive(Default)]
struct View {
    top: f32,              // scrolled this far (px), what is on screen
    goal: f32,             // where the glide is going
    last: Option<Instant>, // the frame before
    sel: Vec<bool>,
    focus: Option<usize>,
    anchor: Option<usize>,
    hover: Option<usize>,
    drag_bar: Option<(i32, f32)>, // the scrollbar thumb held: (pointer y, top) at the press
    dir: PathBuf,
    shown: usize, // how many rows the state was made for
}

thread_local! { static V: RefCell<View> = RefCell::new(View::default()); }

pub fn hwnd() -> HWND { WND.load(Relaxed) as HWND }

/// Is the smooth list the one on screen (the setting, or RX_NEWLIST=1), for the view shown?
pub fn wanted() -> bool {
    !desk() && (env::var_os("RX_NEWLIST").is_some() || prefs_now().smooth_list) && ST.with(|c| c.try_borrow().map_or(false, |s| s.view == View_::Details))
}
use super::View as View_;

fn row_h() -> i32 { ui::row_px() }
fn head_h() -> i32 { sc(HEAD) }

/// The rows changed (a folder listed, a filter, a sort): fresh selection; a new folder starts at the top.
pub fn reset() {
    let (n, dir) = ST.with(|c| c.try_borrow().map(|s| (s.rows.len(), s.dir.clone())).unwrap_or_default());
    V.with_borrow_mut(|v| {
        if v.dir != dir { v.top = 0.0; v.goal = 0.0; v.dir = dir; }
        v.sel = vec![false; n];
        v.focus = None;
        v.anchor = None;
        v.shown = n;
    });
    GPU.with_borrow_mut(|g| if let Some(g) = g { g.texts.clear() });
    clamp();
    invalidate();
}

/// Highest scroll: the rows plus two empty ones at the end (room to right-click the folder).
fn max_top() -> f32 {
    let n = ST.with(|c| c.try_borrow().map_or(0, |s| s.rows.len()));
    let h = unsafe { let mut cr: RECT = std::mem::zeroed(); GetClientRect(hwnd(), &mut cr); cr.bottom };
    (((n + PAD_ROWS) as i32 * row_h()) - (h - head_h())).max(0) as f32
}

fn clamp() {
    let m = max_top();
    V.with_borrow_mut(|v| { v.goal = v.goal.clamp(0.0, m); v.top = v.top.clamp(0.0, m); });
}

/// Draws once more (no animation running: right away, through the message queue).
pub fn invalidate() {
    if !PENDING.swap(true, Relaxed) { unsafe { PostMessageW(hwnd(), WM_FRAME, 0, 0) }; }
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

fn row_at(y: i32) -> Option<usize> {
    if y < head_h() { return None; }
    let top = V.with_borrow(|v| v.top.round() as i32);
    let i = (y - head_h() + top).div_euclid(row_h());
    let n = ST.with(|c| c.try_borrow().map_or(0, |s| s.rows.len()));
    (i >= 0 && (i as usize) < n).then_some(i as usize)
}

/// Column edges: the old list's header widths (it still fits them to the width), from x = 0.
fn columns() -> Vec<i32> {
    let list = ST.with(|c| c.try_borrow().map_or(ptr::null_mut(), |s| s.list));
    let mut x = vec![0];
    for k in 0..ui::COLS { let wd = put(list, LVM_GETCOLUMNWIDTH, k, 0) as i32; x.push(x[k] + wd); }
    x
}

// ---------- drawing ----------

unsafe fn render() {
    let h = hwnd();
    if h.is_null() || IsWindowVisible(h) == 0 { return; }
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let made = GPU.with_borrow(|g| g.is_some());
    if !made {
        match gpu_new(h) { Ok(g) => GPU.set(Some(g)), Err(_) => return }
    }
    let ok = GPU.with_borrow_mut(|g| {
        let g = g.as_mut().unwrap();
        if resize(g, cr.right as u32, cr.bottom as u32).is_err() { return false; }
        draw(g, cr.right, cr.bottom);
        g.swap.Present(0, DXGI_PRESENT(0)).is_ok()
    });
    if !ok { GPU.set(None); } // the device went away (driver update, GPU reset): made again on the next frame
    let px = V.with_borrow(|v| v.top);
    fps_mark("new", px);
}

unsafe fn draw(g: &mut Gpu, cw: i32, ch: i32) {
    let dc = g.dc.clone();
    dc.BeginDraw();
    let bg = ui::listbg();
    dc.Clear(Some(&color(bg)));
    let top = V.with_borrow(|v| v.top.round() as i32);
    let (rh, hh) = (row_h(), head_h());
    let x = columns();
    let focus = GetFocus() == hwnd();
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
    let (sel, hover) = V.with_borrow(|v| (v.sel.clone(), v.hover));
    dc.PushAxisAlignedClip(&rf(0.0, hh as f32, cw as f32, ch as f32), D2D1_ANTIALIAS_MODE_ALIASED);
    let brush = g.brush.clone();
    let paint = |c: u32| brush.SetColor(&color(c));
    let right = (*x.last().unwrap()).min(cw);
    for (i, r) in &rows {
        let y = hh + *i as i32 * rh - top;
        let on = sel.get(*i).copied().unwrap_or(false);
        let hot = hover == Some(*i);
        let fill = match (on, focus) {
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
        let s = sc(16);
        if let Some(b) = icon(g, r.icon, s) {
            dc.DrawBitmap(&b, Some(&rf((x[0] + sc(16)) as f32, (y + (rh - s) / 2) as f32, (x[0] + sc(16) + s) as f32, (y + (rh + s) / 2) as f32)), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, None, None);
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
        cell(0, &shown, x[0] + sc(16) + s + sc(12), x[1] - sc(8), pal().text, true);
        cell(1, &ui::friendly_date(r.time), x[1] + sc(10), x[2] - sc(8), dim, false);
        cell(2, &type_text(&r.name, r.dir), x[2] + sc(10), x[3] - sc(8), dim, false);
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
        let track = (ch - hh - sc(8)) as f32;
        let total = m + (ch - hh) as f32;
        let len = (track * (ch - hh) as f32 / total).max(sc(24) as f32);
        let t0 = hh as f32 + sc(4) as f32 + (track - len) * (top as f32 / m);
        paint(mix(bg, pal().text, 0.28));
        let rr = D2D1_ROUNDED_RECT { rect: rf((cw - sc(7)) as f32, t0, (cw - sc(3)) as f32, t0 + len), radiusX: sc(2) as f32, radiusY: sc(2) as f32 };
        dc.FillRoundedRectangle(&rr, &brush);
    }
    let _ = dc.EndDraw(None, None);
}

// ---------- input ----------

unsafe fn select_only(i: usize) {
    V.with_borrow_mut(|v| { v.sel.iter_mut().for_each(|s| *s = false); if let Some(s) = v.sel.get_mut(i) { *s = true; } v.focus = Some(i); v.anchor = Some(i); });
}

/// Moves the focus by `d` rows (keys): selects it and glides it into view.
unsafe fn move_focus(d: isize) {
    let n = ST.with(|c| c.try_borrow().map_or(0, |s| s.rows.len()));
    if n == 0 { return; }
    let cur = V.with_borrow(|v| v.focus.map_or(-1, |f| f as isize));
    let i = (cur + d).clamp(0, n as isize - 1) as usize;
    select_only(i);
    ensure_visible(i);
    invalidate();
}

fn ensure_visible(i: usize) {
    let h = unsafe { let mut cr: RECT = std::mem::zeroed(); GetClientRect(hwnd(), &mut cr); cr.bottom };
    let (rh, view) = (row_h() as f32, (h - head_h()) as f32);
    let y = i as f32 * rh;
    let goal = V.with_borrow(|v| v.goal);
    if y < goal { scroll_to(y); } else if y + rh > goal + view { scroll_to(y + rh - view); }
}

fn selected_paths() -> Vec<PathBuf> {
    let sel = V.with_borrow(|v| v.sel.clone());
    ST.with(|c| c.try_borrow().map(|s| s.rows.iter().zip(&sel).filter(|(_, on)| **on).map(|(r, _)| s.dir.join(&r.name)).collect()).unwrap_or_default())
}

pub unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
    let main = GetAncestor(h, GA_ROOT);
    match m {
        WM_FRAME => {
            PENDING.store(false, Relaxed);
            let moving = step();
            render();
            if !moving { animate(false); fps_flush(); }
            0
        }
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
            let mut lines = 3u32;
            SystemParametersInfoW(SPI_GETWHEELSCROLLLINES, 0, &mut lines as *mut _ as *mut _, 0);
            // every notch (and every small touchpad turn) moves the goal by its share of rows, to the pixel
            let goal = V.with_borrow(|v| v.goal) - d as f32 * lines.min(10) as f32 * row_h() as f32 / 120.0;
            scroll_to(goal);
            0
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN => {
            SetFocus(h);
            let mut cr: RECT = std::mem::zeroed();
            GetClientRect(h, &mut cr);
            if m == WM_LBUTTONDOWN && x >= cr.right - sc(10) && y >= head_h() {
                // the scrollbar: held and dragged
                SetCapture(h);
                V.with_borrow_mut(|v| v.drag_bar = Some((y, v.top)));
                return 0;
            }
            if y < head_h() {
                if m == WM_LBUTTONDOWN { if let Some(c) = columns().windows(2).position(|e| x >= e[0] && x < e[1]) { sort_click(main, c); } }
                return 0;
            }
            match row_at(y) {
                Some(i) if m == WM_LBUTTONDBLCLK => open_row(main, i),
                Some(i) => {
                    let (ctrl, shift) = (wp & 0x0008 != 0, wp & 0x0004 != 0);
                    if m == WM_RBUTTONDOWN && V.with_borrow(|v| v.sel.get(i).copied().unwrap_or(false)) {
                    } else if ctrl {
                        V.with_borrow_mut(|v| { if let Some(s) = v.sel.get_mut(i) { *s = !*s; } v.focus = Some(i); v.anchor = Some(i); });
                    } else if shift {
                        V.with_borrow_mut(|v| { let a = v.anchor.unwrap_or(i); v.sel.iter_mut().enumerate().for_each(|(k, s)| *s = k >= a.min(i) && k <= a.max(i)); v.focus = Some(i); });
                    } else {
                        select_only(i);
                    }
                }
                None => V.with_borrow_mut(|v| { v.sel.iter_mut().for_each(|s| *s = false); v.focus = None; }),
            }
            invalidate();
            0
        }
        WM_RBUTTONUP => {
            let paths = selected_paths();
            if paths.is_empty() { bg_menu(main); } else { menu_choice(main, sh::show(main as _, &paths).unwrap_or(sh::Choice::Done), &paths); }
            0
        }
        WM_MOUSEMOVE => {
            if let Some((y0, t0)) = V.with_borrow(|v| v.drag_bar) {
                let mut cr: RECT = std::mem::zeroed();
                GetClientRect(h, &mut cr);
                let m = max_top();
                let track = (cr.bottom - head_h() - sc(8)) as f32;
                let total = m + (cr.bottom - head_h()) as f32;
                let len = (track * (cr.bottom - head_h()) as f32 / total).max(sc(24) as f32);
                let t = (t0 + (y - y0) as f32 * m / (track - len).max(1.0)).clamp(0.0, m);
                V.with_borrow_mut(|v| { v.top = t; v.goal = t; });
                invalidate();
                return 0;
            }
            let hv = row_at(y);
            if V.with_borrow(|v| v.hover) != hv {
                V.with_borrow_mut(|v| v.hover = hv);
                let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                TrackMouseEvent(&mut tme);
                invalidate();
            }
            0
        }
        0x02A3 => { V.with_borrow_mut(|v| v.hover = None); invalidate(); 0 } // WM_MOUSELEAVE
        WM_LBUTTONUP => {
            if V.with_borrow(|v| v.drag_bar.is_some()) { V.with_borrow_mut(|v| v.drag_bar = None); ReleaseCapture(); }
            0
        }
        WM_SETFOCUS | WM_KILLFOCUS => { invalidate(); 0 }
        WM_KEYDOWN => {
            let page = (|| { let mut cr: RECT = std::mem::zeroed(); GetClientRect(h, &mut cr); ((cr.bottom - head_h()) / row_h()).max(1) as isize })();
            match wp as u32 {
                0x26 => move_focus(-1),
                0x28 => move_focus(1),
                0x21 => move_focus(-page),
                0x22 => move_focus(page),
                0x24 => move_focus(isize::MIN / 2),
                0x23 => move_focus(isize::MAX / 2),
                0x0D => if let Some(f) = V.with_borrow(|v| v.focus) { open_row(main, f) },
                0x08 => if let Some(p) = cur_dir().parent() { load(main, p.to_path_buf()) },
                _ => {}
            }
            0
        }
        WM_GETDLGCODE => DLGC_WANTARROWS as isize,
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

/// Makes the window (hidden until wanted) and the pacing thread.
pub unsafe fn create(parent: HWND) {
    let h = CreateWindowExW(0, w(CLASS).as_ptr(), ptr::null(), WS_CHILD | WS_CLIPSIBLINGS, 0, 0, 0, 0, parent, ptr::null_mut(), GetModuleHandleW(ptr::null()), ptr::null());
    WND.store(h as isize, Relaxed);
    pacer();
}

