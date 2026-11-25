//! Per-subscription actions and lookups shared by the tray and the window: manage / cancel links,
//! the "not using it anymore" flag, price-increase alerts, settings lists, and the window's
//! `http://icon.localhost/<host>` image protocol backed by the icons.rs cache.
use crate::data::{clean_list, money, Store, Sub};
use crate::{icons, services};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use wry::http::{Response, StatusCode};

/// Max length of a stored or opened manage link.
const URL_MAX: usize = 200;

/// A link the user typed (or a website) as something safe to hand to ShellExecute: `http(s)://`
/// URLs, or a bare domain (`netflix.com/account` → `https://netflix.com/account`). Anything else
/// (`javascript:`, `file:`, spaces, control characters, `user@host`, no dot in the host) → "".
pub fn clean_url(u: &str) -> String {
    let u = u.trim();
    let bad = |c: char| c.is_control() || c.is_whitespace() || matches!(c, '"' | '<' | '>' | '\\' | '`' | '^' | '{' | '}' | '|');
    if u.is_empty() || u.len() > URL_MAX || u.chars().any(bad) {
        return String::new();
    }
    let lower = u.to_ascii_lowercase();
    let (scheme, rest, bare) = if lower.starts_with("https://") {
        ("https://", &u[8..], false)
    } else if lower.starts_with("http://") {
        ("http://", &u[7..], false)
    } else {
        ("https://", u, true)
    };
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port) = host_port.split_once(':').unwrap_or((host_port, ""));
    let host_ok = host.contains('.')
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && !host.contains("..")
        && host.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '.');
    // A bare entry never has a colon before its path ("javascript:", "mailto:", "a.com:80").
    let port_ok = port.is_empty() || (!bare && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()));
    if !host_ok || !port_ok {
        return String::new();
    }
    let out = format!("{}{}", scheme, rest);
    if out.len() > URL_MAX { String::new() } else { out }
}

/// Where "Manage / cancel" goes: 1 the user's own link; 2 App Store item (artwork icon or paid
/// through Apple, not Apple Pay) → Apple subscriptions; 3 Google Play; 4 PayPal automatic
/// payments; 5 the built-in page of a known service (by name, else by site); 6 the website;
/// 7 "" (no button).
pub fn manage_for(s: &Sub) -> String {
    let own = clean_url(&s.manage);
    if !own.is_empty() {
        return own;
    }
    let pay = s.payment.to_lowercase();
    if !s.icon.is_empty() || (pay.contains("apple") && !pay.contains("apple pay")) {
        return services::APPLE_SUBS.into();
    }
    if pay.contains("google play") {
        return services::GOOGLE_PLAY_SUBS.into();
    }
    if pay.contains("paypal") {
        return services::PAYPAL_AUTOPAY.into();
    }
    let known = services::manage_url(&s.name, &s.url);
    if !known.is_empty() {
        return known.into();
    }
    clean_url(&s.url)
}

/// Flags (`on`, keeping an earlier flag day) or un-flags subscription `id`.
/// True when the stored value changed (the caller saves).
pub fn set_unused(st: &mut Store, id: u32, on: bool, today: i32) -> bool {
    let Some(s) = st.subs.iter_mut().find(|s| s.id == id) else { return false };
    let v = if !on { 0 } else if s.unused > 0 { s.unused } else { today };
    if s.unused == v {
        return false;
    }
    s.unused = v;
    true
}

/// Tray: the scanner's `scan_price_up` entries not announced yet, as "Netflix: $15.49 → $17.99".
/// An entry counts while its sub exists, is active, has the same currency and the charge is
/// still > 1% above the saved price. Announced keys (`id|price`) of entries that are gone are
/// dropped (kept while only the sub is missing, until the next scan replaces the entries); with `mark` (the alerts are being shown) the returned ones count as announced.
/// state.tsv is saved when anything changed.
pub fn fresh_price_alerts(st: &mut Store, mark: bool) -> Vec<String> {
    let mut live = HashSet::new();
    let mut out = Vec::new();
    for (id, price, cur) in &st.settings.price_up {
        let key = format!("{}|{:.2}", id, price);
        let Some(s) = st.subs.iter().find(|s| s.id == *id) else {
            // Sub gone (maybe deleted and about to be undone): keep it announced, not re-announced.
            if st.price_alerted.contains(&key) {
                live.insert(key);
            }
            continue;
        };
        if !s.active || s.cur != *cur || *price <= s.price + s.price.abs() * 0.01 {
            continue;
        }
        if !st.price_alerted.contains(&key) && !live.contains(&key) {
            out.push(format!("{}: {} → {}", s.name, money(s.price, &s.cur), money(*price, cur)));
        }
        live.insert(key);
    }
    let before = st.price_alerted.len();
    st.price_alerted.retain(|k| live.contains(k));
    let dropped = st.price_alerted.len() != before;
    let marked = mark && !out.is_empty();
    if marked {
        st.price_alerted.extend(live);
    }
    if dropped || marked {
        st.save_state();
    }
    out
}

/// Windows cuts a balloon body at 255 UTF-16 units.
pub const BALLOON_MAX: usize = 255;

/// Balloon body: up to `cap` of `lines`, as many as fit with a "+N more" line for the rest and
/// the optional `tail` line, all within `max` UTF-16 units (the tail always survives).
pub fn balloon_body(lines: &[String], cap: usize, tail: Option<&str>, max: usize) -> String {
    let len = |s: &str| s.encode_utf16().count();
    let n = lines.len();
    for k in (0..=n.min(cap)).rev() {
        let mut v: Vec<String> = lines[..k].to_vec();
        if k < n {
            v.push(format!("+{} more", n - k));
        }
        v.extend(tail.map(str::to_string));
        let body = v.join("\n");
        if len(&body) <= max {
            return body;
        }
    }
    // Even "+N more" and the tail don't fit (tiny max): keep what fits of the tail.
    tail.map_or(String::new(), |t| t.chars().take(max).collect())
}

/// A website or host → the icons.rs cache key for it: its registrable domain ("" if none).
pub fn site_key(site: &str) -> String {
    let u = site.trim();
    let u = u.split_once("://").map_or(u, |x| x.1);
    let host = u.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or("").split(':').next().unwrap_or("");
    if host.contains('.') { services::registrable(host) } else { String::new() }
}

/// Window: `http://icon.localhost/<host>?v=N` → the cached 128x128 BMP of the host's registrable
/// domain (200 `image/bmp`), else 404 so the page keeps its letter tile.
pub fn icon_response(dir: &Path, uri_path: &str) -> Response<Cow<'static, [u8]>> {
    let key = site_key(uri_path.trim_start_matches('/'));
    let body = if icons::valid_key(&key) { fs::read(icons::path(dir, &key)).ok().filter(|b| b.len() > 54) } else { None };
    let built = match body {
        // The URL carries `?v=<iconsV>`, bumped whenever the cache changes, so a hit can be kept.
        Some(b) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "image/bmp")
            .header("Cache-Control", "max-age=31536000")
            .header("Access-Control-Allow-Origin", "*")
            .body(Cow::Owned(b)),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("Cache-Control", "no-store")
            .header("Access-Control-Allow-Origin", "*")
            .body(Cow::Borrowed(&[][..])),
    };
    built.unwrap_or_else(|_| {
        let mut r = Response::new(Cow::Borrowed(&[][..]));
        *r.status_mut() = StatusCode::NOT_FOUND;
        r
    })
}

/// Window: `http://icon.localhost/__wallpaper?v=N` → the pre-blurred wallpaper BMP (404 if none).
pub fn bmp_response(bmp: &[u8]) -> Response<Cow<'static, [u8]>> {
    let ok = bmp.len() > 54;
    Response::builder()
        .status(if ok { StatusCode::OK } else { StatusCode::NOT_FOUND })
        .header("Content-Type", "image/bmp")
        .header("Cache-Control", "no-store")
        .header("Access-Control-Allow-Origin", "*")
        .body(Cow::Owned(if ok { bmp.to_vec() } else { Vec::new() }))
        .unwrap_or_else(|_| Response::new(Cow::Borrowed(&[][..])))
}

thread_local! {
    static TINTS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    static ICONS_V: Cell<u32> = const { Cell::new(1) };
}

/// Brand color "#rrggbb" of the cached icon for `key` ("" for none/grey), memoized until
/// `icons_changed`.
pub fn tint_of(dir: &Path, key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    TINTS.with(|t| {
        t.borrow_mut()
            .entry(key.to_string())
            .or_insert_with(|| icons::tint(&icons::path(dir, key)).map_or(String::new(), |c| format!("#{:06x}", c & 0xFF_FFFF)))
            .clone()
    })
}

/// `state.iconsV`: bumped when the icon helper saved new images.
pub fn icons_v() -> u32 {
    ICONS_V.with(|v| v.get())
}

/// The icon cache changed: new `iconsV`, tints read again.
pub fn icons_changed() {
    TINTS.with(|t| t.borrow_mut().clear());
    ICONS_V.with(|v| v.set(v.get() + 1));
}

/// A settings list for the window: the managed one, or (never edited) the values in `used`,
/// deduped and sorted case-insensitively.
fn list_or_used<'a>(managed: &Option<Vec<String>>, used: impl Iterator<Item = &'a str>) -> Vec<String> {
    managed.clone().unwrap_or_else(|| {
        let mut v = clean_list(used.map(str::to_string));
        v.sort_by_key(|x| x.to_lowercase());
        v
    })
}

/// `state.settings.payMethods`.
pub fn pay_methods(st: &Store) -> Vec<String> {
    list_or_used(&st.settings.pay_methods, st.subs.iter().map(|s| s.payment.as_str()))
}

/// `state.settings.categories`: never edited = subs' categories + quick-add template categories.
pub fn categories(st: &Store) -> Vec<String> {
    let tpl = services::SERVICES.iter().filter(|s| s.tpl).map(|s| s.category);
    list_or_used(&st.settings.categories, st.subs.iter().map(|s| s.category.as_str()).chain(tpl))
}

/// After a save: a category / payment method typed in the form joins its list
/// (case-insensitive). A never-edited list is first materialized from the derived one (so
/// nothing shown before is lost), then extended. Call before the sub is stored (else a derived
/// list already contains the value and stays derived).
pub fn remember_in_lists(st: &mut Store, category: &str, payment: &str) {
    let add = |list: Vec<String>, v: &str| -> Option<Vec<String>> {
        let v = v.trim();
        (!v.is_empty() && !list.iter().any(|x| x.eq_ignore_ascii_case(v))).then(|| clean_list(list.into_iter().chain([v.to_string()])))
    };
    let (c, p) = (add(categories(st), category), add(pay_methods(st), payment));
    if c.is_some() || p.is_some() {
        st.edit_settings(|s| {
            if let Some(c) = c {
                s.categories = Some(c);
            }
            if let Some(p) = p {
                s.pay_methods = Some(p);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::days_from_civil;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("subscribity-test-actions-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn clean_url_accepts_http_and_bare_domains_only() {
        assert_eq!(clean_url(" https://www.netflix.com/account "), "https://www.netflix.com/account");
        assert_eq!(clean_url("HTTP://Example.com"), "http://Example.com");
        assert_eq!(clean_url("netflix.com/account?x=1#y"), "https://netflix.com/account?x=1#y");
        assert_eq!(clean_url("https://a.com:8443/x"), "https://a.com:8443/x");
        assert_eq!(clean_url("https://chatgpt.com/#settings/Subscription"), "https://chatgpt.com/#settings/Subscription");
        for bad in [
            "javascript:alert(1)", "JavaScript://a.com/%0aalert(1)", "file:///C:/Windows/notepad.exe", "file://server/share",
            "mailto:a@b.com", "https://a.com/x y", "https://a.com/\tx", "a.com\nb", "https://", "https://nodot/x",
            "localhost:3000", "a.com:80", "https://a.com:99999x", "https://user@evil.com", "\"https://a.com\"",
            "https://a.com\\..\\x", "ftp://a.com", "C:\\Windows\\x.exe", "..", "-a.com", "https://a..com", "",
        ] {
            assert_eq!(clean_url(bad), "", "{:?}", bad);
        }
        assert_eq!(clean_url(&format!("https://a.com/{}", "x".repeat(200))), "");
        assert_eq!(clean_url(&format!("{}.com", "x".repeat(193))), ""); // 197 chars, over 200 with https://
    }

    #[test]
    fn builtin_manage_urls_pass_clean_url() {
        for s in services::SERVICES.iter().filter(|s| !s.manage.is_empty()) {
            assert_eq!(clean_url(s.manage), s.manage, "{}", s.name);
        }
        for u in [services::APPLE_SUBS, services::GOOGLE_PLAY_SUBS, services::PAYPAL_AUTOPAY] {
            assert_eq!(clean_url(u), u);
        }
    }

    #[test]
    fn manage_for_resolution_order() {
        let sub = |name: &str, url: &str, payment: &str| Sub { name: name.into(), url: url.into(), payment: payment.into(), ..Default::default() };
        // 1 own link wins, bare domain becomes https; a bad own link is ignored.
        let mut s = sub("Netflix", "netflix.com", "Apple");
        s.manage = "mysite.com/cancel".into();
        assert_eq!(manage_for(&s), "https://mysite.com/cancel");
        s.manage = "javascript:alert(1)".into();
        assert_eq!(manage_for(&s), services::APPLE_SUBS);
        // 2 App Store artwork beats a known service; Apple Pay is a wallet, not the App Store.
        let mut s = sub("Netflix", "netflix.com", "Visa");
        s.icon = "https://is1-ssl.mzstatic.com/a.png".into();
        assert_eq!(manage_for(&s), services::APPLE_SUBS);
        assert_eq!(manage_for(&sub("Netflix", "netflix.com", "Apple Pay")), manage_for(&sub("Netflix", "netflix.com", "")));
        // 3, 4 stores.
        assert_eq!(manage_for(&sub("Some Game", "", "Google Play")), services::GOOGLE_PLAY_SUBS);
        assert_eq!(manage_for(&sub("Some Tool", "tool.io", "PayPal")), services::PAYPAL_AUTOPAY);
        // 5 known service by name, or by a subdomain of its site.
        let n = services::manage_url("Netflix", "");
        assert!(n.starts_with("https://"));
        assert_eq!(manage_for(&sub("Netflix", "", "Visa")), n);
        assert_eq!(manage_for(&sub("My movies", "https://www.netflix.com/browse", "")), n);
        // 6 website, 7 nothing.
        assert_eq!(manage_for(&sub("Acme Widgets", "acme-widgets.example/billing", "")), "https://acme-widgets.example/billing");
        assert_eq!(manage_for(&sub("Acme Widgets", "file:///c:/x", "")), "");
        assert_eq!(manage_for(&sub("Acme Widgets", "", "")), "");
    }

    #[test]
    fn set_unused_flags_keeps_day_and_clears() {
        let dir = temp_dir("unused");
        let mut st = Store::load(dir.clone());
        st.subs.push(Sub { id: 3, name: "A".into(), active: true, ..Default::default() });
        let (d1, d2) = (days_from_civil(2026, 11, 24), days_from_civil(2026, 12, 1));
        assert!(set_unused(&mut st, 3, true, d1));
        assert!(!set_unused(&mut st, 3, true, d2)); // already flagged: keeps the first day
        assert_eq!(st.subs[0].unused, d1);
        assert!(set_unused(&mut st, 3, false, d2));
        assert_eq!(st.subs[0].unused, 0);
        assert!(!set_unused(&mut st, 3, false, d2));
        assert!(!set_unused(&mut st, 99, true, d1)); // unknown id
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn balloon_body_keeps_tail_within_limit() {
        let len = |s: &str| s.encode_utf16().count();
        let tail = "Not using one? Click to review.";
        let short: Vec<String> = (0..3).map(|i| format!("Sub {} $9.99 – tomorrow", i)).collect();
        assert_eq!(balloon_body(&short, 4, Some(tail), BALLOON_MAX), format!("{}\n{}", short.join("\n"), tail));
        let six: Vec<String> = (0..6).map(|i| format!("S{}", i)).collect();
        assert_eq!(balloon_body(&six, 4, None, BALLOON_MAX), "S0\nS1\nS2\nS3\n+2 more");
        // Long names: lines are dropped (counted in "+N more") until the tail fits.
        let long: Vec<String> = (0..4).map(|i| format!("{} {} ₪49.90 – in 3 days", i, "Very long subscription name ".repeat(3))).collect();
        let b = balloon_body(&long, 4, Some(tail), BALLOON_MAX);
        assert!(len(&b) <= BALLOON_MAX && b.ends_with(tail) && b.contains(" more\n"), "{}", b);
        assert!(b.starts_with(&long[0]));
        // Nothing fits but the tail.
        let huge = vec!["x".repeat(400)];
        assert_eq!(balloon_body(&huge, 4, Some(tail), BALLOON_MAX), format!("+1 more\n{}", tail));
        assert_eq!(balloon_body(&huge, 4, Some(tail), 10), "Not using ");
        assert_eq!(balloon_body(&[], 4, None, BALLOON_MAX), "");
    }

    #[test]
    fn fresh_price_alerts_once_per_price_and_cleanup() {
        let dir = temp_dir("price");
        let mut st = Store::load(dir.clone());
        let sub = |id: u32, name: &str, price: f64, cur: &str| Sub { id, name: name.into(), price, cur: cur.into(), every: 1, unit: 2, active: true, ..Default::default() };
        st.subs = vec![sub(1, "Netflix", 15.49, "USD"), sub(2, "Spotify", 10.0, "USD"), sub(3, "Paused", 5.0, "USD")];
        st.subs[2].active = false;
        st.settings.price_up = vec![
            (1, 17.99, "USD".into()), (2, 10.1, "USD".into()), (2, 12.0, "EUR".into()), (3, 9.0, "USD".into()), (9, 1.0, "USD".into()),
        ];
        // Not shown (a more urgent balloon won): nothing marked, they come again next time.
        assert_eq!(fresh_price_alerts(&mut st, false).len(), 1);
        assert!(st.price_alerted.is_empty() && !dir.join("state.tsv").exists());
        let first = fresh_price_alerts(&mut st, true);
        assert_eq!(first, vec!["Netflix: $15.49 → $17.99".to_string()]);
        assert!(Store::load(dir.clone()).price_alerted.contains("1|17.99")); // saved
        assert!(fresh_price_alerts(&mut st, true).is_empty()); // same price again
        st.settings.price_up = vec![(1, 19.99, "USD".into())]; // further increase
        assert_eq!(fresh_price_alerts(&mut st, true).len(), 1);
        assert_eq!(st.price_alerted, HashSet::from(["1|19.99".to_string()]));
        st.subs[0].price = 19.99; // price updated in the app: entry no longer counts, key dropped
        assert!(fresh_price_alerts(&mut st, true).is_empty());
        assert!(st.price_alerted.is_empty());
        // Deleted, then undone: the announced alert stays announced.
        st.subs[0].price = 15.49;
        assert_eq!(fresh_price_alerts(&mut st, true).len(), 1);
        let kept = st.subs.remove(0);
        assert!(fresh_price_alerts(&mut st, true).is_empty() && st.price_alerted.contains("1|19.99"));
        st.subs.push(kept);
        assert!(fresh_price_alerts(&mut st, true).is_empty());
        st.subs.clear(); // sub deleted
        st.settings.price_up = vec![(1, 25.0, "USD".into())];
        assert!(fresh_price_alerts(&mut st, true).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn site_key_and_icon_protocol() {
        assert_eq!(site_key("https://www.netflix.com/account"), "netflix.com");
        assert_eq!(site_key("tv.apple.com"), "apple.com");
        assert_eq!(site_key("user@mail.acme.co.uk:443"), "acme.co.uk");
        assert_eq!(site_key("nodot"), "");
        let dir = temp_dir("icons");
        fs::create_dir_all(dir.join(icons::DIR)).unwrap();
        let bmp: Vec<u8> = (0..200u8).collect();
        fs::write(icons::path(&dir, "netflix.com"), &bmp).unwrap();
        let r = icon_response(&dir, "/www.netflix.com");
        assert_eq!((r.status(), r.headers()["Content-Type"].to_str().unwrap(), r.body().len()), (StatusCode::OK, "image/bmp", 200));
        for miss in ["/spotify.com", "/", "/..%2f..%2fsecret", "/C:\\Windows\\win.ini", "/nodot"] {
            assert_eq!(icon_response(&dir, miss).status(), StatusCode::NOT_FOUND, "{}", miss);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn lists_derive_until_edited_and_remember_new_values() {
        let dir = temp_dir("lists");
        let mut st = Store::load(dir.clone());
        st.subs.push(Sub { category: "zeta".into(), payment: "Visa".into(), ..Default::default() });
        let cats = categories(&st);
        assert!(cats.contains(&"zeta".to_string()) && cats.len() > 1);
        assert!(cats.windows(2).all(|w| w[0].to_lowercase() <= w[1].to_lowercase()));
        assert_eq!(pay_methods(&st), vec!["Visa".to_string()]);
        remember_in_lists(&mut st, "zeta", "visa"); // already derived: lists stay never-edited
        assert!(st.settings.categories.is_none() && st.settings.pay_methods.is_none());
        remember_in_lists(&mut st, "New", "Amex"); // new values: derived lists materialized + appended
        let s = Store::load(dir.clone()).settings;
        let c = s.categories.unwrap();
        assert!(c.len() == cats.len() + 1 && c.last().unwrap() == "New" && c.contains(&"zeta".to_string()));
        assert_eq!(s.pay_methods.unwrap(), vec!["Visa".to_string(), "Amex".into()]);
        st.edit_settings(|s| {
            s.categories = Some(vec!["Music".into()]);
            s.pay_methods = Some(Vec::new());
        });
        remember_in_lists(&mut st, "music", "Amex");
        remember_in_lists(&mut st, "Gym", " ");
        let s = Store::load(dir.clone()).settings;
        assert_eq!(s.categories.unwrap(), vec!["Music".to_string(), "Gym".into()]);
        assert_eq!(s.pay_methods.unwrap(), vec!["Amex".to_string()]);
        let _ = fs::remove_dir_all(dir);
    }
}
