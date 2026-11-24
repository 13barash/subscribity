//! GDI+ flat API, loaded at run time with `LoadLibrary` (helper/window processes only; the tray
//! never loads gdiplus.dll). Used to decode icon/wallpaper images and to scale icons with
//! high-quality bicubic interpolation.
use std::ffi::c_void;
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::WindowsAndMessaging::HICON;

type P = *mut c_void;

#[repr(C)]
struct StartupInput {
    version: u32,
    callback: *const c_void,
    no_thread: BOOL,
    no_codecs: BOOL,
}

/// Every GDI+ entry point we use, resolved once.
struct Fns {
    startup: unsafe extern "system" fn(*mut usize, *const StartupInput, P) -> i32,
    shutdown: unsafe extern "system" fn(usize),
    from_file: unsafe extern "system" fn(*const u16, *mut P) -> i32,
    from_hicon: unsafe extern "system" fn(HICON, *mut P) -> i32,
    to_hbitmap: unsafe extern "system" fn(P, *mut HBITMAP, u32) -> i32,
    dispose: unsafe extern "system" fn(P) -> i32,
    width: unsafe extern "system" fn(P, *mut u32) -> i32,
    height: unsafe extern "system" fn(P, *mut u32) -> i32,
    gfx_from_hdc: unsafe extern "system" fn(HDC, *mut P) -> i32,
    gfx_delete: unsafe extern "system" fn(P) -> i32,
    set_interp: unsafe extern "system" fn(P, i32) -> i32,
    set_pixel_offset: unsafe extern "system" fn(P, i32) -> i32,
    attr_new: unsafe extern "system" fn(*mut P) -> i32,
    attr_wrap: unsafe extern "system" fn(P, i32, u32, BOOL) -> i32,
    attr_dispose: unsafe extern "system" fn(P) -> i32,
    draw: unsafe extern "system" fn(P, P, i32, i32, i32, i32, i32, i32, i32, i32, i32, P, P, P) -> i32,
}

unsafe fn sym<T: Copy>(lib: HMODULE, name: &[u8]) -> Option<T> {
    let p = GetProcAddress(lib, name.as_ptr())?;
    Some(std::mem::transmute_copy::<_, T>(&p))
}

unsafe fn load_fns(lib: HMODULE) -> Option<Fns> {
    Some(Fns {
        startup: sym(lib, b"GdiplusStartup\0")?,
        shutdown: sym(lib, b"GdiplusShutdown\0")?,
        from_file: sym(lib, b"GdipCreateBitmapFromFile\0")?,
        from_hicon: sym(lib, b"GdipCreateBitmapFromHICON\0")?,
        to_hbitmap: sym(lib, b"GdipCreateHBITMAPFromBitmap\0")?,
        dispose: sym(lib, b"GdipDisposeImage\0")?,
        width: sym(lib, b"GdipGetImageWidth\0")?,
        height: sym(lib, b"GdipGetImageHeight\0")?,
        gfx_from_hdc: sym(lib, b"GdipCreateFromHDC\0")?,
        gfx_delete: sym(lib, b"GdipDeleteGraphics\0")?,
        set_interp: sym(lib, b"GdipSetInterpolationMode\0")?,
        set_pixel_offset: sym(lib, b"GdipSetPixelOffsetMode\0")?,
        attr_new: sym(lib, b"GdipCreateImageAttributes\0")?,
        attr_wrap: sym(lib, b"GdipSetImageAttributesWrapMode\0")?,
        attr_dispose: sym(lib, b"GdipDisposeImageAttributes\0")?,
        draw: sym(lib, b"GdipDrawImageRectRectI\0")?,
    })
}

pub struct Gdip {
    lib: HMODULE,
    token: usize,
    f: Fns,
}

/// A GDI+ image; freed on drop.
pub struct Image<'a> {
    g: &'a Gdip,
    raw: P,
}

impl Drop for Image<'_> {
    fn drop(&mut self) {
        unsafe { (self.g.f.dispose)(self.raw) };
    }
}

impl Gdip {
    pub fn start() -> Option<Gdip> {
        unsafe {
            let lib = LoadLibraryW(crate::w("gdiplus.dll").as_ptr());
            if lib.is_null() {
                return None;
            }
            let Some(f) = load_fns(lib) else {
                FreeLibrary(lib);
                return None;
            };
            let input = StartupInput { version: 1, callback: null(), no_thread: 0, no_codecs: 0 };
            let mut token = 0usize;
            if (f.startup)(&mut token, &input, null_mut()) != 0 {
                FreeLibrary(lib);
                return None;
            }
            Some(Gdip { lib, token, f })
        }
    }

    /// Image file → GDI bitmap (alpha composited on white); null on failure. Caller frees it.
    pub unsafe fn hbitmap(&self, p: &Path) -> HBITMAP {
        let mut hb: HBITMAP = null_mut();
        if let Some(img) = self.open(p) {
            (self.f.to_hbitmap)(img.raw, &mut hb, 0xFFFF_FFFF);
        }
        hb // the Image drop released the file
    }

    pub unsafe fn open(&self, p: &Path) -> Option<Image<'_>> {
        let mut raw = null_mut();
        let ok = (self.f.from_file)(crate::w(&p.to_string_lossy()).as_ptr(), &mut raw) == 0 && !raw.is_null();
        ok.then_some(Image { g: self, raw })
    }

    pub unsafe fn from_hicon(&self, ic: HICON) -> Option<Image<'_>> {
        let mut raw = null_mut();
        let ok = (self.f.from_hicon)(ic, &mut raw) == 0 && !raw.is_null();
        ok.then_some(Image { g: self, raw })
    }

    /// Draws `img` fitted and centred into the `size` x `size` square at the origin of `dc`,
    /// scaled up or down with high-quality bicubic (edges clamped, no halo).
    pub unsafe fn draw_fit(&self, dc: HDC, img: &Image, size: i32) -> bool {
        let (mut w, mut h) = (0u32, 0u32);
        (self.f.width)(img.raw, &mut w);
        (self.f.height)(img.raw, &mut h);
        let (w, h) = (w as i64, h as i64);
        if w <= 0 || h <= 0 || w > 32_000 || h > 32_000 {
            return false;
        }
        let s = size as i64;
        let (dw, dh) = if w >= h { (s, (s * h / w).max(1)) } else { ((s * w / h).max(1), s) };
        let (mut g, mut attr) = (null_mut(), null_mut());
        if (self.f.gfx_from_hdc)(dc, &mut g) != 0 || g.is_null() {
            return false;
        }
        (self.f.set_interp)(g, 7); // HighQualityBicubic
        (self.f.set_pixel_offset)(g, 4); // Half
        (self.f.attr_new)(&mut attr);
        if !attr.is_null() {
            (self.f.attr_wrap)(attr, 3, 0, 0); // TileFlipXY: no fringe at the borders
        }
        let st = (self.f.draw)(g, img.raw, ((s - dw) / 2) as i32, ((s - dh) / 2) as i32, dw as i32, dh as i32, 0, 0, w as i32, h as i32, 2, attr, null_mut(), null_mut());
        if !attr.is_null() {
            (self.f.attr_dispose)(attr);
        }
        (self.f.gfx_delete)(g);
        st == 0
    }
}

impl Drop for Gdip {
    fn drop(&mut self) {
        unsafe {
            (self.f.shutdown)(self.token);
            FreeLibrary(self.lib);
        }
    }
}
