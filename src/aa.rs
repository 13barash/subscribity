//! Anti-aliased GDI shapes without GDI+ (the tray never loads extra DLLs): the shape is drawn at
//! 4x into a memory DIB pre-filled with the colour behind it, then StretchBlt'ed down with
//! HALFTONE (an area average), so edges blend with the background. Works in horizontal bands so
//! big shapes never need a big bitmap.
use std::ffi::c_void;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::*;

/// Supersampling factor.
const K: i32 = 4;
/// Upper bound of one band's bitmap.
const BAND_BYTES: i64 = 1 << 20;

/// Maps destination coordinates to the 4x bitmap currently being drawn.
#[derive(Clone, Copy)]
pub struct Map {
    ox: i32,
    oy: i32,
}

impl Map {
    pub fn x(&self, v: i32) -> i32 {
        (v - self.ox) * K
    }
    pub fn y(&self, v: i32) -> i32 {
        (v - self.oy) * K
    }
    /// A length (size, radius) in 4x pixels.
    pub fn len(&self, v: i32) -> i32 {
        v * K
    }
    pub fn rect(&self, r: RECT) -> RECT {
        RECT { left: self.x(r.left), top: self.y(r.top), right: self.x(r.right), bottom: self.y(r.bottom) }
    }
}

/// Repaints `r` of `dc`: starts from `bg` (what is behind the shape), lets `f` draw the shape
/// into the 4x DC (use `Map` for coordinates; leave the DC's selections and clip as found; with
/// a NULL pen, RoundRect to `right*4+1` fills exactly up to `right`), then averages it down.
pub unsafe fn paint(dc: HDC, r: RECT, bg: u32, mut f: impl FnMut(HDC, Map)) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return;
    }
    let band = ((BAND_BYTES / (3 * w as i64 * (K * K) as i64)) as i32).clamp(1, h);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w * K;
    bi.bmiHeader.biHeight = band * K;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 24;
    bi.bmiHeader.biCompression = BI_RGB;
    let mut bits: *mut c_void = null_mut();
    let mem = CreateCompatibleDC(dc);
    let dib = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if mem.is_null() || dib.is_null() {
        if !dib.is_null() {
            DeleteObject(dib);
        }
        if !mem.is_null() {
            DeleteDC(mem);
        }
        return;
    }
    let old = SelectObject(mem, dib);
    let brush = CreateSolidBrush(bg);
    let mode = SetStretchBltMode(dc, HALFTONE);
    SetBrushOrgEx(dc, 0, 0, null_mut());
    let mut y = r.top;
    while y < r.bottom {
        let rows = (r.bottom - y).min(band);
        FillRect(mem, &RECT { left: 0, top: 0, right: w * K, bottom: rows * K }, brush);
        f(mem, Map { ox: r.left, oy: y });
        StretchBlt(dc, r.left, y, w, rows, mem, 0, 0, w * K, rows * K, SRCCOPY);
        y += rows;
    }
    SetStretchBltMode(dc, mode);
    SelectObject(mem, old);
    DeleteObject(brush);
    DeleteObject(dib);
    DeleteDC(mem);
}
