//! Scanner storage: the DPAPI-protected mailbox login (`mail.key`) and the scan cache
//! (`scan.tsv`). Declared from scan.rs (`scan::store`).
//!
//! `scan.tsv` lines (tab-separated, text fields escaped like subscriptions.tsv):
//! - `u  mailbox  uidvalidity  last_uid`: where the last successful mail scan stopped
//! - `k  key`: a candidate the user skipped
//! - `h  key name date price cur every:unit trial renews site category flags src`: one charge
//!   (24 newest per service, 2000 overall); candidates are regrouped from these
//! - `c  key name price cur every unit last next category site score count src trial first method
//!   icon anchor`: the last grouping, used only when there are no `h` lines (trailing fields
//!   optional; `anchor` v0.3.2, missing = `first`)
//! - `a  key icon site`: App Store lookup result for an App Store item (v0.3.0; empty = none found)
//!
//! Unknown line types and extra trailing fields are ignored, so newer versions can add them.
use crate::data::{esc, iso, parse_iso, unesc, write_atomic, Sub, MAIL_KEY_FILE, SCAN_FILE};
use crate::detect::{self, Candidate, Charge};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};

const H_PER_KEY: usize = 24;
const H_TOTAL: usize = 2000;
const ENTROPY: &[u8] = b"Subscribity mail.key v1";

// ---------- mail.key ----------

pub struct Login {
    pub host: String,
    pub user: String,
    pub pass: String,
}

impl Drop for Login {
    fn drop(&mut self) {
        // Zero the password before the memory is freed (best effort).
        unsafe { self.pass.as_bytes_mut().fill(0) };
    }
}

/// CryptProtectData / CryptUnprotectData for the current Windows user.
fn dpapi(data: &[u8], protect: bool) -> Option<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        let ok = if protect {
            CryptProtectData(&input, null(), &entropy, null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        } else {
            CryptUnprotectData(&input, null_mut(), &entropy, null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        if ok == 0 || out.pbData.is_null() {
            return None;
        }
        let mut v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        if !protect {
            // The plain copy is ours now; wipe the system buffer before freeing it.
            std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize).fill(0);
        }
        LocalFree(out.pbData as _);
        v.shrink_to_fit();
        Some(v)
    }
}

pub fn has_key(dir: &Path) -> bool {
    dir.join(MAIL_KEY_FILE).exists()
}

pub fn load_key(dir: &Path) -> Option<Login> {
    let blob = fs::read(dir.join(MAIL_KEY_FILE)).ok()?;
    let mut plain = dpapi(&blob, false)?;
    let login = std::str::from_utf8(&plain).ok().and_then(|t| {
        let mut p = t.splitn(3, '\n');
        let (host, user, pass) = (p.next()?, p.next()?, p.next()?);
        Some(Login { host: host.into(), user: user.into(), pass: pass.into() })
    });
    plain.fill(0);
    login.filter(|l| !l.host.is_empty() && !l.user.is_empty())
}

pub fn save_key(dir: &Path, l: &Login) -> bool {
    let mut plain = format!("{}\n{}\n{}", l.host, l.user, l.pass).into_bytes();
    let blob = dpapi(&plain, true);
    plain.fill(0);
    blob.is_some_and(|b| write_atomic(&dir.join(MAIL_KEY_FILE), b))
}

pub fn delete_key(dir: &Path) {
    let _ = fs::remove_file(dir.join(MAIL_KEY_FILE));
}

// ---------- scan.tsv ----------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Uid {
    pub mailbox: String,
    pub uidvalidity: u32,
    pub last_uid: u32,
}

#[derive(Default)]
pub struct ScanFile {
    pub u: Option<Uid>,
    pub skipped: HashSet<String>,
    pub h: Vec<Charge>,
    pub c: Vec<Candidate>,
    pub apps: HashMap<String, (String, String)>, // App Store lookups: key → (icon, site)
}

fn period_text(p: Option<(u32, u8)>) -> String {
    p.map_or(String::new(), |(e, u)| format!("{}:{}", e, u))
}

fn opt_day(d: Option<i32>) -> String {
    d.map_or(String::new(), iso)
}

fn charge_line(c: &Charge) -> String {
    format!(
        "h\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        esc(&c.key), esc(&c.name), iso(c.date), c.price, c.cur, period_text(c.period), c.trial as u8,
        opt_day(c.renews), esc(&c.site), esc(&c.category), c.score_parts, c.src, esc(&c.method)
    )
}

fn parse_charge(f: &[&str]) -> Option<Charge> {
    if f.len() < 13 {
        return None;
    }
    let period = f[6].split_once(':').and_then(|(e, u)| Some((e.parse().ok()?, u.parse::<u8>().ok()?.min(3))));
    let c = Charge {
        key: unesc(f[1]),
        name: unesc(f[2]),
        date: parse_iso(f[3])?,
        price: f[4].parse().ok().filter(|p: &f64| p.is_finite() && *p >= 0.0)?,
        cur: f[5].into(),
        period,
        trial: f[7] == "1",
        renews: parse_iso(f[8]),
        site: unesc(f[9]),
        category: unesc(f[10]),
        score_parts: f[11].parse().unwrap_or(0),
        src: f[12].parse().unwrap_or(0),
        method: f.get(13).map_or(String::new(), |m| unesc(m)), // added in v0.3.0: optional
    };
    (!c.key.is_empty() && c.cur.len() == 3).then_some(c)
}

fn candidate_line(c: &Candidate) -> String {
    format!(
        "c\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        esc(&c.key), esc(&c.name), c.price, c.cur, c.every, c.unit, iso(c.last), iso(c.next),
        esc(&c.category), esc(&c.site), c.score, c.count, c.src, c.trial as u8, iso(c.first), esc(&c.method), esc(&c.icon),
        iso(c.anchor)
    )
}

fn parse_candidate(f: &[&str]) -> Option<Candidate> {
    if f.len() < 15 {
        return None;
    }
    let first = f.get(15).and_then(|d| parse_iso(d)).or(parse_iso(f[7]))?;
    Some(Candidate {
        key: unesc(f[1]),
        name: unesc(f[2]),
        price: f[3].parse().ok()?,
        cur: f[4].into(),
        every: f[5].parse().unwrap_or(1),
        unit: f[6].parse::<u8>().unwrap_or(2).min(3),
        last: parse_iso(f[7])?,
        next: parse_iso(f[8])?,
        category: unesc(f[9]),
        site: unesc(f[10]),
        score: f[11].parse().unwrap_or(0),
        count: f[12].parse().unwrap_or(1),
        src: f[13].parse().unwrap_or(0),
        trial: f[14] == "1",
        // Optional trailing fields (v0.3.0): first payment, payment method. Older lines: last charge.
        first,
        method: f.get(16).map_or(String::new(), |m| unesc(m)),
        icon: f.get(17).map_or(String::new(), |m| unesc(m)),
        // v0.3.2: schedule anchor; older lines: = first.
        anchor: f.get(18).and_then(|d| parse_iso(d)).unwrap_or(first),
        ..Default::default()
    })
}

impl ScanFile {
    pub fn load(dir: &Path) -> ScanFile {
        let mut s = ScanFile::default();
        let Ok(t) = fs::read_to_string(dir.join(SCAN_FILE)) else { return s };
        for line in t.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f[0] {
                "u" if f.len() >= 4 => {
                    s.u = Some(Uid { mailbox: unesc(f[1]), uidvalidity: f[2].parse().unwrap_or(0), last_uid: f[3].parse().unwrap_or(0) })
                }
                "k" if f.len() >= 2 && !f[1].is_empty() => {
                    s.skipped.insert(unesc(f[1]));
                }
                "h" => s.h.extend(parse_charge(&f)),
                "c" => s.c.extend(parse_candidate(&f)),
                "a" if f.len() >= 4 && !f[1].is_empty() => {
                    s.apps.insert(unesc(f[1]), (unesc(f[2]), unesc(f[3])));
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self, dir: &Path) -> bool {
        let mut t = String::new();
        if let Some(u) = &self.u {
            t.push_str(&format!("u\t{}\t{}\t{}\n", esc(&u.mailbox), u.uidvalidity, u.last_uid));
        }
        let mut keys: Vec<&String> = self.skipped.iter().collect();
        keys.sort();
        for k in keys {
            t.push_str(&format!("k\t{}\n", esc(k)));
        }
        for c in &self.h {
            t.push_str(&charge_line(c));
        }
        for c in &self.c {
            t.push_str(&candidate_line(c));
        }
        let mut apps: Vec<_> = self.apps.iter().collect();
        apps.sort();
        for (k, (icon, site)) in apps {
            t.push_str(&format!("a\t{}\t{}\t{}\n", esc(k), esc(icon), esc(site)));
        }
        write_atomic(&dir.join(SCAN_FILE), t)
    }

    /// Adds charges (exact repeats dropped), then keeps the newest 24 per service and 2000 in
    /// all. `replace_mail`: a complete mail scan replaces the mail charges kept so far.
    pub fn merge(&mut self, new: Vec<Charge>, replace_mail: bool) {
        if replace_mail {
            self.h.retain(|c| c.src != 0);
        }
        for c in new {
            let dup = self.h.iter().any(|o| o.key == c.key && o.date == c.date && o.cur == c.cur && o.src == c.src && (o.price - c.price).abs() < 0.005);
            if !dup {
                self.h.push(c);
            }
        }
        self.h.sort_by(|a, b| b.date.cmp(&a.date).then(a.key.cmp(&b.key)));
        let mut per: HashMap<String, usize> = HashMap::new();
        self.h.retain(|c| {
            let n = per.entry(c.key.clone()).or_default();
            *n += 1;
            *n <= H_PER_KEY
        });
        self.h.truncate(H_TOTAL);
    }

    /// Candidates from the cached charges, marked against the current subscriptions.
    pub fn regroup(&mut self, subs: &[Sub], today: i32) {
        if !self.h.is_empty() {
            self.c = detect::group(&self.h, today);
        }
        for c in &mut self.c {
            if let Some((icon, site)) = self.apps.get(&c.key) {
                c.icon.clone_from(icon);
                if c.site.is_empty() {
                    c.site.clone_from(site);
                }
            }
        }
        self.mark(subs);
    }

    pub fn mark(&mut self, subs: &[Sub]) {
        detect::mark(&mut self.c, subs, &self.skipped);
    }

    /// Names of App Store items without a site that were never looked up (at most `max`).
    pub fn to_look_up(&self, max: usize) -> Vec<(String, String)> {
        let need = self.c.iter().filter(|c| c.apple && c.site.is_empty() && !self.apps.contains_key(&c.key));
        need.take(max).map(|c| (c.key.clone(), c.name.clone())).collect()
    }

    /// Candidates neither tracked nor skipped.
    pub fn pending(&self) -> impl Iterator<Item = &Candidate> {
        self.c.iter().filter(|c| !c.tracked && !c.skipped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::days_from_civil;

    fn charge(key: &str, date: i32, price: f64, src: u8) -> Charge {
        Charge {
            key: key.into(), name: format!("{}\tX", key), date, price, cur: "USD".into(), period: Some((1, 2)), trial: false,
            renews: None, site: format!("{}.com", key), category: "Video".into(), score_parts: detect::KNOWN | detect::AMOUNT, src,
            method: "Visa 0000".into(),
        }
    }

    #[test]
    fn scan_file_roundtrip_caps_and_replace() {
        let dir = std::env::temp_dir().join(format!("subscribity-test-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let d0 = days_from_civil(2026, 1, 1);
        let mut f = ScanFile::load(&dir);
        f.u = Some(Uid { mailbox: "[Gmail]/All\tMail".into(), uidvalidity: 7, last_uid: 42 });
        f.skipped.insert("acme".into());
        f.merge((0..30).map(|i| charge("netflix", d0 + i * 30, 15.49, 0)).collect(), false);
        f.merge(vec![charge("netflix", d0 + 29 * 30, 15.49, 0), charge("spotify", d0, 9.99, 1)], false);
        assert_eq!(f.h.iter().filter(|c| c.key == "netflix").count(), H_PER_KEY);
        assert_eq!(f.h.len(), H_PER_KEY + 1);
        f.regroup(&[], d0 + 900);
        assert!(f.c.iter().any(|c| c.key == "netflix"));
        assert!(f.save(&dir) && !dir.join("scan.tsv.tmp").exists());
        let back = ScanFile::load(&dir);
        assert_eq!(back.u, f.u);
        assert_eq!(back.skipped, f.skipped);
        assert_eq!(back.h, f.h);
        assert_eq!(back.c.len(), f.c.len());
        // A complete mail scan replaces mail charges but keeps PayPal ones.
        let mut b = back;
        b.merge(vec![charge("hulu", d0, 7.99, 0)], true);
        let keys: HashSet<&str> = b.h.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, HashSet::from(["hulu", "spotify"]));
        // Garbage and future line types are skipped, not fatal.
        fs::write(dir.join(SCAN_FILE), "z\tnew\nh\tbroken\nu\tINBOX\t1\t5\nh\tk\tK\tnot-a-date\t1\tUSD\t\t0\t\t\t\t0\t0\n").unwrap();
        let g = ScanFile::load(&dir);
        assert_eq!((g.h.len(), g.u.map(|u| u.last_uid)), (0, Some(5)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn candidate_line_anchor_roundtrip_and_old_line() {
        let d = days_from_civil(2026, 10, 15);
        let c = Candidate { key: "icloud".into(), name: "iCloud+".into(), cur: "ILS".into(), price: 3.9, every: 1, unit: 2,
            first: d, anchor: d + 1, last: d + 32, next: d + 62, icon: "https://x.mzstatic.com/a.png".into(), ..Default::default() };
        let line = candidate_line(&c);
        let f: Vec<&str> = line.trim_end().split('\t').collect();
        let back = parse_candidate(&f).unwrap();
        assert_eq!((back.first, back.anchor, back.icon.as_str()), (d, d + 1, "https://x.mzstatic.com/a.png"));
        // A v0.3.1 line (18 fields, no anchor): anchor = first.
        let old = f[..18].to_vec();
        let back = parse_candidate(&old).unwrap();
        assert_eq!((back.first, back.anchor), (d, d));
    }

    #[test]
    fn mail_key_roundtrip() {
        let dir = std::env::temp_dir().join(format!("subscribity-test-key-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(save_key(&dir, &Login { host: "imap.example.com".into(), user: "a@example.com".into(), pass: "p\nq".into() }));
        assert!(!fs::read(dir.join(MAIL_KEY_FILE)).unwrap().windows(3).any(|w| w == b"a@e"));
        let l = load_key(&dir).unwrap();
        assert_eq!((l.host.as_str(), l.user.as_str(), l.pass.as_str()), ("imap.example.com", "a@example.com", "p\nq"));
        delete_key(&dir);
        assert!(load_key(&dir).is_none());
        let _ = fs::remove_dir_all(dir);
    }
}
