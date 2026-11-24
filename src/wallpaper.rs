//! "Wallpaper glass": the desktop wallpaper, shrunk, blurred and dimmed in Rust once when the window
//! opens, served as a small BMP at `http://icon.localhost/__wallpaper` so the page needs no CSS
//! filter (no per-frame blur). Used when the OS acrylic is unavailable (Transparency effects off).
//! GDI+ is loaded only here in the window process, never in the tray.
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETDESKWALLPAPER};

const MAX_W: i32 = 320; // the page scales it up (cover); it is blurred anyway
const BLUR_R: usize = 10; // 3 box passes of radius 10 at 320 px ~ 40 px CSS blur at 1040 px
const SATURATION: f32 = 1.4;
/// Brightest channel after processing. With the page's slate tint (rgba(14,17,22,.72)) and a 5%
/// white card on top, this keeps #9AA4B2 text at >= 4.5:1 even for an all-white wallpaper.
const MAX_CHANNEL: f32 = 120.0;

/// Windows setting "Transparency effects" (Personalize\EnableTransparency); on when unreadable.
pub fn transparency_on() -> bool {
    unsafe {
        let mut v = 1u32;
        let mut size = 4u32;
        let key = crate::w("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        let ok = RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), crate::w("EnableTransparency").as_ptr(), RRF_RT_REG_DWORD, null_mut(), &mut v as *mut u32 as *mut c_void, &mut size);
        ok != 0 || v != 0
    }
}

fn current_path() -> Option<PathBuf> {
    let mut buf = [0u16; 520];
    let ok = unsafe { SystemParametersInfoW(SPI_GETDESKWALLPAPER, buf.len() as u32, buf.as_mut_ptr() as *mut c_void, 0) };
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    (ok != 0 && n > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

fn transcoded_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Microsoft\\Windows\\Themes\\TranscodedWallpaper"))
}

/// The wallpaper as a ready-to-serve 24-bit BMP file; None for a solid-colour desktop or on failure.
pub fn render() -> Option<Vec<u8>> {
    let first = current_path()?; // empty = solid colour: no image
    let (w, h, mut px) = [Some(first), transcoded_path()].into_iter().flatten().find_map(|p| shrink(&p))?;
    process(&mut px, w, h);
    Some(bmp24(&px, w, h))
}

/// Decodes `p` and shrinks it to <= MAX_W wide; returns (w, h, top-down BGRA).
fn shrink(p: &Path) -> Option<(usize, usize, Vec<u8>)> {
    let src = crate::icons::decode_file(p);
    if src.is_null() {
        return None;
    }
    unsafe {
        let mut bm: BITMAP = std::mem::zeroed();
        GetObjectW(src, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void);
        let out = if bm.bmWidth > 0 && bm.bmHeight > 0 {
            let (w, h) = fit(bm.bmWidth as usize, bm.bmHeight as usize, MAX_W as usize);
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w as i32;
            bi.bmiHeader.biHeight = -(h as i32);
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = null_mut();
            let dc = CreateCompatibleDC(null_mut());
            let dib = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            let mut res = None;
            if !dc.is_null() && !dib.is_null() && !bits.is_null() {
                let old = SelectObject(dc, dib);
                let sdc = CreateCompatibleDC(dc);
                let sold = SelectObject(sdc, src);
                SetStretchBltMode(dc, HALFTONE);
                SetBrushOrgEx(dc, 0, 0, null_mut());
                if StretchBlt(dc, 0, 0, w as i32, h as i32, sdc, 0, 0, bm.bmWidth, bm.bmHeight, SRCCOPY) != 0 {
                    GdiFlush();
                    res = Some((w, h, std::slice::from_raw_parts(bits as *const u8, w * h * 4).to_vec()));
                }
                SelectObject(sdc, sold);
                DeleteDC(sdc);
                SelectObject(dc, old);
            }
            if !dib.is_null() {
                DeleteObject(dib);
            }
            if !dc.is_null() {
                DeleteDC(dc);
            }
            res
        } else {
            None
        };
        DeleteObject(src);
        out
    }
}

/// Size that fits `max` wide, never upscaled, aspect kept, at least 1x1.
fn fit(w: usize, h: usize, max: usize) -> (usize, usize) {
    let nw = w.min(max).max(1);
    (nw, (h * nw / w.max(1)).max(1))
}

/// Blur (3 box passes), saturate, then scale so no channel exceeds MAX_CHANNEL (only if needed).
fn process(px: &mut [u8], w: usize, h: usize) {
    for _ in 0..3 {
        box_blur_axis(px, w, 4, h, w * 4, BLUR_R); // horizontal
        box_blur_axis(px, h, w * 4, w, 4, BLUR_R); // vertical
    }
    let mut max = 1.0f32;
    for p in px.chunks_exact_mut(4) {
        let (b, g, r) = (p[0] as f32, p[1] as f32, p[2] as f32);
        let l = 0.299 * r + 0.587 * g + 0.114 * b;
        for (i, c) in [b, g, r].into_iter().enumerate() {
            let v = (l + (c - l) * SATURATION).clamp(0.0, 255.0);
            p[i] = v.round() as u8;
            max = max.max(v);
        }
    }
    if max > MAX_CHANNEL {
        let k = MAX_CHANNEL / max;
        for p in px.chunks_exact_mut(4) {
            for c in &mut p[..3] {
                *c = (*c as f32 * k).round() as u8;
            }
        }
    }
}

/// One box-blur pass of radius `r` (edges clamped) over the first 3 channels of each pixel.
/// `n` pixels per line, `step` bytes between them; `lines` lines, `line_step` bytes apart.
fn box_blur_axis(px: &mut [u8], n: usize, step: usize, lines: usize, line_step: usize, r: usize) {
    if n < 2 || r == 0 {
        return;
    }
    let win = (2 * r + 1) as i32;
    let at = |i: i64| i.clamp(0, n as i64 - 1) as usize;
    let mut v = vec![0i32; n];
    for l in 0..lines {
        for c in 0..3 {
            let base = l * line_step + c;
            for (i, e) in v.iter_mut().enumerate() {
                *e = px[base + i * step] as i32;
            }
            let mut sum: i32 = (-(r as i64)..=r as i64).map(|k| v[at(k)]).sum();
            for i in 0..n {
                px[base + i * step] = ((sum + win / 2) / win) as u8;
                sum += v[at(i as i64 + r as i64 + 1)] - v[at(i as i64 - r as i64)];
            }
        }
    }
}

/// Top-down BGRA → bottom-up 24-bit BMP file (rows padded to 4 bytes).
fn bmp24(px: &[u8], w: usize, h: usize) -> Vec<u8> {
    let row = (w * 3 + 3) & !3;
    let size = row * h;
    let mut f = Vec::with_capacity(54 + size);
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&((54 + size) as u32).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&(w as u32).to_le_bytes());
    f.extend_from_slice(&(h as u32).to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&24u16.to_le_bytes());
    for v in [0u32, size as u32, 2835, 2835, 0, 0] {
        f.extend_from_slice(&v.to_le_bytes());
    }
    for y in (0..h).rev() {
        for x in 0..w {
            f.extend_from_slice(&px[(y * w + x) * 4..(y * w + x) * 4 + 3]);
        }
        f.resize(f.len() + row - w * 3, 0);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_downscales_only() {
        assert_eq!(fit(3840, 2160, 320), (320, 180));
        assert_eq!(fit(200, 100, 320), (200, 100));
        assert_eq!(fit(1000, 1, 320), (320, 1));
        assert_eq!(fit(1080, 1920, 320), (320, 568));
    }

    #[test]
    fn blur_keeps_flat_and_spreads_edge() {
        let (w, h) = (40, 6);
        let mut px = vec![200u8; w * h * 4];
        process_blur_only(&mut px, w, h);
        assert!(px.iter().all(|&c| c == 200));
        // left half black, right half white: the middle becomes grey, the far ends stay put
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in w / 2..w {
                px[(y * w + x) * 4..(y * w + x) * 4 + 3].fill(255);
            }
        }
        process_blur_only(&mut px, w, h);
        let at = |x: usize| px[(2 * w + x) * 4];
        assert!(at(0) < 5 && at(w - 1) > 250);
        assert!(at(w / 2) > 80 && at(w / 2) < 180);
        assert!(at(5) <= at(15) && at(15) <= at(25) && at(25) <= at(35));
    }

    fn process_blur_only(px: &mut [u8], w: usize, h: usize) {
        box_blur_axis(px, w, 4, h, w * 4, 4);
        box_blur_axis(px, h, w * 4, w, 4, 4);
    }

    #[test]
    fn white_wallpaper_is_capped() {
        let (w, h) = (16, 9);
        let mut px = vec![255u8; w * h * 4];
        process(&mut px, w, h);
        assert!(px.chunks_exact(4).all(|p| p[..3].iter().all(|&c| c as f32 <= MAX_CHANNEL)));
        let mut dark = vec![60u8; w * h * 4];
        process(&mut dark, w, h);
        assert!(dark.chunks_exact(4).all(|p| p[..3].iter().all(|&c| c == 60)));
    }

    #[test]
    fn bmp_layout() {
        let px = [10u8, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 1, 2, 3, 255]; // 2x2
        let f = bmp24(&px, 2, 2);
        assert_eq!(&f[..2], b"BM");
        assert_eq!(f.len(), 54 + 8 * 2);
        assert_eq!(&f[54..57], &[70, 80, 90]); // bottom row first
        assert_eq!(&f[62..65], &[10, 20, 30]);
    }
}
