//! Тема, шрифты и примитивы рисования (GDI) в стиле диспетчера задач Windows 11.
use std::ptr::null_mut;
use winapi::shared::windef::{HBRUSH, HDC, HFONT, HGDIOBJ, POINT, RECT};
use winapi::um::{wingdi::*, winuser::*};

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn c(self) -> u32 {
        self.0 as u32 | (self.1 as u32) << 8 | (self.2 as u32) << 16
    }
    pub fn lerp(self, o: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb(m(self.0, o.0), m(self.1, o.1), m(self.2, o.2))
    }
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    pub bg: Rgb,       // фон страницы и таблицы
    pub nav: Rgb,      // боковая панель
    pub panel: Rgb,    // плашки, поле поиска
    pub text: Rgb,
    pub dim: Rgb,      // приглушённый текст
    pub border: Rgb,
    pub sel: Rgb,      // выбранная строка
    pub hover: Rgb,
    pub accent: Rgb,
    pub heat0: Rgb, // «тепловая» подсветка ячеек: от малого значения
    pub heat1: Rgb, // … до большого
    pub warn: Rgb,
    pub bad: Rgb,
}

pub fn theme(dark: bool) -> Theme {
    if dark {
        Theme {
            dark,
            bg: Rgb(22, 22, 24),
            nav: Rgb(28, 28, 31),
            panel: Rgb(36, 36, 40),
            text: Rgb(245, 245, 247),
            dim: Rgb(142, 142, 147),
            border: Rgb(46, 46, 50),
            sel: Rgb(44, 44, 49),
            hover: Rgb(33, 33, 37),
            accent: Rgb(10, 132, 255),
            heat0: Rgb(16, 34, 60),
            heat1: Rgb(20, 98, 196),
            warn: Rgb(255, 214, 10),
            bad: Rgb(255, 69, 58),
        }
    } else {
        Theme {
            dark,
            bg: Rgb(251, 251, 253),
            nav: Rgb(242, 242, 247),
            panel: Rgb(255, 255, 255),
            text: Rgb(29, 29, 31),
            dim: Rgb(110, 110, 115),
            border: Rgb(229, 229, 234),
            sel: Rgb(229, 229, 234),
            hover: Rgb(238, 238, 242),
            accent: Rgb(0, 113, 227),
            heat0: Rgb(232, 241, 253),
            heat1: Rgb(102, 166, 240),
            warn: Rgb(178, 110, 0),
            bad: Rgb(215, 0, 21),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub struct R {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl R {
    pub fn new(l: i32, t: i32, r: i32, b: i32) -> R {
        R { l, t, r, b }
    }
    pub fn w(&self) -> i32 {
        self.r - self.l
    }
    pub fn h(&self) -> i32 {
        self.b - self.t
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
    pub fn inset(&self, d: i32) -> R {
        R::new(self.l + d, self.t + d, self.r - d, self.b - d)
    }
    pub fn rc(&self) -> RECT {
        RECT { left: self.l, top: self.t, right: self.r, bottom: self.b }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Font {
    Ui,       // основной текст
    Small,    // мелкий
    Semibold, // заголовки колонок/разделов
    Title,    // заголовок страницы
    Big,      // крупные числа
    Huge,     // заголовок ресурса на странице «Производительность»
    Icons,    // значки (Segoe Fluent Icons)
    IconsSm,
}

pub struct Fonts {
    f: [HFONT; 8],
}

fn mk(face: &str, px: i32, weight: i32) -> HFONT {
    let w: Vec<u16> = face.encode_utf16().chain(Some(0)).collect();
    unsafe { CreateFontW(-px, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, 0, 0, CLEARTYPE_QUALITY, 0, w.as_ptr()) }
}

/// Есть ли в системе шрифт значков Windows 11.
fn icon_face() -> &'static str {
    // Segoe Fluent Icons есть в Windows 11; в Windows 10 — Segoe MDL2 Assets (те же коды символов)
    winrust_common::style::icon_face()
}

impl Fonts {
    pub fn new(s: f64) -> Fonts {
        let p = |x: f64| (x * s).round() as i32;
        Fonts {
            f: [
                mk(winrust_common::style::sans_face(), p(13.0), 400),
                mk(winrust_common::style::sans_face(), p(12.0), 400),
                mk(winrust_common::style::sans_face(), p(13.0), 600),
                mk(winrust_common::style::sans_face(), p(22.0), 600),
                mk(winrust_common::style::sans_face(), p(20.0), 400),
                mk(winrust_common::style::sans_face(), p(30.0), 300),
                mk(icon_face(), p(16.0), 400),
                mk(icon_face(), p(13.0), 400),
            ],
        }
    }
    pub fn get(&self, f: Font) -> HFONT {
        self.f[f as usize]
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for f in self.f {
            unsafe { DeleteObject(f as HGDIOBJ) };
        }
    }
}

/// Контекст рисования: DC + масштаб + тема + шрифты.
pub struct G<'a> {
    pub dc: HDC,
    pub s: f64,
    pub th: Theme,
    pub fonts: &'a Fonts,
}

impl<'a> G<'a> {
    pub fn px(&self, v: f64) -> i32 {
        (v * self.s).round() as i32
    }

    pub fn fill(&self, r: R, c: Rgb) {
        unsafe {
            let b = CreateSolidBrush(c.c());
            FillRect(self.dc, &r.rc(), b);
            DeleteObject(b as HGDIOBJ);
        }
    }

    pub fn frame(&self, r: R, c: Rgb) {
        unsafe {
            let b = CreateSolidBrush(c.c());
            FrameRect(self.dc, &r.rc(), b);
            DeleteObject(b as HGDIOBJ);
        }
    }

    pub fn hline(&self, x0: i32, x1: i32, y: i32, c: Rgb) {
        self.fill(R::new(x0, y, x1, y + 1.max(self.px(1.0) / 2)), c)
    }

    pub fn vline(&self, x: i32, y0: i32, y1: i32, c: Rgb) {
        self.fill(R::new(x, y0, x + 1.max(self.px(1.0) / 2), y1), c)
    }

    pub fn round(&self, r: R, radius: i32, fill: Option<Rgb>, border: Option<Rgb>) {
        unsafe {
            let brush: HBRUSH = match fill {
                Some(c) => CreateSolidBrush(c.c()),
                None => GetStockObject(NULL_BRUSH as i32) as HBRUSH,
            };
            let pen = match border {
                Some(c) => CreatePen(PS_SOLID as i32, 1, c.c()),
                None => GetStockObject(NULL_PEN as i32) as _,
            };
            let (ob, op) = (SelectObject(self.dc, brush as HGDIOBJ), SelectObject(self.dc, pen as HGDIOBJ));
            RoundRect(self.dc, r.l, r.t, r.r + 1, r.b + 1, radius * 2, radius * 2);
            SelectObject(self.dc, ob);
            SelectObject(self.dc, op);
            if fill.is_some() {
                DeleteObject(brush as HGDIOBJ);
            }
            if border.is_some() {
                DeleteObject(pen as HGDIOBJ);
            }
        }
    }

    /// Текст в прямоугольнике. `flags` — DT_*; по умолчанию слева, по центру по вертикали, с многоточием.
    pub fn text_in(&self, r: R, s: &str, f: Font, c: Rgb, flags: u32) {
        if s.is_empty() {
            return;
        }
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let old = SelectObject(self.dc, self.fonts.get(f) as HGDIOBJ);
            SetBkMode(self.dc, TRANSPARENT as i32);
            SetTextColor(self.dc, c.c());
            let mut rc = r.rc();
            DrawTextW(self.dc, w.as_ptr(), w.len() as i32, &mut rc, flags | DT_NOPREFIX);
            SelectObject(self.dc, old);
        }
    }

    pub fn text(&self, r: R, s: &str, f: Font, c: Rgb) {
        self.text_in(r, s, f, c, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS)
    }

    pub fn text_right(&self, r: R, s: &str, f: Font, c: Rgb) {
        self.text_in(r, s, f, c, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS)
    }

    pub fn text_center(&self, r: R, s: &str, f: Font, c: Rgb) {
        self.text_in(r, s, f, c, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS)
    }

    /// Высота текста с переносом по словам при заданной ширине.
    pub fn wrap_h(&self, s: &str, f: Font, w: i32) -> i32 {
        let ws: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let old = SelectObject(self.dc, self.fonts.get(f) as HGDIOBJ);
            let mut rc = RECT { left: 0, top: 0, right: w.max(10), bottom: 0 };
            DrawTextW(self.dc, ws.as_ptr(), ws.len() as i32, &mut rc, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX);
            SelectObject(self.dc, old);
            rc.bottom
        }
    }

    pub fn text_wrapped(&self, r: R, s: &str, f: Font, c: Rgb) {
        self.text_in(r, s, f, c, DT_LEFT | DT_TOP | DT_WORDBREAK);
    }

    pub fn text_w(&self, s: &str, f: Font) -> i32 {
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let old = SelectObject(self.dc, self.fonts.get(f) as HGDIOBJ);
            let mut sz = std::mem::zeroed();
            GetTextExtentPoint32W(self.dc, w.as_ptr(), w.len() as i32, &mut sz);
            SelectObject(self.dc, old);
            sz.cx
        }
    }

    /// Символ из шрифта значков по центру прямоугольника.
    pub fn glyph(&self, r: R, ch: char, c: Rgb, small: bool) {
        let mut b = [0u8; 4];
        self.text_center(r, ch.encode_utf8(&mut b), if small { Font::IconsSm } else { Font::Icons }, c);
    }

    pub fn polyline(&self, pts: &[POINT], c: Rgb, width: i32) {
        if pts.len() < 2 {
            return;
        }
        unsafe {
            let pen = CreatePen(PS_SOLID as i32, width, c.c());
            let old = SelectObject(self.dc, pen as HGDIOBJ);
            Polyline(self.dc, pts.as_ptr(), pts.len() as i32);
            SelectObject(self.dc, old);
            DeleteObject(pen as HGDIOBJ);
        }
    }

    pub fn polygon(&self, pts: &[POINT], c: Rgb) {
        if pts.len() < 3 {
            return;
        }
        unsafe {
            let brush = CreateSolidBrush(c.c());
            let (ob, op) = (SelectObject(self.dc, brush as HGDIOBJ), SelectObject(self.dc, GetStockObject(NULL_PEN as i32)));
            Polygon(self.dc, pts.as_ptr(), pts.len() as i32);
            SelectObject(self.dc, ob);
            SelectObject(self.dc, op);
            DeleteObject(brush as HGDIOBJ);
        }
    }

    pub fn icon(&self, hicon: isize, x: i32, y: i32, size: i32) {
        if hicon != 0 {
            unsafe { DrawIconEx(self.dc, x, y, hicon as _, size, size, 0, null_mut(), 3 /*DI_NORMAL*/) };
        }
    }

    pub fn clip(&self, r: R) {
        unsafe {
            let rgn = CreateRectRgn(r.l, r.t, r.r, r.b);
            SelectClipRgn(self.dc, rgn);
            DeleteObject(rgn as HGDIOBJ);
        }
    }

    pub fn unclip(&self) {
        unsafe { SelectClipRgn(self.dc, null_mut()) };
    }
}

/// Цвет «тепловой» подсветки ячейки по доле от шкалы (корень — чтобы малые значения были заметны).
pub fn heat(th: &Theme, value: f64, scale: f64) -> Rgb {
    th.heat0.lerp(th.heat1, ((value / scale).max(0.0).sqrt()) as f32)
}

/// Человекочитаемые единицы.
pub fn fmt_mem(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    if mb >= 1000.0 { format!("{} MB", group_thousands(mb as u64)) } else if mb >= 100.0 { format!("{:.0} MB", mb) } else { format!("{:.1} MB", mb) }
}

pub fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}




pub fn fmt_uptime(secs: u64) -> String {
    format!("{}:{:02}:{:02}:{:02}", secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(group_thousands(3572), "3,572");
        assert_eq!(group_thousands(12), "12");
        assert_eq!(fmt_mem(3_746_000_000), "3,572 MB");
        assert_eq!(fmt_uptime(2 * 86400 + 18 * 3600 + 27 * 60 + 54), "2:18:27:54");
    }

    #[test]
    fn heat_is_monotonic_and_clamped() {
        let t = theme(true);
        assert_eq!(heat(&t, 0.0, 50.0), t.heat0);
        assert_eq!(heat(&t, 500.0, 50.0), t.heat1);
        let (a, b) = (heat(&t, 5.0, 50.0), heat(&t, 25.0, 50.0));
        assert!(b.2 > a.2);
    }

    #[test]
    fn lerp_endpoints() {
        assert_eq!(Rgb(0, 0, 0).lerp(Rgb(100, 200, 50), 0.0), Rgb(0, 0, 0));
        assert_eq!(Rgb(0, 0, 0).lerp(Rgb(100, 200, 50), 1.0), Rgb(100, 200, 50));
    }
}
