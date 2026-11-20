//! The UI style shared by the WinRust programs: %ProgramData%\WinRust\style.txt holds its name (the topbar gear menu writes it).
//! Programs re-read it at most once a second and repaint at once on the broadcast `set_style` sends (`msg()`).
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed},
};
use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, PostMessageW, RegisterWindowMessageW};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    Normal,
    Terminal, // monospace, green on black, square corners
    Dock,     // the bar is a floating pill in the middle
    Tiling,   // a thin status strip at the top, windows side by side
    Road,     // "White road": light ivory, faint triangles in the background
}

pub const ALL: [Style; 5] = [Style::Normal, Style::Terminal, Style::Dock, Style::Tiling, Style::Road];

impl Style {
    pub fn name(self) -> &'static str {
        match self {
            Style::Normal => "normal",
            Style::Terminal => "terminal",
            Style::Dock => "dock",
            Style::Tiling => "tiling",
            Style::Road => "road",
        }
    }
    /// Unknown or empty text (no file yet, a hand edit) is the normal style.
    pub fn parse(s: &str) -> Style {
        ALL.into_iter().find(|v| v.name().eq_ignore_ascii_case(s.trim())).unwrap_or(Style::Normal)
    }
    /// Square corners everywhere.
    // only the terminal keeps the monospace, square look; tiling uses the same sans and rounded shapes as the rest
    pub fn square(self) -> bool {
        self == Style::Terminal
    }
    pub fn mono(self) -> bool {
        self == Style::Terminal
    }
    /// Dark text on a light background.
    pub fn light(self) -> bool {
        self == Style::Road
    }
}

static CUR: AtomicU8 = AtomicU8::new(u8::MAX); // index into ALL; MAX = not read yet
static READ_AT: AtomicU64 = AtomicU64::new(0); // when the file was last read, ms since the epoch

fn file() -> Option<PathBuf> {
    std::env::var_os("ProgramData").map(|p| PathBuf::from(p).join("WinRust").join("style.txt"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn store(s: Style) {
    CUR.store(ALL.iter().position(|&v| v == s).unwrap_or(0) as u8, Relaxed);
    READ_AT.store(now_ms(), Relaxed);
}

/// Reads the file again now (on the "style changed" broadcast).
pub fn reload() {
    // WINRUST_STYLE overrides the file for this process only (to look at a style without switching every window)
    let name = std::env::var("WINRUST_STYLE").ok().or_else(|| file().and_then(|f| std::fs::read_to_string(f).ok()));
    store(Style::parse(&name.unwrap_or_default()));
}

/// The current style. The file is read again at most once a second, so a program that missed the broadcast still follows.
pub fn style() -> Style {
    if CUR.load(Relaxed) == u8::MAX || now_ms().saturating_sub(READ_AT.load(Relaxed)) > 1000 {
        reload();
    }
    ALL[CUR.load(Relaxed) as usize % ALL.len()]
}

/// Switches this process, writes the choice and tells the other WinRust windows to repaint in it.
pub fn set_style(s: Style) {
    store(s);
    if let Some(f) = file() {
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = std::fs::write(f, s.name());
    }
    unsafe { PostMessageW(HWND_BROADCAST, msg(), 0, 0) };
}

fn autohide_file() -> Option<PathBuf> {
    std::env::var_os("ProgramData").map(|p| PathBuf::from(p).join("WinRust").join("autohide.txt"))
}

/// The bar hides when the pointer is away and comes back at the screen edge (off unless the file says 1). Re-read once a second.
pub fn autohide() -> bool {
    if now_ms().saturating_sub(HIDE_AT.load(Relaxed)) > 1000 {
        HIDE.store(autohide_file().and_then(|f| std::fs::read_to_string(f).ok()).is_some_and(|s| s.trim() == "1") as u8, Relaxed);
        HIDE_AT.store(now_ms(), Relaxed);
    }
    HIDE.load(Relaxed) == 1
}

static HIDE: AtomicU8 = AtomicU8::new(0);
static HIDE_AT: AtomicU64 = AtomicU64::new(0);

pub fn set_autohide(on: bool) {
    HIDE.store(on as u8, Relaxed);
    HIDE_AT.store(now_ms(), Relaxed);
    if let Some(f) = autohide_file() {
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = std::fs::write(f, if on { "1" } else { "0" });
    }
}

fn opacity_file() -> Option<PathBuf> {
    std::env::var_os("ProgramData").map(|p| PathBuf::from(p).join("WinRust").join("bar-opacity.txt"))
}

/// How opaque the bar's background is, in percent: 100 = solid (no file, the default), 0 = fully clear (the wallpaper
/// shows through; icons and text stay solid). Re-read once a second.
pub fn bar_opacity() -> u8 {
    if now_ms().saturating_sub(OPACITY_AT.load(Relaxed)) > 1000 {
        OPACITY.store(parse_opacity(&opacity_file().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default()), Relaxed);
        OPACITY_AT.store(now_ms(), Relaxed);
    }
    OPACITY.load(Relaxed)
}

/// Text of bar-opacity.txt -> percent; empty or not a number (no file, a hand edit) is solid.
pub fn parse_opacity(s: &str) -> u8 {
    s.trim().trim_end_matches('%').trim().parse::<u32>().map_or(100, |v| v.min(100) as u8)
}

static OPACITY: AtomicU8 = AtomicU8::new(100);
static OPACITY_AT: AtomicU64 = AtomicU64::new(0);

pub fn set_bar_opacity(pct: u8) {
    let pct = pct.min(100);
    OPACITY.store(pct, Relaxed);
    OPACITY_AT.store(now_ms(), Relaxed);
    if let Some(f) = opacity_file() {
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = std::fs::write(f, pct.to_string());
    }
}

/// The broadcast message a window gets when the style changed: call `reload()` and repaint.
pub fn msg() -> u32 {
    static M: AtomicU32 = AtomicU32::new(0);
    match M.load(Relaxed) {
        0 => {
            let n: Vec<u16> = "WinRustStyleChanged\0".encode_utf16().collect();
            let m = unsafe { RegisterWindowMessageW(n.as_ptr()) };
            M.store(m, Relaxed);
            m
        }
        m => m,
    }
}

/// Colours of one style, as COLORREF (0x00BBGGRR) like GDI wants them.
pub struct Pal {
    pub bg: u32,
    pub line: u32,
    pub hover: u32,
    pub active: u32,
    pub accent: u32,
    pub text: u32,
    pub dim: u32,
}

pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// In the order of `ALL`. Neutral, low-contrast surfaces and one restrained accent (the look of macOS / modern fintech apps).
pub const PALS: [Pal; 5] = [
    // normal: graphite with the Apple system blue
    Pal { bg: rgb(22, 22, 24), line: rgb(46, 46, 50), hover: rgb(36, 36, 40), active: rgb(44, 44, 49), accent: rgb(10, 132, 255), text: rgb(245, 245, 247), dim: rgb(142, 142, 147) },
    // terminal: green on black
    Pal { bg: rgb(8, 11, 9), line: rgb(30, 48, 35), hover: rgb(16, 28, 20), active: rgb(22, 38, 27), accent: rgb(74, 222, 128), text: rgb(204, 232, 212), dim: rgb(98, 132, 108) },
    // dock: midnight ink with indigo
    Pal { bg: rgb(17, 18, 25), line: rgb(40, 42, 56), hover: rgb(29, 31, 42), active: rgb(37, 40, 54), accent: rgb(124, 134, 255), text: rgb(240, 241, 246), dim: rgb(138, 142, 160) },
    // tiling: dark with amber
    Pal { bg: rgb(16, 17, 20), line: rgb(38, 40, 46), hover: rgb(27, 29, 34), active: rgb(35, 37, 43), accent: rgb(245, 182, 82), text: rgb(224, 226, 231), dim: rgb(120, 125, 138) },
    // white road: warm ivory with gold
    Pal { bg: rgb(247, 245, 240), line: rgb(226, 220, 207), hover: rgb(238, 233, 222), active: rgb(232, 225, 210), accent: rgb(176, 134, 52), text: rgb(28, 26, 22), dim: rgb(124, 117, 104) },
];

/// Status colours shared by every style (Apple's dark-mode system green, yellow, red).
pub const GOOD: u32 = rgb(48, 209, 88);
pub const WARN: u32 = rgb(255, 214, 10);
pub const BAD: u32 = rgb(255, 69, 58);

pub fn pal() -> &'static Pal {
    &PALS[ALL.iter().position(|&s| s == style()).unwrap_or(0)]
}

/// A raised surface (a card, a tile): pure white on the light style, a step up from the background on the dark ones.
pub fn card() -> u32 {
    if style().light() { rgb(255, 255, 255) } else { mix(pal().bg, pal().text, 0.06) }
}

/// Text and glyphs drawn on an accent fill.
pub fn on_accent() -> u32 {
    if style().light() { rgb(255, 255, 255) } else { pal().bg }
}

/// `a` moved toward `b` by `t` (0..1), per channel.
pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| {
        let (x, y) = (((a >> s) & 255) as f32, ((b >> s) & 255) as f32);
        (x + (y - x) * t.clamp(0.0, 1.0)).round() as u32
    };
    ch(0) | ch(8) << 8 | ch(16) << 16
}

/// The White road background over `r`: faceted ivory triangles, a few of them traced in gold.
/// Does nothing in the other styles. Needs GDI+ started (GdiplusStartup) by the program.
pub unsafe fn backdrop(dc: windows_sys::Win32::Graphics::Gdi::HDC, r: windows_sys::Win32::Foundation::RECT, scale: f32) {
    use windows_sys::Win32::Graphics::GdiPlus::*;
    if !style().light() {
        return;
    }
    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if unsafe { GdipCreateFromHDC(dc, &mut g) } != 0 || g.is_null() {
        return;
    }
    let argb = |c: u32, a: f32| ((a.clamp(0.0, 1.0) * 255.0) as u32) << 24 | (c & 255) << 16 | (c & 0xFF00) | (c >> 16 & 255);
    let (x0, y0, w, h) = (r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32);
    let s = 58.0 * scale.max(0.5);
    let (cols, rows) = ((w / s) as i32 + 2, (h / (s * 0.866)) as i32 + 2);
    // a lattice of equilateral-ish triangles; each vertex nudged by a hash so the facets look cut by hand
    let pt = |i: i32, j: i32| {
        let (hx, hy) = (hash(i, j, 1) - 0.5, hash(i, j, 2) - 0.5);
        let off = if j & 1 == 1 { s * 0.5 } else { 0.0 };
        PointF { X: x0 + i as f32 * s - off + hx * s * 0.34, Y: y0 + j as f32 * s * 0.866 + hy * s * 0.3 - s * 0.4 }
    };
    let gold = pal().accent;
    unsafe {
        GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
        let mut br: *mut GpSolidFill = std::ptr::null_mut();
        GdipCreateSolidFill(0, &mut br);
        let mut pen: *mut GpPen = std::ptr::null_mut();
        GdipCreatePen1(0, 1.0, UnitPixel, &mut pen);
        for j in 0..rows {
            for i in 0..cols {
                let (a, b, c, d) = if j & 1 == 0 { (pt(i, j), pt(i + 1, j), pt(i, j + 1), pt(i + 1, j + 1)) } else { (pt(i, j), pt(i + 1, j), pt(i + 1, j + 1), pt(i + 2, j + 1)) };
                for (k, tri) in [[a, b, c], [b, d, c]].iter().enumerate() {
                    let v = hash(i, j, 3 + k as u32);
                    // light from the top left: most facets near white, some a touch warmer or greyer
                    let shade = if v < 0.5 { mix(pal().bg, rgb(255, 255, 255), 0.25 + v) } else { mix(pal().bg, rgb(226, 214, 190), (v - 0.5) * 0.45) };
                    GdipSetSolidFillColor(br, argb(shade, 1.0));
                    GdipFillPolygon(g, br as *mut GpBrush, tri.as_ptr(), 3, FillModeAlternate);
                    // facet edges: a hairline of white; one triangle in nine traced in gold
                    let (edge, wd) = if hash(i, j, 7 + k as u32) > 0.84 { (argb(gold, 0.6), 1.0 * scale) } else { (argb(rgb(255, 255, 255), 0.7), 0.6) };
                    GdipSetPenColor(pen, edge);
                    GdipSetPenWidth(pen, wd);
                    GdipDrawPolygon(g, pen, tri.as_ptr(), 3);
                }
            }
        }
        GdipDeletePen(pen);
        GdipDeleteBrush(br as *mut GpBrush);
        GdipDeleteGraphics(g);
    }
}

/// 0..1 from a lattice point (stable between paints).
fn hash(i: i32, j: i32, salt: u32) -> f32 {
    let mut x = (i as u32).wrapping_mul(0x9E37_79B1) ^ (j as u32).wrapping_mul(0x85EB_CA77) ^ salt.wrapping_mul(0xC2B2_AE3D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.0
}

fn font_file(name: &str) -> bool {
    std::env::var_os("WINDIR").is_some_and(|w| PathBuf::from(w).join("Fonts").join(name).is_file())
}

/// The proportional UI font: Windows 11's Segoe UI Variable where it is installed, else Segoe UI.
pub fn sans_face() -> &'static str {
    static F: std::sync::OnceLock<&str> = std::sync::OnceLock::new();
    F.get_or_init(|| if font_file("SegUIVar.ttf") { "Segoe UI Variable Text" } else { "Segoe UI" })
}

/// The UI font of the current style.
pub fn ui_face() -> &'static str {
    if style().mono() { "Consolas" } else { sans_face() }
}

/// The icon font: Segoe Fluent Icons (Windows 11, same code points) or Segoe MDL2 Assets.
pub fn icon_face() -> &'static str {
    static F: std::sync::OnceLock<&str> = std::sync::OnceLock::new();
    F.get_or_init(|| if font_file("SegoeIcons.ttf") { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opacity_text_and_junk_is_solid() {
        assert_eq!(parse_opacity("0"), 0);
        assert_eq!(parse_opacity(" 60%\r\n"), 60);
        assert_eq!(parse_opacity("250"), 100);
        assert_eq!(parse_opacity(""), 100);
        assert_eq!(parse_opacity("-5"), 100);
        assert_eq!(parse_opacity("clear"), 100);
    }

    #[test]
    fn names_round_trip_and_junk_is_normal() {
        for s in ALL {
            assert_eq!(Style::parse(s.name()), s);
        }
        assert_eq!(Style::parse(" Tiling\r\n"), Style::Tiling);
        assert_eq!(Style::parse(""), Style::Normal);
        assert_eq!(Style::parse("neon"), Style::Normal);
        assert_eq!(PALS.len(), ALL.len());
    }

    #[test]
    fn hash_is_stable_and_in_range() {
        assert_eq!(hash(3, 4, 1), hash(3, 4, 1));
        assert!((0..50).all(|i| (0.0..=1.0).contains(&hash(i, -i, 2))));
        assert_eq!(mix(rgb(0, 0, 0), rgb(200, 100, 50), 0.5), rgb(100, 50, 25));
    }
}
