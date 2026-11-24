//! Tray popup icons. A small cache of 64x64 24-bit BMPs (already composited on white) in
//! `icons\` beside the data: `<fnv1a-64 hex of key>.bmp`, key = the sub's App Store artwork URL
//! (mzstatic) or else its domain (favicon from DuckDuckGo). `<hash>.no` marks a failed fetch,
//! retried after 7 days. A cache only: not backed up, unreferenced files pruned.
//! `fetch_missing` (network + GDI+ decode) runs only in the headless `--icons` helper; gdiplus.dll
//! is loaded there on demand, never in the tray. The tray only calls `path`, `pending`, `load`
//! and `draw` (plain file checks and GDI blits).
use crate::scan::appstore;
use std::collections::HashSet;
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::{Duration, SystemTime};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconFromResourceEx, DestroyIcon, DrawIconEx, LoadImageW, DI_NORMAL, IMAGE_BITMAP, LR_CREATEDIBSECTION, LR_LOADFROMFILE};

pub const DIR: &str = "icons";
/// Held by the `--icons` helper while it runs (uninstall waits for it).
pub const MUTEX: &str = "Local\\SubscribityIcons";
pub const MAX_PER_RUN: usize = 8;
const SIZE: i32 = 64;
const RETRY: Duration = Duration::from_secs(7 * 24 * 3600);

fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// Cache file of `key` under the data folder `dir`.
pub fn path(dir: &Path, key: &str) -> PathBuf {
    dir.join(DIR).join(format!("{:016x}.bmp", fnv1a(key)))
}

fn marker(p: &Path) -> PathBuf {
    p.with_extension("no")
}

/// A bare host name like "netflix.com" (letters, digits, dots, dashes).
fn is_domain(k: &str) -> bool {
    (3..=253).contains(&k.len())
        && k.contains('.')
        && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !k.starts_with(['.', '-'])
        && !k.ends_with(['.', '-'])
        && !k.contains("..")
}

/// (host, path) to download `key` from; None for a key we never fetch.
fn source(key: &str) -> Option<(String, String)> {
    if appstore::is_artwork(key) {
        let rest = &key["https://".len()..];
        let cut = rest.find('/').unwrap_or(rest.len());
        let path = if cut == rest.len() { "/".to_string() } else { rest[cut..].to_string() };
        return Some((rest[..cut].to_ascii_lowercase(), path));
    }
    is_domain(key).then(|| ("icons.duckduckgo.com".into(), format!("/ip3/{}.ico", key.to_ascii_lowercase())))
}

/// Whether to download: no image yet, and no failure marker younger than 7 days.
fn due(has_image: bool, marker_age: Option<Duration>) -> bool {
    !has_image && marker_age.is_none_or(|a| a >= RETRY)
}

fn is_due(p: &Path) -> bool {
    // A marker dated in the future (clock moved back) counts as old.
    let age = fs::metadata(marker(p)).and_then(|m| m.modified()).ok().map(|t| SystemTime::now().duration_since(t).unwrap_or(RETRY));
    due(p.exists(), age)
}

/// Some key has neither an image nor a recent failure marker (cheap; tray-safe).
pub fn pending(dir: &Path, keys: &[String]) -> bool {
    keys.iter().any(|k| source(k).is_some() && is_due(&path(dir, k)))
}

/// Deletes cache files (bmp, no, tmp) whose hash is not one of `keep`.
fn prune(root: &Path, keep: &HashSet<String>) {
    let Ok(rd) = fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let ours = matches!(p.extension().and_then(|x| x.to_str()), Some("bmp" | "no" | "tmp"));
        let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("");
        if ours && !keep.contains(stem) {
            let _ = fs::remove_file(&p);
        }
    }
}

/// Downloads up to 8 missing icons for `keys` (all subs' keys, popup rows first), writes them
/// to the cache and prunes files no key refers to. Returns how many new images were saved.
/// Stops at the first network error (offline: nothing is marked, the next run tries again).
/// Helper process only: loads gdiplus.dll.
pub fn fetch_missing(dir: &Path, keys: &[String]) -> u32 {
    let root = dir.join(DIR);
    let mut seen = HashSet::new();
    let keys: Vec<&String> = keys.iter().filter(|k| source(k).is_some() && seen.insert(k.as_str())).collect();
    prune(&root, &keys.iter().map(|k| format!("{:016x}", fnv1a(k))).collect());
    let todo: Vec<&String> = keys.into_iter().filter(|k| is_due(&path(dir, k))).take(MAX_PER_RUN).collect();
    // Created once here, never per file: if uninstall removes the folder meanwhile, writes fail.
    if todo.is_empty() || fs::create_dir_all(&root).is_err() {
        return 0;
    }
    let mut gdip = None;
    let mut saved = 0;
    for k in todo {
        let p = path(dir, k);
        let Some((host, url_path)) = source(k) else { continue };
        let img = match appstore::fetch(&host, &url_path) {
            Err(0) => break,
            Err(_) => None,
            Ok(body) => convert(&body, &mut gdip, &p.with_extension("tmp")),
        };
        match img {
            Some(bmp) => {
                let tmp = p.with_extension("tmp");
                if fs::write(&tmp, bmp).is_ok() && fs::rename(&tmp, &p).is_ok() {
                    let _ = fs::remove_file(marker(&p));
                    saved += 1;
                } else {
                    let _ = fs::remove_file(&tmp);
                }
            }
            None => {
                let _ = fs::write(marker(&p), b"");
            }
        }
    }
    saved
}

/// Image data of the best entry of an .ico file: the smallest at least 64 px wide, else the
/// largest. None when `b` is not an icon file.
fn ico_entry(b: &[u8]) -> Option<&[u8]> {
    if b.len() < 6 || b[..4] != [0, 0, 1, 0] {
        return None;
    }
    let n = u16::from_le_bytes([b[4], b[5]]) as usize;
    let mut best: Option<(u32, u16, &[u8])> = None;
    for i in 0..n {
        let e = b.get(6 + i * 16..6 + i * 16 + 16)?;
        let w = if e[0] == 0 { 256 } else { e[0] as u32 };
        let bpp = u16::from_le_bytes([e[6], e[7]]);
        let size = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
        let off = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
        let Some(data) = b.get(off..off.checked_add(size)?) else { continue };
        let better = match best {
            None => true,
            Some((bw, bb, _)) => {
                let (fits, bfits) = (w >= SIZE as u32, bw >= SIZE as u32);
                if fits != bfits { fits } else if w != bw { (w < bw) == fits } else { bpp > bb }
            }
        };
        if better && size > 0 {
            best = Some((w, bpp, data));
        }
    }
    best.map(|(_, _, d)| d)
}

/// A 24-bit bottom-up BMP file of `bits` (64x64, rows of 192 bytes).
fn bmp_file(bits: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(54 + bits.len());
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&(54 + bits.len() as u32).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&54u32.to_le_bytes());
    for v in [40u32, SIZE as u32, SIZE as u32] {
        f.extend_from_slice(&v.to_le_bytes());
    }
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&24u16.to_le_bytes());
    for v in [0u32, bits.len() as u32, 2835, 2835, 0, 0] {
        f.extend_from_slice(&v.to_le_bytes());
    }
    f.extend_from_slice(bits);
    f
}

/// Downloaded image (ico, png, jpg, gif, bmp) → 64x64 BMP file on white. `tmp`: scratch file
/// GDI+ reads from.
fn convert(body: &[u8], gdip: &mut Option<Option<Gdip>>, tmp: &Path) -> Option<Vec<u8>> {
    unsafe {
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = SIZE;
        bi.bmiHeader.biHeight = SIZE; // bottom-up, like the file
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 24;
        bi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut c_void = null_mut();
        let dc = CreateCompatibleDC(null_mut());
        let dib = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if dc.is_null() || dib.is_null() || bits.is_null() {
            if !dib.is_null() {
                DeleteObject(dib);
            }
            DeleteDC(dc);
            return None;
        }
        let len = (SIZE * SIZE * 3) as usize;
        std::ptr::write_bytes(bits as *mut u8, 0xFF, len);
        let old = SelectObject(dc, dib);
        let ok = match ico_entry(body) {
            Some(e) => {
                let ic = CreateIconFromResourceEx(e.as_ptr(), e.len() as u32, 1, 0x0003_0000, SIZE, SIZE, 0);
                let ok = !ic.is_null() && DrawIconEx(dc, 0, 0, ic, SIZE, SIZE, 0, null_mut(), DI_NORMAL) != 0;
                if !ic.is_null() {
                    DestroyIcon(ic);
                }
                ok
            }
            None => gdip.get_or_insert_with(|| Gdip::start()).as_ref().is_some_and(|g| g.draw(dc, body, tmp)),
        };
        GdiFlush();
        let out = ok.then(|| bmp_file(std::slice::from_raw_parts(bits as *const u8, len)));
        SelectObject(dc, old);
        DeleteObject(dib);
        DeleteDC(dc);
        out
    }
}

#[repr(C)]
struct StartupInput {
    version: u32,
    callback: *const c_void,
    no_thread: BOOL,
    no_codecs: BOOL,
}
type Startup = unsafe extern "system" fn(*mut usize, *const StartupInput, *mut c_void) -> i32;
type FromFile = unsafe extern "system" fn(*const u16, *mut *mut c_void) -> i32;
type ToHbitmap = unsafe extern "system" fn(*mut c_void, *mut HBITMAP, u32) -> i32;
type Dispose = unsafe extern "system" fn(*mut c_void) -> i32;
type Shutdown = unsafe extern "system" fn(usize);

/// GDI+ flat API, loaded at run time (helper process only).
struct Gdip {
    lib: HMODULE,
    token: usize,
    from_file: FromFile,
    to_hbitmap: ToHbitmap,
    dispose: Dispose,
    shutdown: Shutdown,
}

impl Gdip {
    fn start() -> Option<Gdip> {
        unsafe {
            let lib = LoadLibraryW(crate::w("gdiplus.dll").as_ptr());
            if lib.is_null() {
                return None;
            }
            let f = |n: &[u8]| GetProcAddress(lib, n.as_ptr());
            let (Some(st), Some(ff), Some(th), Some(di), Some(sd)) =
                (f(b"GdiplusStartup\0"), f(b"GdipCreateBitmapFromFile\0"), f(b"GdipCreateHBITMAPFromBitmap\0"), f(b"GdipDisposeImage\0"), f(b"GdiplusShutdown\0"))
            else {
                FreeLibrary(lib);
                return None;
            };
            let input = StartupInput { version: 1, callback: null(), no_thread: 0, no_codecs: 0 };
            let mut token = 0usize;
            if std::mem::transmute::<_, Startup>(st)(&mut token, &input, null_mut()) != 0 {
                FreeLibrary(lib);
                return None;
            }
            Some(Gdip {
                lib,
                token,
                from_file: std::mem::transmute::<_, FromFile>(ff),
                to_hbitmap: std::mem::transmute::<_, ToHbitmap>(th),
                dispose: std::mem::transmute::<_, Dispose>(di),
                shutdown: std::mem::transmute::<_, Shutdown>(sd),
            })
        }
    }

    /// Decodes `body` and draws it fitted and centred into the 64x64 bitmap selected in `dc`.
    unsafe fn draw(&self, dc: HDC, body: &[u8], tmp: &Path) -> bool {
        if fs::write(tmp, body).is_err() {
            return false;
        }
        let mut img = null_mut();
        let mut hb: HBITMAP = null_mut();
        if (self.from_file)(crate::w(&tmp.to_string_lossy()).as_ptr(), &mut img) == 0 && !img.is_null() {
            (self.to_hbitmap)(img, &mut hb, 0xFFFF_FFFF); // alpha composited on white
        }
        if !img.is_null() {
            (self.dispose)(img); // releases the file
        }
        let _ = fs::remove_file(tmp);
        if hb.is_null() {
            return false;
        }
        let mut bm: BITMAP = std::mem::zeroed();
        GetObjectW(hb, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void);
        let ok = bm.bmWidth > 0 && bm.bmHeight > 0 && {
            let (w, h) = if bm.bmWidth >= bm.bmHeight { (SIZE, (SIZE * bm.bmHeight / bm.bmWidth).max(1)) } else { ((SIZE * bm.bmWidth / bm.bmHeight).max(1), SIZE) };
            let src = CreateCompatibleDC(dc);
            let old = SelectObject(src, hb);
            SetStretchBltMode(dc, HALFTONE);
            SetBrushOrgEx(dc, 0, 0, null_mut());
            let ok = StretchBlt(dc, (SIZE - w) / 2, (SIZE - h) / 2, w, h, src, 0, 0, bm.bmWidth, bm.bmHeight, SRCCOPY) != 0;
            SelectObject(src, old);
            DeleteDC(src);
            ok
        };
        DeleteObject(hb);
        ok
    }
}

impl Drop for Gdip {
    fn drop(&mut self) {
        unsafe {
            (self.shutdown)(self.token);
            FreeLibrary(self.lib);
        }
    }
}

/// Cached icon as a GDI bitmap; null when missing. The caller frees it with DeleteObject.
pub fn load(p: &Path) -> HBITMAP {
    unsafe { LoadImageW(null_mut(), crate::w(&p.to_string_lossy()).as_ptr(), IMAGE_BITMAP, 0, 0, LR_LOADFROMFILE | LR_CREATEDIBSECTION) as HBITMAP }
}

/// Icon tile in `r`: a `ring`-coloured rounded square (corner radius `radius`), a white tile
/// `inset` px inside it, and `bmp` stretched into the white tile, clipped to its corners.
pub fn draw(dc: HDC, r: RECT, bmp: HBITMAP, ring: u32, radius: i32, inset: i32) {
    unsafe {
        let inner = RECT { left: r.left + inset, top: r.top + inset, right: r.right - inset, bottom: r.bottom - inset };
        let d = ((radius - inset).max(0) * 2).max(1);
        let op = SelectObject(dc, GetStockObject(NULL_PEN));
        for (rr, color, dd) in [(r, ring, (radius * 2).max(1)), (inner, 0x00FF_FFFF, d)] {
            let b = CreateSolidBrush(color);
            let ob = SelectObject(dc, b);
            RoundRect(dc, rr.left, rr.top, rr.right, rr.bottom, dd, dd); // same box as the letter tile
            SelectObject(dc, ob);
            DeleteObject(b);
        }
        SelectObject(dc, op);
        let mut bm: BITMAP = std::mem::zeroed();
        if bmp.is_null() || GetObjectW(bmp, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void) == 0 {
            return;
        }
        let rgn = CreateRoundRectRgn(inner.left, inner.top, inner.right, inner.bottom, d, d);
        SelectClipRgn(dc, rgn);
        let src = CreateCompatibleDC(dc);
        let old = SelectObject(src, bmp);
        let mode = SetStretchBltMode(dc, HALFTONE);
        SetBrushOrgEx(dc, 0, 0, null_mut());
        StretchBlt(dc, inner.left, inner.top, inner.right - inner.left, inner.bottom - inner.top, src, 0, 0, bm.bmWidth, bm.bmHeight, SRCCOPY);
        SetStretchBltMode(dc, mode);
        SelectObject(src, old);
        DeleteDC(src);
        SelectClipRgn(dc, null_mut());
        DeleteObject(rgn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_path_and_keys() {
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a("a"), 0xaf63_dc4c_8601_ec8c);
        let p = path(Path::new("D:\\data"), "a");
        assert_eq!(p, Path::new("D:\\data\\icons\\af63dc4c8601ec8c.bmp"));
        assert_eq!(marker(&p), Path::new("D:\\data\\icons\\af63dc4c8601ec8c.no"));
        assert_ne!(path(Path::new("x"), "netflix.com"), path(Path::new("x"), "Netflix.com"));
        assert_eq!(source("netflix.com"), Some(("icons.duckduckgo.com".into(), "/ip3/netflix.com.ico".into())));
        let art = "https://is1-ssl.mzstatic.com/image/thumb/x/100x100bb.jpg";
        assert_eq!(source(art), Some(("is1-ssl.mzstatic.com".into(), "/image/thumb/x/100x100bb.jpg".into())));
        for bad in ["", "localhost", "a..b", ".a.com", "a.com-", "a b.com", "a.com/x", "https://evil.example/a.png", "http://a.mzstatic.com/a.jpg"] {
            assert_eq!(source(bad), None, "{bad}");
        }
    }

    #[test]
    fn icons_marker_retry() {
        let day = Duration::from_secs(24 * 3600);
        assert!(due(false, None));
        assert!(!due(true, None));
        assert!(!due(true, Some(8 * day)));
        assert!(!due(false, Some(Duration::ZERO)));
        assert!(!due(false, Some(6 * day)));
        assert!(due(false, Some(7 * day)));
        assert!(due(false, Some(30 * day)));
    }

    #[test]
    fn icons_ico_entry_and_bmp() {
        // Three entries: 16 px, 256 px (w byte 0), 64 px; data bytes 0x10, 0x20, 0x30.
        let mut b = vec![0, 0, 1, 0, 3, 0];
        let off = 6 + 3 * 16;
        for (i, w) in [16u8, 0, 64].iter().enumerate() {
            b.extend_from_slice(&[*w, *w, 0, 0, 1, 0, 32, 0]);
            b.extend_from_slice(&1u32.to_le_bytes());
            b.extend_from_slice(&((off + i) as u32).to_le_bytes());
        }
        b.extend_from_slice(&[0x10, 0x20, 0x30]);
        assert_eq!(ico_entry(&b), Some(&[0x30u8][..])); // smallest >= 64
        b[6 + 2 * 16] = 32; // third entry now 32 px: 256 wins
        assert_eq!(ico_entry(&b), Some(&[0x20u8][..]));
        b[6 + 16] = 24; // 16, 24, 32: largest
        assert_eq!(ico_entry(&b), Some(&[0x30u8][..]));
        assert_eq!(ico_entry(&b[..30]), None); // truncated directory
        assert_eq!(ico_entry(b"\x89PNG\r\n\x1a\n"), None);
        let f = bmp_file(&vec![0xFF; 64 * 64 * 3]);
        assert_eq!(f.len(), 54 + 12288);
        assert_eq!(&f[..2], b"BM");
        assert_eq!(u32::from_le_bytes([f[2], f[3], f[4], f[5]]), 12342);
        assert_eq!(u16::from_le_bytes([f[28], f[29]]), 24);
    }

    #[test]
    fn icons_prune_keeps_referenced() {
        let root = std::env::temp_dir().join(format!("subscribity-icons-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(DIR)).unwrap();
        let keep = path(&root, "netflix.com");
        let gone = path(&root, "old.example");
        for p in [&keep, &marker(&gone), &gone] {
            fs::write(p, b"x").unwrap();
        }
        fs::write(root.join(DIR).join("readme.txt"), b"x").unwrap();
        prune(&root.join(DIR), &[format!("{:016x}", fnv1a("netflix.com"))].into_iter().collect());
        assert!(keep.exists() && !gone.exists() && !marker(&gone).exists());
        assert!(root.join(DIR).join("readme.txt").exists()); // not ours
        assert!(!pending(&root, &["netflix.com".into()]));
        assert!(pending(&root, &["old.example".into()]));
        fs::write(marker(&gone), b"").unwrap();
        assert!(!pending(&root, &["old.example".into(), "".into()]));
        let _ = fs::remove_dir_all(&root);
    }

    /// Live: one favicon (ico), one App Store artwork (jpg via GDI+), one unknown domain (404).
    /// `SUBSCRIBITY_ICONS_TEST_DIR` keeps the files for a look.
    #[test]
    #[ignore]
    fn icons_live_fetch() {
        let root = std::env::var_os("SUBSCRIBITY_ICONS_TEST_DIR").map_or_else(|| std::env::temp_dir().join("subscribity-icons-live"), PathBuf::from);
        let _ = fs::remove_dir_all(root.join(DIR));
        let (art, _) = appstore::lookup("Instagram", "us").expect("network");
        let keys = vec!["github.com".to_string(), art, "nonexistent-zz9q.example".into()];
        assert_eq!(fetch_missing(&root, &keys), 2);
        assert!(path(&root, &keys[0]).exists() && path(&root, &keys[1]).exists());
        assert!(marker(&path(&root, &keys[2])).exists() && !pending(&root, &keys));
        let b = load(&path(&root, &keys[0]));
        assert!(!b.is_null());
        unsafe { DeleteObject(b) };
        let _ = fs::copy(path(&root, &keys[1]), root.join("artwork.bmp"));
        assert_eq!(fetch_missing(&root, &keys[..1]), 0); // cached; others pruned
        assert!(!path(&root, &keys[1]).exists());
    }
}
