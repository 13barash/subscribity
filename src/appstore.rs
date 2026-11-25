//! App Store lookup for App Store subscriptions without a known site: Apple's public iTunes
//! Search API gives the app icon (`artworkUrl100`) and the developer's website (`sellerUrl`).
//! Only the app name (and the store country) is sent. Window scans only, a few per scan, each
//! with a 5 s timeout; results are cached in scan.tsv (`a` lines). Declared from scan.rs.
//! Own small GET (update.rs's has no timeouts and is protected code), also used by icons.rs.
use crate::services::{norm, registrable};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Networking::WinHttp::*;

pub const MAX_PER_SCAN: usize = 20;
const TIMEOUT_MS: i32 = 5000;
const LIMIT: usize = 256 << 10;

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Percent-encodes everything but unreserved characters (RFC 3986).
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// HTTPS GET of `path` on `host`, 5 s timeouts, body up to 256 KB. Redirects to https are
/// followed (e.g. apex → www); a redirect from https down to http is not.
pub(crate) fn get(host: &str, path: &str) -> Option<Vec<u8>> {
    fetch(host, path).ok()
}

/// Like `get`; Err(status) when the server answered something other than a complete 200
/// (Err(413): body over 256 KB; Err(0): no answer at all, i.e. offline, timeout, cut-off body).
pub(crate) fn fetch(host: &str, path: &str) -> Result<Vec<u8>, u32> {
    unsafe {
        let ses = WinHttpOpen(w("Subscribity").as_ptr(), 4 /* automatic proxy */, null(), null(), 0);
        if ses.is_null() {
            return Err(0);
        }
        WinHttpSetTimeouts(ses, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS);
        let con = WinHttpConnect(ses, w(host).as_ptr(), 443, 0);
        let req = if con.is_null() { null_mut() } else {
            WinHttpOpenRequest(con, w("GET").as_ptr(), w(path).as_ptr(), null(), null(), null(), 0x0080_0000 /* secure */)
        };
        let mut out = Err(0);
        if !req.is_null() {
            let mut policy = 1u32; // WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP: https→https redirects are followed
            WinHttpSetOption(req, 88 /* REDIRECT_POLICY */, &mut policy as *mut u32 as *mut c_void, 4);
            if WinHttpSendRequest(req, null(), 0, null(), 0, 0, 0) != 0 && WinHttpReceiveResponse(req, null_mut()) != 0 {
                let (mut status, mut size) = (0u32, 4u32);
                WinHttpQueryHeaders(req, 19 | 0x2000_0000, null(), &mut status as *mut u32 as *mut c_void, &mut size, null_mut());
                let mut body = Vec::new();
                if status != 200 {
                    out = Err(status);
                }
                while status == 200 {
                    let mut avail = 0u32;
                    if WinHttpQueryDataAvailable(req, &mut avail) == 0 {
                        break;
                    }
                    if body.len() + avail as usize > LIMIT {
                        out = Err(413); // too large: a lasting answer, not a network error
                        break;
                    }
                    if avail == 0 {
                        out = Ok(std::mem::take(&mut body));
                        break;
                    }
                    let mut buf = vec![0u8; avail as usize];
                    let mut read = 0u32;
                    if WinHttpReadData(req, buf.as_mut_ptr() as *mut c_void, avail, &mut read) == 0 {
                        break;
                    }
                    body.extend_from_slice(&buf[..read as usize]);
                }
            }
            WinHttpCloseHandle(req);
        }
        if !con.is_null() {
            WinHttpCloseHandle(con);
        }
        WinHttpCloseHandle(ses);
        out
    }
}

/// Host of an https URL, lowercase, no port.
fn https_host(u: &str) -> Option<String> {
    let rest = u.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?.split(':').next()?.to_ascii_lowercase();
    (!host.is_empty() && !host.contains('@') && host.contains('.')).then_some(host)
}

/// An https URL on Apple's artwork CDN (mzstatic.com), at most 300 characters, no spaces or
/// tabs: the only image URLs stored on subscriptions and fetched for the tray.
pub(crate) fn is_artwork(u: &str) -> bool {
    u.len() <= 300
        && !u.contains(|c: char| c.is_whitespace() || c.is_control())
        && https_host(u).is_some_and(|h| h == "mzstatic.com" || h.ends_with(".mzstatic.com"))
}

/// (icon, site) from a search answer; ("", "") when no app matches `name`. The app must share
/// its first word with `name`, the icon must be on Apple's CDN.
pub fn parse(body: &[u8], name: &str) -> (String, String) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else { return Default::default() };
    let r = &v["results"][0];
    let first = norm(name.split_whitespace().next().unwrap_or(""));
    let track = norm(r["trackName"].as_str().unwrap_or(""));
    if first.is_empty() || !(track.contains(&first) || (!track.is_empty() && first.contains(&track))) {
        return Default::default();
    }
    let art = r["artworkUrl100"].as_str().unwrap_or("");
    let icon = if is_artwork(art) { art.to_string() } else { String::new() };
    let site = r["sellerUrl"].as_str().and_then(|u| https_host(u).or_else(|| https_host(&u.replacen("http://", "https://", 1))));
    let site = site.map(|h| registrable(&h)).filter(|s| s != "apple.com" || first == "apple").unwrap_or_default();
    (icon, site)
}

/// Looks `name` up in the store of `country` ("il", "us"). None: network error (try again later).
pub fn lookup(name: &str, country: &str) -> Option<(String, String)> {
    let path = format!("/search?term={}&entity=software&limit=1&country={}", encode(name.trim()), encode(country));
    get("itunes.apple.com", &path).map(|b| parse(&b, name))
}

/// Store country for the main currency.
pub fn country(main_cur: &str) -> &'static str {
    if main_cur.eq_ignore_ascii_case("ILS") { "il" } else { "us" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appstore_parse_answers() {
        let ok = br#"{"resultCount":1,"results":[{"trackName":"Acme Tracker: Food Log","artworkUrl100":"https://is1-ssl.mzstatic.com/image/thumb/x/100x100bb.jpg","sellerUrl":"https://www.acme-tracker.example.co.uk/app"}]}"#;
        let (icon, site) = parse(ok, "Acme Tracker");
        assert_eq!((icon.as_str(), site.as_str()), ("https://is1-ssl.mzstatic.com/image/thumb/x/100x100bb.jpg", "example.co.uk"));
        // Another app, a foreign icon host, http seller URL, nothing found, garbage.
        assert_eq!(parse(ok, "Other App"), Default::default());
        let bad = br#"{"results":[{"trackName":"Acme","artworkUrl100":"https://evil.example/mzstatic.com/a.jpg","sellerUrl":"http://acme.example"}]}"#;
        assert_eq!(parse(bad, "Acme"), (String::new(), "acme.example".into()));
        assert_eq!(parse(br#"{"resultCount":0,"results":[]}"#, "Acme"), Default::default());
        assert_eq!(parse(b"<html>", "Acme"), Default::default());
        assert_eq!(encode("Me PRO+ & co"), "Me%20PRO%2B%20%26%20co");
        assert_eq!(country("ILS"), "il");
        assert!(is_artwork("https://is1-ssl.mzstatic.com/image/thumb/x/100x100bb.jpg"));
        assert!(!is_artwork("https://evil.example/mzstatic.com/a.jpg"));
        assert!(!is_artwork("http://is1-ssl.mzstatic.com/a.jpg"));
        assert!(!is_artwork("https://notmzstatic.com/a.jpg"));
        assert!(!is_artwork("https://a.mzstatic.com/a b.jpg"));
        assert!(!is_artwork(&format!("https://a.mzstatic.com/{}", "x".repeat(300))));
    }

    #[test]
    #[ignore]
    fn appstore_live_lookup() {
        for n in ["Instagram", "MacroFactor"] {
            let (icon, site) = lookup(n, "il").expect("network");
            println!("{n}: icon host {:?}, site {site}", https_host(&icon));
            assert!(https_host(&icon).is_some_and(|h| h.ends_with(".mzstatic.com")));
        }
    }
}
