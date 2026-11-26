//! Pictures for the icon view. Each file's picture is asked from the shell once, on a background thread, at one of three
//! sizes (64, 128 or 256 px), kept as a GDI+ bitmap and drawn scaled to whatever size the zoom asks for. Zooming therefore
//! never reloads anything and never flashes: a picture that is too small for the new size is drawn scaled up until the
//! sharper one arrives. Keyed by path, so filtering and sorting keep them; a new folder drops them.
#![allow(unsafe_op_in_unsafe_fn)]
use std::{cell::{Cell, RefCell}, collections::HashMap, ffi::c_void, os::windows::ffi::OsStrExt, path::{Path, PathBuf}, sync::{OnceLock, atomic::{AtomicU32, Ordering::Relaxed}, mpsc}};
use windows_sys::Win32::Graphics::{Gdi::*, GdiPlus::{GdipCreateBitmapFromScan0, GdipCreateFromHDC, GdipDeleteGraphics, GdipDisposeImage, GdipDrawImageRectI, GdipSetInterpolationMode, GdipSetPixelOffsetMode, GpBitmap, GpGraphics, GpImage, InterpolationModeHighQualityBicubic, PixelOffsetModeHalf}};

pub const WM_PIC: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 9; // worker -> UI: wp = Box<Done>
const PARGB: i32 = 0xE200B; // PixelFormat32bppPARGB
const BUDGET: usize = 160 << 20; // bytes of pictures kept; the least recently drawn go first

/// A GDI+ bitmap over pixels we own (GDI+ only points at them, so they live as long as it does). Dropping it frees both.
pub struct Image { bmp: isize, w: i32, h: i32, _px: Box<[u8]> }
unsafe impl Send for Image {}
impl Drop for Image {
    fn drop(&mut self) { unsafe { GdipDisposeImage(self.bmp as *mut GpImage) }; } // before the pixels go (field order)
}

/// A finished picture on its way to the UI thread.
pub struct Done { era: u32, path: PathBuf, size: i32, img: Option<Image> }

struct Pic { img: Image, size: i32, used: u64 }
enum Slot { Asked, Ready(Pic, Option<i32>), Failed } // Ready(.., a sharper size asked for)

static ERA: AtomicU32 = AtomicU32::new(0); // bumped for each new folder: older answers are dropped
static TX: OnceLock<mpsc::Sender<(u32, PathBuf, i32)>> = OnceLock::new();
thread_local! {
    static CACHE: RefCell<HashMap<PathBuf, Slot>> = RefCell::new(HashMap::new());
    static BYTES: Cell<usize> = const { Cell::new(0) };
    static TICK: Cell<u64> = const { Cell::new(0) };
}

/// The size to ask the shell for, for a picture drawn `px` big.
fn bucket(px: i32) -> i32 { if px <= 64 { 64 } else if px <= 128 { 128 } else { 256 } }

fn ask(p: &Path, size: i32) {
    if let Some(tx) = TX.get() { let _ = tx.send((ERA.load(Relaxed), p.to_path_buf(), size)); }
}

fn bytes(i: &Image) -> usize { (i.w * i.h * 4) as usize }

/// The picture of `p` for drawing `px` big: (GDI+ bitmap, width, height), asked for (or a sharper one) when missing.
/// None = not here yet, or the shell has none. The bitmap stays valid until the next WM_PIC or clear().
pub fn get(p: &Path, px: i32) -> Option<(isize, i32, i32)> {
    let want = bucket(px);
    let t = TICK.get() + 1;
    TICK.set(t);
    CACHE.with_borrow_mut(|c| match c.get_mut(p) {
        None => { c.insert(p.to_path_buf(), Slot::Asked); ask(p, want); None }
        Some(Slot::Asked) | Some(Slot::Failed) => None,
        Some(Slot::Ready(pic, more)) => {
            pic.used = t;
            if pic.size < want && more.is_none_or(|m| m < want) { *more = Some(want); ask(p, want); }
            Some((pic.img.bmp, pic.img.w, pic.img.h))
        }
    })
}

/// A new folder: every picture goes.
pub fn clear() {
    ERA.fetch_add(1, Relaxed);
    CACHE.with_borrow_mut(|c| c.clear());
    BYTES.set(0);
}

/// A picture arrived (WM_PIC): kept, unless it belongs to a folder no longer shown. true = repaint.
pub unsafe fn store(wp: usize) -> bool {
    let d = *Box::from_raw(wp as *mut Done);
    if d.era != ERA.load(Relaxed) { return false; }
    let t = TICK.get();
    CACHE.with_borrow_mut(|c| {
        let Some(img) = d.img else {
            // none from the shell; a sharper one that failed keeps the one we have (and is not asked for again)
            match c.get_mut(&d.path) { Some(Slot::Ready(_, more)) => *more = Some(i32::MAX), _ => { c.insert(d.path, Slot::Failed); } }
            return;
        };
        BYTES.set(BYTES.get() + bytes(&img));
        if let Some(Slot::Ready(old, _)) = c.insert(d.path, Slot::Ready(Pic { img, size: d.size, used: t }, None)) {
            BYTES.set(BYTES.get().saturating_sub(bytes(&old.img)));
        }
        if BYTES.get() > BUDGET {
            // over budget: the half drawn longest ago goes
            let mut ages: Vec<(u64, PathBuf)> = c.iter().filter_map(|(k, s)| if let Slot::Ready(p, _) = s { Some((p.used, k.clone())) } else { None }).collect();
            ages.sort();
            for (_, k) in ages.iter().take(ages.len() / 2) {
                if let Some(Slot::Ready(p, _)) = c.remove(k) { BYTES.set(BYTES.get().saturating_sub(bytes(&p.img))); }
            }
        }
    });
    true
}

/// The background thread that asks the shell. GDI+ must be started before.
pub fn start(main: isize) {
    use windows::{Win32::{Foundation::{HWND, LPARAM, SIZE, WPARAM}, System::Com::{COINIT_MULTITHREADED, CoInitializeEx}, UI::{Shell::*, WindowsAndMessaging::PostMessageW}}, core::PCWSTR};
    let (tx, rx) = mpsc::channel::<(u32, PathBuf, i32)>();
    let _ = TX.set(tx);
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        for (era, path, size) in rx {
            if era != ERA.load(Relaxed) { continue; } // the folder changed meanwhile
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
            let mut img = None;
            if let Ok(f) = SHCreateItemFromParsingName::<_, _, IShellItemImageFactory>(PCWSTR(wide.as_ptr()), None) {
                // scaled by the shell to fit (a bigger one would not fit the cell)
                if let Ok(hb) = f.GetImage(SIZE { cx: size, cy: size }, SIIGBF_RESIZETOFIT) {
                    img = to_gdiplus(hb.0 as HBITMAP);
                    DeleteObject(hb.0 as HGDIOBJ);
                }
            }
            let d = Box::new(Done { era, path, size, img });
            let _ = PostMessageW(Some(HWND(main as *mut c_void)), WM_PIC, WPARAM(Box::into_raw(d) as usize), LPARAM(0));
        }
    });
}

/// A GDI+ picture from a 32-bit DIB of the shell, on a copy of its pixels (the DIB is deleted right after). Icons come
/// premultiplied with alpha; photos have no alpha at all (all zero) and are made opaque.
unsafe fn to_gdiplus(hb: HBITMAP) -> Option<Image> {
    let mut bm: BITMAP = std::mem::zeroed();
    if GetObjectW(hb as HGDIOBJ, size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut c_void) == 0 || bm.bmWidth <= 0 || bm.bmHeight == 0 { return None; }
    let (w, h) = (bm.bmWidth as usize, bm.bmHeight.unsigned_abs() as usize);
    // GDI hands the pixels over top row first (negative height asked for), whichever way the shell stored them: thumbnails
    // and plain icons come in opposite row orders with the same header, so reading the memory directly turned some upside down
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w as i32, biHeight: -(h as i32), biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..std::mem::zeroed() };
    let mut px = vec![0u8; w * h * 4].into_boxed_slice();
    let dc = CreateCompatibleDC(std::ptr::null_mut());
    let rows = GetDIBits(dc, hb, 0, h as u32, px.as_mut_ptr() as *mut c_void, &mut bi, DIB_RGB_COLORS);
    DeleteDC(dc);
    if rows <= 0 { return None; }
    if !px.chunks_exact(4).any(|p| p[3] != 0) { px.chunks_exact_mut(4).for_each(|p| p[3] = 255); } // photos: no alpha at all
    let mut bmp: *mut GpBitmap = std::ptr::null_mut();
    if GdipCreateBitmapFromScan0(w as i32, h as i32, (w * 4) as i32, PARGB, px.as_mut_ptr(), &mut bmp) != 0 || bmp.is_null() { return None; }
    Some(Image { bmp: bmp as isize, w: w as i32, h: h as i32, _px: px })
}

/// Draws picture `bmp` (w x h) fitted into the square `box_px` at (x, y), centred, smoothly scaled.
pub unsafe fn draw(dc: HDC, (bmp, w, h): (isize, i32, i32), x: i32, y: i32, box_px: i32) {
    let k = (box_px as f32 / w.max(h).max(1) as f32).min(if w.max(h) < box_px / 2 { 1.0 } else { f32::MAX }); // tiny icons stay sharp
    let (dw, dh) = (((w as f32) * k).round() as i32, ((h as f32) * k).round() as i32);
    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if GdipCreateFromHDC(dc, &mut g) != 0 || g.is_null() { return; }
    GdipSetInterpolationMode(g, InterpolationModeHighQualityBicubic);
    GdipSetPixelOffsetMode(g, PixelOffsetModeHalf);
    GdipDrawImageRectI(g, bmp as *mut GpImage, x + (box_px - dw) / 2, y + (box_px - dh) / 2, dw, dh);
    GdipDeleteGraphics(g);
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes_asked_for() {
        assert_eq!(super::bucket(40), 64);
        assert_eq!(super::bucket(96), 128);
        assert_eq!(super::bucket(144), 256);
        assert_eq!(super::bucket(480), 256);
    }
}
