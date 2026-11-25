//! App icons (tray popup + window). A small cache of 128x128 24-bit BMPs (already composited on
//! white, small sources upscaled with bicubic) in `icons\` beside the data:
//! `<fnv1a-64 hex of key>-3.bmp` (the `-3` = the generation; older `<hash>.bmp` / `-2.bmp`
//! files are pruned), key = the sub's App Store artwork URL (mzstatic) or else its domain
//! (apple-touch-icon, else the Google favicon, else the DuckDuckGo one; the largest wins).
//! `<hash>-3.no` marks a failed fetch, retried
//! after 7 days. A cache only: not backed up, unreferenced files pruned.
//! `fetch_missing` (network + GDI+ decode) runs only in the headless `--icons` helper; gdiplus.dll
//! is loaded there on demand, never in the tray. The tray only calls `path`, `pending`, `load`
//! and `draw` (plain file checks and GDI blits).
use crate::gdip::Gdip;
use crate::scan::appstore;
use std::collections::HashSet;
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::time::{Duration, SystemTime};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconFromResourceEx, DestroyIcon, LoadImageW, IMAGE_BITMAP, LR_CREATEDIBSECTION, LR_LOADFROMFILE};

pub const DIR: &str = "icons";
/// Held by the `--icons` helper while it runs (uninstall waits for it).
pub const MUTEX: &str = "Local\\SubscribityIcons";
pub const MAX_PER_RUN: usize = 8;
const SIZE: i32 = 128;
/// Cache generation suffix of the file stem; files of older generations (no suffix = 64 px, `-2`)
/// are pruned, so they are refetched once.
const GEN: &str = "-3";
/// A source at least this many px wide ends the candidate search; smaller ones are fallbacks.
const GOOD: u32 = 96;
const RETRY: Duration = Duration::from_secs(7 * 24 * 3600);

fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// Cache file of `key` under the data folder `dir`.
pub fn path(dir: &Path, key: &str) -> PathBuf {
    dir.join(DIR).join(format!("{}.bmp", stem(key)))
}

/// File stem (hash + generation) of `key`.
fn stem(key: &str) -> String {
    format!("{:016x}{GEN}", fnv1a(key))
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

/// App Store artwork path asking for 256 px when it names a smaller size segment
/// (`/100x100bb.jpg` → `/256x256bb.jpg`); unchanged otherwise.
fn upsize(path: &str) -> String {
    let cut = path.rfind('/').map_or(0, |i| i + 1);
    let seg = &path[cut..];
    let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    let wl = digits(seg);
    if wl > 0 && seg[wl..].starts_with('x') {
        let hl = digits(&seg[wl + 1..]);
        let (w, h) = (seg[..wl].parse::<u32>(), seg[wl + 1..wl + 1 + hl].parse::<u32>());
        if hl > 0 && matches!((w, h), (Ok(w), Ok(h)) if w < 256 && h < 256) {
            return format!("{}256x256{}", &path[..cut], &seg[wl + 1 + hl..]);
        }
    }
    path.to_string()
}

/// Download sources for `key`, best first (empty for a key we never fetch). A domain: its own
/// apple-touch-icon (usually 180 px; https redirects are followed, but some sites only serve it
/// on one of the two names without redirecting, so the apex then `www.`), then
/// Google's favicon (up to 256 px), then DuckDuckGo's (often 16-48 px). Artwork: the 256 px rendition, then the URL as stored.
fn candidates(key: &str) -> Vec<(String, String)> {
    let Some(src) = source(key) else { return Vec::new() };
    if appstore::is_artwork(key) {
        let big = upsize(&src.1);
        let mut v = vec![(src.0.clone(), big.clone())];
        if big != src.1 {
            v.push(src);
        }
        return v;
    }
    let d = key.to_ascii_lowercase();
    let mut v = vec![(d.clone(), "/apple-touch-icon.png".to_string())];
    if !d.starts_with("www.") {
        v.push((format!("www.{d}"), "/apple-touch-icon.png".to_string()));
    }
    v.push(google(&d));
    v.push(src);
    v
}

/// Google's favicon cache at its largest native size (up to 256 px). Answers 404 (a 16 px globe
/// body, which `fetch` drops as non-200) for sites it has no icon of, so no default is ever used.
/// The `google.com/s2/favicons` URL only redirects here; asking here directly saves a round trip.
fn google(d: &str) -> (String, String) {
    ("t1.gstatic.com".into(), format!("/faviconV2?client=SOCIAL&type=FAVICON&fallback_opts=TYPE,SIZE,URL&url=https://{d}&size=256"))
}

/// A server answered (any status, or a body); `Err(0)` is no answer: offline, timeout, cut off.
fn got_answer(r: &Result<Vec<u8>, u32>) -> bool {
    !matches!(r, Err(0))
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

/// Whether `k` is a key we would ever fetch or serve (bare domain or App Store artwork URL).
/// Gate for anything that turns a request path into a cache lookup.
pub fn valid_key(k: &str) -> bool {
    source(k).is_some()
}

/// Brand color of a cached icon as 0xRRGGBB: the average of its saturated, non-white pixels.
/// None when the file is missing/not our SIZE x SIZE BMP, or the icon is grey/white/black.
pub fn tint(p: &Path) -> Option<u32> {
    let b = fs::read(p).ok()?;
    let off = u32::from_le_bytes(b.get(10..14)?.try_into().ok()?) as usize;
    let px = b.get(off..)?.get(..(SIZE * SIZE * 3) as usize)?;
    let (mut r, mut g, mut bl, mut n) = (0u64, 0u64, 0u64, 0u64);
    for c in px.chunks_exact(3) {
        let (cb, cg, cr) = (c[0] as i32, c[1] as i32, c[2] as i32);
        let (hi, lo) = (cb.max(cg).max(cr), cb.min(cg).min(cr));
        if hi - lo < 48 || lo > 225 || hi < 60 {
            continue; // grey, near-white or near-black
        }
        r += cr as u64;
        g += cg as u64;
        bl += cb as u64;
        n += 1;
    }
    if n < (SIZE * SIZE / 100) as u64 {
        return None; // under 1% of the tile: not a colored icon
    }
    Some(((r / n) as u32) << 16 | ((g / n) as u32) << 8 | (bl / n) as u32)
}

/// Whether an image `w` px wide replaces the best one found so far (strictly larger only).
fn keeps(best: &Option<(Vec<u8>, u32)>, w: u32) -> bool {
    best.as_ref().is_none_or(|(_, bw)| w > *bw)
}

/// Downloads up to `max` missing icons for `keys` (all subs' keys, popup rows first), writes
/// them to the cache and prunes files no key refers to. Returns how many new images were saved.
/// Stops at the first icon no source answered at all (offline: nothing is marked, the next run
/// tries again).
/// Helper process only: loads gdiplus.dll.
pub fn fetch_missing(dir: &Path, keys: &[String], max: usize) -> u32 {
    let root = dir.join(DIR);
    let mut seen = HashSet::new();
    let keys: Vec<&String> = keys.iter().filter(|k| source(k).is_some() && seen.insert(k.as_str())).collect();
    prune(&root, &keys.iter().map(|k| stem(k)).collect());
    let todo: Vec<&String> = keys.into_iter().filter(|k| is_due(&path(dir, k))).take(max).collect();
    // Created once here, never per file: if uninstall removes the folder meanwhile, writes fail.
    if todo.is_empty() || fs::create_dir_all(&root).is_err() {
        return 0;
    }
    let mut gdip = None;
    let mut saved = 0;
    for k in todo {
        let p = path(dir, k);
        // Best source first; the first that decodes at GOOD px or more wins, else the largest
        // decoded one. Offline only if no source answered at all (a hanging own-site or a
        // blocked DuckDuckGo just falls through; any HTTP answer, even 404, means online).
        let cands = candidates(k);
        let mut img: Option<(Vec<u8>, u32)> = None;
        let mut answered = false;
        for (host, url_path) in &cands {
            let r = appstore::fetch(host, url_path);
            answered |= got_answer(&r);
            match r {
                Err(_) => {}
                Ok(body) => {
                    if let Some((bmp, w)) = convert(&body, &mut gdip, &p.with_extension("tmp")) {
                        if keeps(&img, w) {
                            img = Some((bmp, w));
                        }
                    }
                }
            }
            if img.as_ref().is_some_and(|(_, w)| *w >= GOOD) {
                break;
            }
        }
        if !answered {
            break;
        }
        match img.map(|(b, _)| b) {
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

/// (width, image data) of the best entry of an .ico file: the smallest at least SIZE px wide,
/// else the largest. None when `b` is not an icon file.
fn ico_entry(b: &[u8]) -> Option<(u32, &[u8])> {
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
    best.map(|(w, _, d)| (w, d))
}

/// A 24-bit bottom-up BMP file of `bits` (SIZE x SIZE, rows of SIZE*3 bytes).
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

/// Decodes `body` (ico, png, jpg, gif, bmp) and draws it fitted into the SIZE bitmap selected in
/// `dc` with bicubic scaling (small sources are upscaled). `tmp`: scratch file GDI+ reads from.
unsafe fn draw_body(g: &Gdip, dc: HDC, body: &[u8], tmp: &Path) -> Option<u32> {
    if let Some((w, e)) = ico_entry(body).filter(|(_, e)| !e.starts_with(b"\x89PNG")) {
        // Classic DIB-format icon entry: decode at its own size, scale ourselves.
        let ic = CreateIconFromResourceEx(e.as_ptr(), e.len() as u32, 1, 0x0003_0000, w as i32, w as i32, 0);
        if ic.is_null() {
            return None;
        }
        let ok = g.from_hicon(ic).and_then(|img| g.draw_fit(dc, &img, SIZE));
        DestroyIcon(ic);
        return ok;
    }
    let data = ico_entry(body).map_or(body, |(_, e)| e); // PNG inside an .ico, or a plain image
    if fs::write(tmp, data).is_err() {
        return None;
    }
    let ok = g.open(tmp).and_then(|img| g.draw_fit(dc, &img, SIZE));
    let _ = fs::remove_file(tmp);
    ok
}

/// Downloaded image → (SIZE x SIZE BMP file on white, native px of the source's longer side).
fn convert(body: &[u8], gdip: &mut Option<Option<Gdip>>, tmp: &Path) -> Option<(Vec<u8>, u32)> {
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
        let native = gdip.get_or_insert_with(Gdip::start).as_ref().and_then(|g| draw_body(g, dc, body, tmp));
        GdiFlush();
        let out = native.map(|w| (bmp_file(std::slice::from_raw_parts(bits as *const u8, len)), w));
        SelectObject(dc, old);
        DeleteObject(dib);
        DeleteDC(dc);
        out
    }
}

/// Cached icon as a GDI bitmap; null when missing. The caller frees it with DeleteObject.
pub fn load(p: &Path) -> HBITMAP {
    unsafe { LoadImageW(null_mut(), crate::w(&p.to_string_lossy()).as_ptr(), IMAGE_BITMAP, 0, 0, LR_LOADFROMFILE | LR_CREATEDIBSECTION) as HBITMAP }
}

/// Icon tile in `r`, anti-aliased (4x supersampled, see `aa`): a `ring`-coloured rounded square
/// (corner radius `radius`), a white tile `inset` px inside it, and `bmp` stretched into the
/// white tile, clipped to its corners. `bg` is the colour behind the tile (row or hover colour).
/// Background colour of a cached logo: the average of its four corner pixels (cache BMPs are
/// composited on white, so a transparent logo gives white).
fn logo_bg(bmp: HBITMAP) -> u32 {
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        let old = SelectObject(dc, bmp);
        let mut bm: BITMAP = std::mem::zeroed();
        GetObjectW(bmp, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void);
        let (w, h) = (bm.bmWidth - 2, bm.bmHeight - 2);
        let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
        for (x, y) in [(1, 1), (w, 1), (1, h), (w, h)] {
            let c = GetPixel(dc, x, y);
            r += c & 0xFF;
            g += (c >> 8) & 0xFF;
            b += (c >> 16) & 0xFF;
        }
        SelectObject(dc, old);
        DeleteDC(dc);
        (r / 4) | ((g / 4) << 8) | ((b / 4) << 16)
    }
}

pub fn draw(dc: HDC, r: RECT, bmp: HBITMAP, ring: u32, radius: i32, inset: i32, bg: u32) {
    unsafe {
        let inner = RECT { left: r.left + inset, top: r.top + inset, right: r.right - inset, bottom: r.bottom - inset };
        let d = ((radius - inset).max(0) * 2).max(1);
        let mut bm: BITMAP = std::mem::zeroed();
        let has_bmp = !bmp.is_null() && GetObjectW(bmp, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void) != 0;
        // Fill the tile with the logo's own background (its corner pixel) so the padded logo
        // blends into a full-bleed tile instead of sitting on a white disc.
        let fill = if has_bmp { logo_bg(bmp) } else { 0x00FF_FFFF };
        crate::aa::paint(dc, r, bg, |big, m| {
            let op = SelectObject(big, GetStockObject(NULL_PEN));
            for (rr, color, dd) in [(r, ring, (radius * 2).max(1)), (inner, fill, d)] {
                let b = CreateSolidBrush(color);
                let ob = SelectObject(big, b);
                RoundRect(big, m.x(rr.left), m.y(rr.top), m.x(rr.right) + 1, m.y(rr.bottom) + 1, m.len(dd), m.len(dd));
                SelectObject(big, ob);
                DeleteObject(b);
            }
            SelectObject(big, op);
            if !has_bmp {
                return;
            }
            let ib = m.rect(inner);
            let rgn = CreateRoundRectRgn(ib.left, ib.top, ib.right, ib.bottom, m.len(d), m.len(d));
            SelectClipRgn(big, rgn);
            let src = CreateCompatibleDC(big);
            let old = SelectObject(src, bmp);
            let mode = SetStretchBltMode(big, HALFTONE);
            SetBrushOrgEx(big, 0, 0, null_mut());
            // Logo sits inside the white tile with ~14% padding so bold logos (Netflix) don't fill it edge to edge.
            let pad = (ib.right - ib.left) * 12 / 100;
            StretchBlt(big, ib.left + pad, ib.top + pad, ib.right - ib.left - 2 * pad, ib.bottom - ib.top - 2 * pad, src, 0, 0, bm.bmWidth, bm.bmHeight, SRCCOPY);
            SetStretchBltMode(big, mode);
            SelectObject(src, old);
            DeleteDC(src);
            SelectClipRgn(big, null_mut());
            DeleteObject(rgn);
        });
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
        assert_eq!(p, Path::new("D:\\data\\icons\\af63dc4c8601ec8c-3.bmp"));
        assert_eq!(marker(&p), Path::new("D:\\data\\icons\\af63dc4c8601ec8c-3.no"));
        assert_eq!(p.with_extension("tmp"), Path::new("D:\\data\\icons\\af63dc4c8601ec8c-3.tmp"));
        assert_ne!(path(Path::new("x"), "netflix.com"), path(Path::new("x"), "Netflix.com"));
        assert_eq!(source("netflix.com"), Some(("icons.duckduckgo.com".into(), "/ip3/netflix.com.ico".into())));
        let art = "https://is1-ssl.mzstatic.com/image/thumb/x/100x100bb.jpg";
        assert_eq!(source(art), Some(("is1-ssl.mzstatic.com".into(), "/image/thumb/x/100x100bb.jpg".into())));
        for bad in ["", "localhost", "a..b", ".a.com", "a.com-", "a b.com", "a.com/x", "https://evil.example/a.png", "http://a.mzstatic.com/a.jpg"] {
            assert_eq!(source(bad), None, "{bad}");
        }
    }

    #[test]
    fn icons_artwork_upsize_and_candidates() {
        assert_eq!(upsize("/image/thumb/x/100x100bb.jpg"), "/image/thumb/x/256x256bb.jpg");
        assert_eq!(upsize("/image/thumb/x/60x60bb.png"), "/image/thumb/x/256x256bb.png");
        assert_eq!(upsize("/image/thumb/x/512x512bb.jpg"), "/image/thumb/x/512x512bb.jpg");
        assert_eq!(upsize("/image/thumb/x/256x256bb.jpg"), "/image/thumb/x/256x256bb.jpg");
        assert_eq!(upsize("/image/x.jpg"), "/image/x.jpg");
        assert_eq!(upsize("/100x/a.jpg"), "/100x/a.jpg");
        assert_eq!(upsize("/x/100xbb.jpg"), "/x/100xbb.jpg");
        assert_eq!(upsize("/"), "/");
        assert_eq!(upsize(""), "");
        let art = "https://is1-ssl.mzstatic.com/image/thumb/x/100x100bb.jpg";
        let c = candidates(art);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], ("is1-ssl.mzstatic.com".to_string(), "/image/thumb/x/256x256bb.jpg".to_string()));
        assert_eq!(c[1].1, "/image/thumb/x/100x100bb.jpg");
        assert_eq!(candidates("https://is1-ssl.mzstatic.com/a/256x256bb.jpg").len(), 1);
        let d = candidates("Netflix.com");
        let hosts: Vec<&str> = d.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(hosts, ["netflix.com", "www.netflix.com", "t1.gstatic.com", "icons.duckduckgo.com"]);
        assert_eq!(d[0].1, "/apple-touch-icon.png");
        assert!(d[2].1.starts_with("/faviconV2?") && d[2].1.ends_with("&url=https://netflix.com&size=256"));
        assert_eq!(d[3].1, "/ip3/netflix.com.ico");
        assert_eq!(candidates("www.a.com").len(), 3);
        assert!(candidates("localhost").is_empty());
    }

    #[test]
    fn icons_small_source_is_only_a_fallback() {
        let b = |w| Some((vec![0u8], w));
        assert!(keeps(&None, 16)); // anything beats nothing (a 404 globe never gets here: non-200)
        assert!(keeps(&b(32), 48));
        assert!(!keeps(&b(48), 48)); // ties keep the earlier (better-ranked) source
        assert!(!keeps(&b(180), 64));
        assert!(GOOD > 64 && GOOD <= SIZE as u32); // 64 px Google/DDG icons do not stop the search
    }

    #[test]
    fn icons_valid_key_and_tint() {
        for ok in ["netflix.com", "https://is1-ssl.mzstatic.com/image/x.jpg"] {
            assert!(valid_key(ok), "{ok}");
        }
        for bad in ["", "../x", "a/b.com", "javascript:alert(1)", "localhost", "http://a.mzstatic.com/a.jpg"] {
            assert!(!valid_key(bad), "{bad}");
        }
        let root = std::env::temp_dir().join(format!("subscribity-tint-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let mk = |name: &str, bgr: [u8; 3]| {
            let bits: Vec<u8> = (0..128 * 128).flat_map(|_| bgr).collect();
            let p = root.join(name);
            fs::write(&p, bmp_file(&bits)).unwrap();
            p
        };
        assert_eq!(tint(&mk("red.bmp", [0x20, 0x20, 0xE0])), Some(0xE02020));
        assert_eq!(tint(&mk("grey.bmp", [0x80, 0x80, 0x80])), None);
        assert_eq!(tint(&mk("white.bmp", [0xFF, 0xFF, 0xFF])), None);
        assert_eq!(tint(&mk("black.bmp", [0, 0, 0])), None);
        // White tile with a 32x32 blue square (1024 px): tint is the blue, not diluted by white.
        let mut bits = vec![0xFFu8; 128 * 128 * 3];
        for y in 0..32 {
            for x in 0..32 {
                let i = (y * 128 + x) * 3;
                bits[i..i + 3].copy_from_slice(&[0xC0, 0x40, 0x20]);
            }
        }
        let p = root.join("blue.bmp");
        fs::write(&p, bmp_file(&bits)).unwrap();
        assert_eq!(tint(&p), Some(0x2040C0));
        fs::write(&p, b"BM short").unwrap();
        assert_eq!(tint(&p), None);
        assert_eq!(tint(&root.join("missing.bmp")), None);
        let _ = fs::remove_dir_all(&root);
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
    fn icons_offline_only_without_any_answer() {
        let any = |rs: &[Result<Vec<u8>, u32>]| rs.iter().fold(false, |a, r| a | got_answer(r));
        // Own site hangs, www hangs, Google 404, DuckDuckGo blocked: online, so a marker is written.
        assert!(any(&[Err(0), Err(0), Err(404), Err(0)]));
        assert!(any(&[Err(0), Ok(vec![1])]));
        assert!(any(&[Err(413)]));
        // Nothing answered at all: offline, nothing marked.
        assert!(!any(&[Err(0), Err(0), Err(0), Err(0)]));
    }

    #[test]
    fn icons_ico_entry_and_bmp() {
        // Three entries: 16 px, 256 px (w byte 0), 128 px; data bytes 0x10, 0x20, 0x30.
        let mut b = vec![0, 0, 1, 0, 3, 0];
        let off = 6 + 3 * 16;
        for (i, w) in [16u8, 0, 128].iter().enumerate() {
            b.extend_from_slice(&[*w, *w, 0, 0, 1, 0, 32, 0]);
            b.extend_from_slice(&1u32.to_le_bytes());
            b.extend_from_slice(&((off + i) as u32).to_le_bytes());
        }
        b.extend_from_slice(&[0x10, 0x20, 0x30]);
        assert_eq!(ico_entry(&b), Some((128, &[0x30u8][..]))); // smallest >= 128
        b[6 + 2 * 16] = 32; // third entry now 32 px: 256 wins
        assert_eq!(ico_entry(&b), Some((256, &[0x20u8][..])));
        b[6 + 16] = 24; // 16, 24, 32: largest
        assert_eq!(ico_entry(&b), Some((32, &[0x30u8][..])));
        assert_eq!(ico_entry(&b[..30]), None); // truncated directory
        assert_eq!(ico_entry(b"\x89PNG\r\n\x1a\n"), None);
        let f = bmp_file(&vec![0xFF; 128 * 128 * 3]);
        assert_eq!(f.len(), 54 + 49152);
        assert_eq!(&f[..2], b"BM");
        assert_eq!(u32::from_le_bytes([f[2], f[3], f[4], f[5]]), 49206);
        assert_eq!(u32::from_le_bytes([f[18], f[19], f[20], f[21]]), 128);
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
        let old64 = root.join(DIR).join(format!("{:016x}.bmp", fnv1a("netflix.com"))); // v0.4 64 px file
        fs::write(&old64, b"x").unwrap();
        prune(&root.join(DIR), &[stem("netflix.com")].into_iter().collect());
        assert!(keep.exists() && !gone.exists() && !marker(&gone).exists());
        assert!(!old64.exists()); // old generation refetched, then pruned
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
        assert_eq!(fetch_missing(&root, &keys, MAX_PER_RUN), 2);
        assert!(path(&root, &keys[0]).exists() && path(&root, &keys[1]).exists());
        assert!(marker(&path(&root, &keys[2])).exists() && !pending(&root, &keys));
        let b = load(&path(&root, &keys[0]));
        assert!(!b.is_null());
        unsafe { DeleteObject(b) };
        let _ = fs::copy(path(&root, &keys[1]), root.join("artwork.bmp"));
        assert_eq!(fetch_missing(&root, &keys[..1], MAX_PER_RUN), 0); // cached; others pruned
        assert!(!path(&root, &keys[1]).exists());
    }
}
