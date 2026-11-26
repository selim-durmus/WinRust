//! The window: drawing (GDI+ shapes, GDI text), layout, dialogs and the message loop handler.
use crate::*;

// ---------- drawing ----------
//
// One back buffer per paint: GDI+ for the anti-aliased shapes, then GDI (ClearType) for text on top.

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// COLORREF (0x00BBGGRR) + alpha -> GDI+ ARGB.
pub(crate) const fn argb(c: u32, a: u8) -> u32 {
    (a as u32) << 24 | (c & 255) << 16 | (c >> 8 & 255) << 8 | (c >> 16 & 255)
}

pub(crate) fn lerp(a: u32, b: u32, t: f32) -> u32 {
    let m = |sh: u32| (((a >> sh & 255) as f32) * (1.0 - t) + ((b >> sh & 255) as f32) * t) as u32;
    m(0) | m(8) << 8 | m(16) << 16
}

pub(crate) type Box4 = [f32; 4]; // left, top, right, bottom in logical px

pub(crate) struct Ui {
    dc: HDC,
    g: *mut GpGraphics,
    s: f32,
}
impl Ui {
    fn px(&self, v: f32) -> i32 {
        (v * self.s).round() as i32
    }
    unsafe fn shape(&self, b: Box4, rad: f32, color: u32) {
        let (s, mut path) = (self.s, null_mut::<GpPath>());
        let (x, y, w, h) = (b[0] * s, b[1] * s, (b[2] - b[0]) * s, (b[3] - b[1]) * s);
        let d = (rad * 2.0 * s).min(h).min(w).max(0.01);
        GdipCreatePath(FillModeAlternate, &mut path);
        GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
        GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
        GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
        GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
        GdipClosePathFigure(path);
        let mut br = null_mut::<GpSolidFill>();
        GdipCreateSolidFill(color, &mut br);
        GdipFillPath(self.g, br as *mut GpBrush, path);
        GdipDeleteBrush(br as *mut GpBrush);
        GdipDeletePath(path);
    }
    /// Rounded box with a 1 px outline (`line` == `fill` for none).
    unsafe fn round(&self, b: Box4, rad: f32, fill: u32, line: u32) {
        if line != fill {
            self.shape(b, rad, argb(line, 255));
            let i = 1.0 / self.s;
            self.shape([b[0] + i, b[1] + i, b[2] - i, b[3] - i], rad - i, argb(fill, 255));
        } else {
            self.shape(b, rad, argb(fill, 255));
        }
    }
    unsafe fn font(&self, size: f32, weight: i32, face: &str) -> HFONT {
        CreateFontW(-self.px(size), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, wide(face).as_ptr())
    }
    unsafe fn text_in(&self, s: &str, b: Box4, size: f32, weight: i32, color: u32, flags: u32, face: &str) {
        let f = self.font(size, weight, face);
        let of = SelectObject(self.dc, f);
        SetTextColor(self.dc, color);
        SetBkMode(self.dc, TRANSPARENT as i32);
        let mut r = RECT { left: self.px(b[0]), top: self.px(b[1]), right: self.px(b[2]), bottom: self.px(b[3]) };
        DrawTextW(self.dc, wide(s).as_ptr(), -1, &mut r, flags | DT_NOPREFIX);
        SelectObject(self.dc, of);
        DeleteObject(f);
    }
    unsafe fn text(&self, s: &str, b: Box4, size: f32, weight: i32, color: u32, flags: u32) {
        self.text_in(s, b, size, weight, color, flags, winrust_common::style::sans_face());
    }
    /// Width of a single line of text, logical px.
    unsafe fn width(&self, s: &str, size: f32, weight: i32) -> f32 {
        let f = self.font(size, weight, winrust_common::style::sans_face());
        let of = SelectObject(self.dc, f);
        let mut r: RECT = zeroed();
        DrawTextW(self.dc, wide(s).as_ptr(), -1, &mut r, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(self.dc, of);
        DeleteObject(f);
        r.right as f32 / self.s
    }
}

// layout, logical px
pub(crate) const WIN_W: f32 = 600.0;
pub(crate) const M: f32 = 24.0; // outer margin
pub(crate) const N_READY: usize = {
    let (mut n, mut i) = (0, 0);
    while i < PLUGINS.len() {
        if PLUGINS[i].actions.is_some() && PLUGINS[i].bulk {
            n += 1;
        }
        i += 1;
    }
    n
};
pub(crate) const LIST_TOP: f32 = 136.0;
pub(crate) const ROW_H: f32 = 62.0;
pub(crate) const LIST_H: f32 = N_READY as f32 * ROW_H + 12.0;
pub(crate) const MAIN_TOP: f32 = LIST_TOP + LIST_H + 20.0;
pub(crate) const MAIN_H: f32 = 54.0;
pub(crate) const FOOT_H: f32 = 72.0;
pub(crate) const WIN_H: f32 = MAIN_TOP + MAIN_H + 16.0 + FOOT_H;
pub(crate) const BTN_W: f32 = 136.0;
pub(crate) const BTN_H: f32 = 36.0;
// clickable things
pub(crate) const ID_MAIN: usize = 0;
pub(crate) const ID_ROLLBACK: usize = 1;
pub(crate) const ID_OK: usize = 2;
pub(crate) const ID_CANCEL: usize = 3;
pub(crate) const ID_SWITCH: usize = 4;
pub(crate) const ID_GEAR: usize = 5;
pub(crate) const ID_CHECK: usize = 100; // + plugin index: the row's checkbox
pub(crate) const STARTING: usize = usize::MAX - 1; // JOB before install_all picks its first component

/// Installable components, in install order (topbar comes before rust-explorer, which needs it).
pub(crate) fn ready() -> impl Iterator<Item = usize> {
    (0..PLUGINS.len()).filter(|&i| PLUGINS[i].actions.is_some() && PLUGINS[i].bulk)
}

/// The components the checkboxes have ticked, in install order.
pub(crate) fn chosen() -> impl Iterator<Item = usize> {
    ready().filter(|&i| SELECTED[i].load(Relaxed))
}

/// Checkbox at the left end of row `k`, and the larger area that toggles it.
pub(crate) fn check_box(k: usize, w: f32) -> (Box4, Box4) {
    let r = row_box(k, w);
    ([r[0] + 12.0, r[1] + 22.0, r[0] + 30.0, r[1] + 40.0], [r[0] + 4.0, r[1] + 8.0, r[0] + 38.0, r[3] - 8.0])
}

pub(crate) fn row_box(k: usize, w: f32) -> Box4 {
    let t = LIST_TOP + 6.0 + k as f32 * ROW_H;
    [M + 6.0, t, w - M - 6.0, t + ROW_H]
}

/// Settings gear at the right end of the row of the plugin that has settings (topbar): (plugin index, box).
pub(crate) fn gear_box(w: f32) -> Option<(usize, Box4)> {
    let (k, i) = ready().enumerate().find(|&(_, i)| PLUGINS[i].settings)?;
    let r = row_box(k, w);
    let t = r[1] + (ROW_H - 32.0) / 2.0;
    Some((i, [r[2] - 14.0 - 32.0, t, r[2] - 14.0, t + 32.0]))
}

pub(crate) fn main_box(w: f32) -> Box4 {
    [M, MAIN_TOP, w - M, MAIN_TOP + MAIN_H]
}

/// On/off switch inside the settings panel row.
pub(crate) fn switch_box(w: f32) -> Box4 {
    let (d, _, _) = dialog_boxes(w);
    [d[2] - 24.0 - 16.0 - 44.0, d[1] + 78.0 + 23.0, d[2] - 24.0 - 16.0, d[1] + 78.0 + 47.0]
}

pub(crate) fn rollback_box(w: f32) -> Box4 {
    let t = WIN_H - FOOT_H + (FOOT_H - BTN_H) / 2.0;
    [w - M - 150.0, t, w - M, t + BTN_H]
}

/// Dialog panel and its (cancel, ok) buttons.
pub(crate) fn dialog_boxes(w: f32) -> (Box4, Box4, Box4) {
    let (dw, dh) = (528.0, 352.0);
    let (l, t) = ((w - dw) / 2.0, (WIN_H - dh) / 2.0);
    let (bt, r) = (t + dh - 24.0 - BTN_H, l + dw - 24.0);
    ([l, t, l + dw, t + dh], [r - BTN_W - 10.0 - 120.0, bt, r - BTN_W - 10.0, bt + BTN_H], [r - BTN_W, bt, r, bt + BTN_H])
}

/// A button: `kind` 0 = primary (accent), 1 = secondary (outline), 2 = danger (outline, red text), 3 = disabled.
pub(crate) unsafe fn button(ui: &Ui, b: Box4, label: &str, kind: u8, hot: bool) {
    let (fill, line, fg) = match kind {
        0 => (ACCENT, ACCENT, rgb(255, 255, 255)),
        1 => (PANEL, BORDER, TEXT),
        2 => (PANEL, BORDER, BAD),
        _ => (PANEL, lerp(PANEL, BORDER, 0.6), lerp(DIM, PANEL, 0.35)),
    };
    let fill = if hot && kind != 3 { lerp(fill, TEXT, if kind == 0 { 0.14 } else { 0.07 }) } else { fill };
    let line = if hot && kind == 2 { BAD } else if kind == 0 { fill } else { line };
    ui.round(b, 8.0, fill, line);
    ui.text(label, b, 13.5, 600, fg, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
}

pub(crate) unsafe fn paint(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    let (cw, ch) = (rc.right, rc.bottom);
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, cw, ch);
    let ob = SelectObject(mem, bmp);
    let s = GetDeviceCaps(hdc, LOGPIXELSX as i32) as f32 / 96.0;
    let bgb = CreateSolidBrush(BG);
    FillRect(mem, &rc, bgb);
    DeleteObject(bgb);
    let mut g = null_mut::<GpGraphics>();
    GdipCreateFromHDC(mem, &mut g);
    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    let ui = Ui { dc: mem, g, s };
    let w = cw as f32 / s;
    let hover = HOVER.get();
    let dialog = DIALOG.with_borrow(|d| d.as_ref().map(|d| (d.title.clone(), d.text.clone(), d.ok.clone(), d.danger)));
    let hot = |id: usize| hover == Some(id);
    let busy = BUSY.load(Relaxed);
    let job = JOB.load(Relaxed);
    let n = PLUGINS.len();
    let n_inst = ready().filter(|&i| INSTALLED[i].load(Relaxed)).count();

    // header: logo + name
    ui.shape([0.0, 0.0, w, 3.0], 0.0, argb(ACCENT, 255));
    let logo = [M, 30.0, M + 60.0, 90.0];
    ui.round(logo, 16.0, ACCENT, ACCENT);
    let inner = lerp(ACCENT, BG, 0.12);
    ui.round([logo[0] + 4.0, logo[1] + 4.0, logo[2] - 4.0, logo[3] - 4.0], 13.0, inner, inner);
    ui.text("W", [logo[0], logo[1] - 2.0, logo[2], logo[3]], 32.0, 900, rgb(255, 255, 255), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    let tx = logo[2] + 18.0;
    ui.text("WinRust", [tx, 28.0, w - M, 64.0], 30.0, 700, TEXT, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    ui.text(&format!("A Windows shell in Rust  \u{00B7}  {n_inst} of {N_READY} installed"), [tx, 64.0, w - M, 88.0], 13.0, 400, DIM, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    ui.text("INCLUDED", [M + 4.0, LIST_TOP - 26.0, w - M, LIST_TOP - 6.0], 11.0, 700, DIM, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    // component list
    ui.round([M, LIST_TOP, w - M, LIST_TOP + LIST_H], 12.0, PANEL, BORDER);
    let pos = chosen().position(|i| i == job); // where install_all is now
    let n_sel = chosen().count();
    let n_sel_inst = chosen().filter(|&i| INSTALLED[i].load(Relaxed)).count();
    let gear = gear_box(w);
    for (k, i) in ready().enumerate() {
        let p = &PLUGINS[i];
        let r = row_box(k, w);
        let installed = INSTALLED[i].load(Relaxed);
        let working = busy && (job == i || (job == n && installed));
        let queued = busy && !installed && SELECTED[i].load(Relaxed) && (job == STARTING || ready().position(|j| j == job).is_some_and(|q| k > q));
        if k > 0 {
            ui.shape([r[0] + 12.0, r[1], r[2] - 12.0, r[1] + 1.0 / s], 0.0, argb(BORDER, 255));
        }
        if working {
            let t = lerp(PANEL, ACCENT, 0.10);
            ui.round([r[0], r[1] + 3.0, r[2], r[3] - 3.0], 9.0, t, t);
        }
        let (cb, _) = check_box(k, w);
        let sel = SELECTED[i].load(Relaxed);
        ui.round(cb, 5.0, if sel { ACCENT } else { PANEL }, if sel { ACCENT } else { lerp(BORDER, TEXT, if hot(ID_CHECK + i) { 0.4 } else { 0.0 }) });
        if sel {
            ui.text_in("\u{E73E}", cb, 11.0, 700, rgb(255, 255, 255), DT_CENTER | DT_VCENTER | DT_SINGLELINE, winrust_common::style::icon_face());
        }
        let tile = [r[0] + 42.0, r[1] + 13.0, r[0] + 78.0, r[1] + 49.0];
        let lit = installed || working;
        let tint = if lit { lerp(PANEL, ACCENT, 0.20) } else { lerp(PANEL, TEXT, 0.06) };
        ui.round(tile, 9.0, tint, tint);
        ui.text_in(p.glyph, tile, 16.0, 400, if lit { ACCENT } else { TEXT }, DT_CENTER | DT_VCENTER | DT_SINGLELINE, winrust_common::style::icon_face());

        let (pill, pc) = if working {
            (if job == n { "Removing…" } else { "Installing…" }, ACCENT)
        } else if installed {
            ("Installed", GOOD)
        } else if queued {
            ("Queued", DIM)
        } else {
            ("Not installed", DIM)
        };
        let pr = r[2] - 14.0 - 32.0 - 10.0;
        let pw = ui.width(pill, 11.5, 600) + 18.0;
        let pb = [pr - pw, r[1] + 21.0, pr, r[1] + 41.0];
        let ptint = lerp(PANEL, pc, 0.16);
        ui.round(pb, 10.0, ptint, ptint);
        ui.text(pill, pb, 11.5, 600, pc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

        let nx = tile[2] + 14.0;
        ui.text(p.name, [nx, r[1] + 11.0, pb[0] - 10.0, r[1] + 32.0], 14.5, 600, TEXT, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        let desc = p.desc;
        let short = desc.split(". ").next().unwrap_or(desc).trim_end_matches('.');
        ui.text(short, [nx, r[1] + 32.0, pb[0] - 10.0, r[3] - 10.0], 12.0, 400, DIM, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);

        if let Some((_, gb)) = gear.filter(|&(gi, _)| gi == i && installed) {
            let h = hot(ID_GEAR) && !busy;
            ui.round(gb, 8.0, if h { lerp(PANEL, TEXT, 0.08) } else { PANEL }, BORDER);
            ui.text_in("\u{E713}", gb, 14.0, 400, if busy { DIM } else { TEXT }, DT_CENTER | DT_VCENTER | DT_SINGLELINE, winrust_common::style::icon_face());
        }
    }

    // the one button: installs everything that is missing (or updates all when nothing is)
    let mb = main_box(w);
    if busy {
        let base = lerp(PANEL, ACCENT, 0.22);
        ui.round(mb, 12.0, base, base);
        let done = if job == n { 0.0 } else { pos.map_or(0.0, |q| (q as f32 + 0.5) / n_sel as f32) };
        if done > 0.0 {
            ui.shape([mb[0], mb[1], mb[0] + (mb[2] - mb[0]) * done, mb[3]], 12.0, argb(ACCENT, 255));
        }
        let label = if job == n {
            "Removing WinRust…".to_string()
        } else {
            pos.map_or("Preparing…".into(), |q| format!("Installing {}  ({} of {})", PLUGINS[job].name, q + 1, n_sel))
        };
        ui.text(&label, mb, 15.5, 700, TEXT, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    } else {
        let all = n_sel_inst == n_sel && n_sel > 0;
        let label = if n_sel == 0 {
            "Tick the components".to_string()
        } else if n_sel < N_READY {
            if all { format!("Update selected  ({n_sel})") } else { format!("Install selected  ({n_sel})") }
        } else if all {
            "Update WinRust".to_string()
        } else if n_inst == 0 {
            "Install WinRust".into()
        } else {
            format!("Install and update WinRust  ({} new)", N_READY - n_inst)
        };
        let (fill, fg) = if all || n_sel == 0 { (PANEL, if n_sel == 0 { DIM } else { TEXT }) } else { (ACCENT, rgb(255, 255, 255)) };
        let fill = if hot(ID_MAIN) { lerp(fill, TEXT, if all { 0.07 } else { 0.14 }) } else { fill };
        ui.round(mb, 12.0, fill, if all { BORDER } else { fill });
        let tw = ui.width(&label, 16.0, 700);
        let cx = (mb[0] + mb[2]) / 2.0 + 13.0;
        ui.text_in(if all { "\u{E72C}" } else { "\u{E896}" }, [cx - tw / 2.0 - 30.0, mb[1], cx - tw / 2.0 - 6.0, mb[3]], 15.0, 400, fg, DT_CENTER | DT_VCENTER | DT_SINGLELINE, winrust_common::style::icon_face());
        ui.text(&label, [cx - tw / 2.0, mb[1], cx + tw / 2.0 + 4.0, mb[3]], 16.0, 700, fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    }

    // footer: status on the left, remove-all on the right
    let ft = WIN_H - FOOT_H;
    ui.shape([M, ft, w - M, ft + 1.0 / s], 0.0, argb(BORDER, 255));
    let (m, ok) = MSG.lock().unwrap().clone();
    let (m, dot, fg) = if busy {
        ("Working, do not close the window…".to_string(), ACCENT, DIM)
    } else if m.is_empty() {
        ("A restore point is created before installing.".into(), DIM, DIM)
    } else if ok {
        (m, GOOD, TEXT)
    } else {
        (m, BAD, BAD)
    };
    let rb = rollback_box(w);
    ui.shape([M, ft + FOOT_H / 2.0 - 4.0, M + 8.0, ft + FOOT_H / 2.0 + 4.0], 4.0, argb(dot, 255));
    let one_line = ui.width(&m, 12.5, 400) < rb[0] - 16.0 - M - 18.0;
    let tb = [M + 18.0, ft + if one_line { 0.0 } else { 12.0 }, rb[0] - 16.0, WIN_H - if one_line { 0.0 } else { 8.0 }];
    ui.text(&m, tb, 12.5, 400, fg, DT_LEFT | DT_END_ELLIPSIS | if one_line { DT_SINGLELINE | DT_VCENTER } else { DT_WORDBREAK });
    button(&ui, rb, "Remove all", if n_inst > 0 && !busy { 2 } else { 3 }, hot(ID_ROLLBACK));

    // confirmation dialog over the dimmed window
    if let Some((title, text, ok_label, danger)) = dialog {
        ui.shape([0.0, 0.0, w, WIN_H + 1.0], 0.0, argb(0, 160));
        let (d, cancel, okb) = dialog_boxes(w);
        ui.round(d, 14.0, lerp(PANEL, BG, 0.25), BORDER);
        let accent = if danger { BAD } else { ACCENT };
        let icon = [d[0] + 24.0, d[1] + 24.0, d[0] + 60.0, d[1] + 60.0];
        let tint = lerp(PANEL, accent, 0.18);
        ui.round(icon, 18.0, tint, tint);
        ui.text_in(if danger { "\u{E7BA}" } else { "\u{E946}" }, icon, 16.0, 400, accent, DT_CENTER | DT_VCENTER | DT_SINGLELINE, winrust_common::style::icon_face());
        ui.text(&title, [icon[2] + 14.0, d[1] + 24.0, d[2] - 24.0, d[1] + 60.0], 17.0, 600, TEXT, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        ui.text(&text, [d[0] + 24.0, d[1] + 76.0, d[2] - 24.0, cancel[1] - 12.0], 12.5, 400, DIM, DT_LEFT | DT_WORDBREAK | DT_END_ELLIPSIS);
        button(&ui, cancel, "Cancel", 1, hot(ID_CANCEL));
        let okfill = if hot(ID_OK) { lerp(accent, TEXT, 0.14) } else { accent };
        ui.round(okb, 8.0, okfill, okfill);
        ui.text(&ok_label, okb, 13.5, 600, rgb(255, 255, 255), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }

    // settings panel of a plugin (topbar: OneDrive switch)
    if let Some(i) = SETTINGS.get() {
        ui.shape([0.0, 0.0, w, WIN_H + 1.0], 0.0, argb(0, 160));
        let (d, _, closeb) = dialog_boxes(w);
        ui.round(d, 14.0, lerp(PANEL, BG, 0.25), BORDER);
        let icon = [d[0] + 24.0, d[1] + 24.0, d[0] + 60.0, d[1] + 60.0];
        let tint = lerp(PANEL, ACCENT, 0.18);
        ui.round(icon, 18.0, tint, tint);
        ui.text_in("\u{E713}", icon, 16.0, 400, ACCENT, DT_CENTER | DT_VCENTER | DT_SINGLELINE, winrust_common::style::icon_face());
        ui.text(&format!("{} settings", PLUGINS[i].name), [icon[2] + 14.0, d[1] + 24.0, d[2] - 24.0, d[1] + 60.0], 17.0, 600, TEXT, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        let row = [d[0] + 24.0, d[1] + 78.0, d[2] - 24.0, d[1] + 148.0];
        let hid = od_hidden();
        ui.round(row, 10.0, PANEL, BORDER);
        ui.text("Disable OneDrive", [row[0] + 16.0, row[1] + 10.0, row[2] - 80.0, row[1] + 32.0], 14.0, 600, TEXT, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        ui.text("The OneDrive icon is not shown in the bar (default). Sync keeps working.", [row[0] + 16.0, row[1] + 32.0, row[2] - 80.0, row[3] - 6.0], 12.0, 400, DIM, DT_LEFT | DT_WORDBREAK);
        let sw = switch_box(w);
        let off = lerp(PANEL, TEXT, 0.25);
        let track = if hid { ACCENT } else { off };
        ui.round(sw, 12.0, track, track);
        let kx = if hid { sw[2] - 12.0 } else { sw[0] + 12.0 };
        ui.round([kx - 8.0, sw[1] + 4.0, kx + 8.0, sw[3] - 4.0], 8.0, TEXT, TEXT);
        button(&ui, closeb, "Close", 1, hot(ID_CANCEL));
    }

    GdipDeleteGraphics(g);
    BitBlt(hdc, 0, 0, cw, ch, mem, 0, 0, SRCCOPY);
    SelectObject(mem, ob);
    DeleteObject(bmp);
    DeleteDC(mem);
    EndPaint(hwnd, &ps);
}

// ---------- window ----------

/// What a confirmed dialog runs.
#[derive(Clone, Copy)]
pub(crate) enum Act {
    InstallAll,
    Rollback,
}
pub(crate) struct Dialog {
    title: String,
    text: String,
    ok: String,
    danger: bool,
    act: Act,
}
thread_local! { pub(crate) static DIALOG: std::cell::RefCell<Option<Dialog>> = const { std::cell::RefCell::new(None) }; }

/// Clickable thing under the cursor (ID_*). While a panel is up only its buttons count; while a job runs nothing does.
pub(crate) unsafe fn hit(hwnd: HWND, lp: isize) -> Option<usize> {
    let (x, y) = ((lp & 0xFFFF) as i16 as f32, ((lp >> 16) & 0xFFFF) as i16 as f32);
    let (mut rc, dc): (RECT, _) = (zeroed(), GetDC(hwnd));
    let s = GetDeviceCaps(dc, LOGPIXELSX as i32) as f32 / 96.0;
    ReleaseDC(hwnd, dc);
    GetClientRect(hwnd, &mut rc);
    let w = rc.right as f32 / s;
    let inside = |b: Box4| x >= b[0] * s && x <= b[2] * s && y >= b[1] * s && y <= b[3] * s;
    if SETTINGS.get().is_some() {
        let (_, _, closeb) = dialog_boxes(w);
        return if inside(switch_box(w)) { Some(ID_SWITCH) } else if inside(closeb) { Some(ID_CANCEL) } else { None };
    }
    if DIALOG.with_borrow(|d| d.is_some()) {
        let (_, cancel, ok) = dialog_boxes(w);
        return if inside(ok) { Some(ID_OK) } else if inside(cancel) { Some(ID_CANCEL) } else { None };
    }
    if BUSY.load(Relaxed) {
        return None;
    }
    if gear_box(w).is_some_and(|(i, b)| INSTALLED[i].load(Relaxed) && inside(b)) {
        return Some(ID_GEAR);
    }
    if let Some(i) = ready().enumerate().find_map(|(k, i)| inside(check_box(k, w).1).then_some(i)) {
        return Some(ID_CHECK + i);
    }
    if inside(main_box(w)) {
        return Some(ID_MAIN);
    }
    (INSTALLED.iter().any(|f| f.load(Relaxed)) && inside(rollback_box(w))).then_some(ID_ROLLBACK)
}

/// Runs `job` off the UI thread; its Ok/Err text becomes the status line. `which` = plugin index, n = rollback.
pub(crate) fn start(hwnd: HWND, which: usize, job: impl FnOnce() -> Result<String, String> + Send + 'static) {
    JOB.store(which, Relaxed);
    BUSY.store(true, Relaxed);
    HOVER.set(None);
    unsafe { InvalidateRect(hwnd, null(), 0) };
    let h = hwnd as isize;
    thread::spawn(move || {
        *MSG.lock().unwrap() = match job() {
            Ok(m) => (m, true),
            Err(e) => (format!("Error: {e}"), false),
        };
        refresh();
        BUSY.store(false, Relaxed);
        unsafe { PostMessageW(h as HWND, WM_DONE, 0, 0) };
    });
}

/// One click: every component in order; missing ones are installed, installed ones updated from this installer.
/// Only the ticked ones. A failed or skipped one does not stop the others (rust-explorer refuses by itself while topbar is
/// missing); the failures are listed at the end. `h` = window to repaint between steps.
pub(crate) fn install_all(h: isize) -> Result<String, String> {
    let all = chosen().all(|i| (PLUGINS[i].installed)());
    let (mut done, mut failed, mut warned) = (Vec::new(), Vec::new(), false);
    for i in chosen() {
        // installed ones are updated too: a newer installer must bring its fixes to them, not only add what is missing
        JOB.store(i, Relaxed);
        unsafe { InvalidateRect(h as HWND, null(), 0) };
        if let Err(e) = install_plugin(i) {
            failed.push(format!("{}: {e}", PLUGINS[i].name));
            continue;
        }
        warned |= !WARN.lock().unwrap().is_empty(); // install_plugin clears the list for each component
        done.push(PLUGINS[i].name);
        refresh();
    }
    if !failed.is_empty() {
        let ok = if done.is_empty() { String::new() } else { format!(" Done: {}.", done.join(", ")) };
        return Err(format!("{}.{ok}", failed.join("; ")));
    }
    let note = if warned { " There are warnings (see %ProgramData%\\plugin-launcher\\install.log)." } else { "" };
    Ok(format!("WinRust {}: {}.{note}", if all { "updated" } else { "installed" }, done.join(", ")))
}

pub(crate) unsafe fn ask(hwnd: HWND, d: Dialog) {
    DIALOG.set(Some(d));
    HOVER.set(None);
    InvalidateRect(hwnd, null(), 0);
}

pub(crate) unsafe fn close_dialog(hwnd: HWND, run: bool) {
    let Some(d) = DIALOG.take() else { return };
    HOVER.set(None);
    InvalidateRect(hwnd, null(), 0);
    if !run {
        return;
    }
    match d.act {
        Act::Rollback => start(hwnd, PLUGINS.len(), rollback),
        Act::InstallAll => {
            let h = hwnd as isize;
            start(hwnd, STARTING, move || install_all(h))
        }
    }
}

pub(crate) unsafe fn click(hwnd: HWND, id: usize) {
    if SETTINGS.get().is_some() {
        if id == ID_SWITCH {
            if let Err(e) = set_od_hidden(!od_hidden()) {
                *MSG.lock().unwrap() = (format!("Error: {e}"), false);
            }
        } else if id == ID_CANCEL {
            SETTINGS.set(None);
        }
        HOVER.set(None);
        InvalidateRect(hwnd, null(), 0);
        return;
    }
    if id == ID_OK || id == ID_CANCEL {
        return close_dialog(hwnd, id == ID_OK);
    }
    if BUSY.load(Relaxed) {
        return;
    }
    match id {
        i if i >= ID_CHECK => {
            SELECTED[i - ID_CHECK].fetch_xor(true, Relaxed);
            InvalidateRect(hwnd, null(), 0);
        }
        ID_MAIN if chosen().next().is_none() => {}
        ID_GEAR => {
            SETTINGS.set(PLUGINS.iter().position(|p| p.settings));
            HOVER.set(None);
            InvalidateRect(hwnd, null(), 0);
        }
        ID_ROLLBACK => ask(hwnd, Dialog {
            title: "Remove WinRust?".into(),
            text: "All components will be removed. Windows Search, the stock Task Manager, the taskbar and regular Explorer come back.".into(),
            ok: "Remove all".into(),
            danger: true,
            act: Act::Rollback,
        }),
        ID_MAIN if chosen().all(|i| INSTALLED[i].load(Relaxed)) => ask(hwnd, Dialog {
            title: "Update WinRust?".into(),
            text: "All components will be reinstalled from this version of the installer. Settings are kept.".into(),
            ok: "Update".into(),
            danger: false,
            act: Act::InstallAll,
        }),
        ID_MAIN if chosen().count() < N_READY => ask(hwnd, Dialog {
            title: "Install the selected?".into(),
            text: format!("To be installed or updated: {}.\n\nThe components change the system (see each one's description). A restore point is created, 'Remove all' restores the system.", chosen().map(|i| PLUGINS[i].name).collect::<Vec<_>>().join(", ")).into(),
            ok: "Install".into(),
            danger: false,
            act: Act::InstallAll,
        }),
        ID_MAIN => ask(hwnd, Dialog {
            title: "Install WinRust?".into(),
            text: "Windows components will be replaced:\n\u{2022} Windows Search is disabled, rust-search (Alt+Space) takes its place;\n\u{2022} Task Manager is replaced by rtaskmgr;\n\u{2022} the taskbar is replaced by topbar (at the bottom of the screen);\n\u{2022} Explorer as the shell is disabled: rust-explorer opens folders and the desktop, telemetry services are disabled.\n\nA restore point is created, 'Remove all' restores the system. If the screen stays empty: Ctrl+Alt+Del \u{2192} Task Manager \u{2192} Run explorer.exe.".into(),
            ok: "Install".into(),
            danger: false,
            act: Act::InstallAll,
        }),
        _ => {}
    }
}

pub(crate) unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => paint(hwnd),
        WM_ERASEBKGND => return 1,
        WM_MOUSEMOVE => {
            let h = hit(hwnd, lp);
            if h != HOVER.get() {
                HOVER.set(h);
                InvalidateRect(hwnd, null(), 0);
            }
            let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
            TrackMouseEvent(&mut t);
        }
        WM_SETCURSOR if (lp & 0xFFFF) as u32 == HTCLIENT => {
            SetCursor(LoadCursorW(null_mut(), if HOVER.get().is_some() { IDC_HAND } else { IDC_ARROW }));
            return 1;
        }
        WM_MOUSELEAVE => {
            HOVER.set(None);
            InvalidateRect(hwnd, null(), 0);
        }
        WM_LBUTTONUP => {
            if let Some(i) = hit(hwnd, lp) {
                click(hwnd, i);
            }
        }
        WM_KEYDOWN if SETTINGS.get().is_some() && wp as u16 == VK_ESCAPE => {
            SETTINGS.set(None);
            InvalidateRect(hwnd, null(), 0);
        }
        WM_KEYDOWN if DIALOG.with_borrow(|d| d.is_some()) => match wp as u16 {
            VK_ESCAPE => close_dialog(hwnd, false),
            VK_RETURN if DIALOG.with_borrow(|d| d.as_ref().is_some_and(|d| !d.danger)) => close_dialog(hwnd, true), // a dangerous one needs the click

            _ => {}
        },
        WM_DONE => {
            InvalidateRect(hwnd, null(), 0);
        }
        WM_CLOSE if BUSY.load(Relaxed) => {} // closing mid-install would leave the system half-changed
        WM_DESTROY => PostQuitMessage(0),
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}
