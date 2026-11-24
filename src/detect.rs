//! Scanner detection: a charge from one parsed mail (who, amount, period, trial, renewal date),
//! period inference from charge dates, grouping into scored candidates, and marking candidates
//! that are already tracked. PayPal activity CSV rows: `paypal.rs`.
use crate::data::{self, Sub, CURRENCIES};
use crate::mime::Parsed;
use crate::services::{self, norm, Aggregator, Service, SERVICES};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[path = "paypal.rs"]
pub mod paypal;
#[path = "detect_extract.rs"]
mod extract;
pub use extract::round2;

// score_parts bits
pub const KNOWN: u16 = 1; // known service sender (or a merchant that maps to one)
pub const RECEIPT: u16 = 2; // receipt word
pub const AMOUNT: u16 = 4; // amount with a currency
pub const PERIOD: u16 = 8; // explicit period keyword
pub const RECUR: u16 = 16; // automatic/recurring payment wording or PayPal subscription type
pub const MARKETING: u16 = 32;
pub const ENDED: u16 = 64; // cancellation / expiry notice: not a charge, ends the subscription
pub const NOTICE: u16 = 128; // confirmation / renewal notice, not a receipt: evidence only for its promised date
pub const APPLE: u16 = 256; // App Store item (its site and icon can come from an App Store lookup)

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Charge {
    pub key: String, // norm(name): grouping key, also the skip key
    pub name: String,
    pub date: i32,
    pub price: f64,
    pub cur: String,
    pub period: Option<(u32, u8)>, // (every, unit) with data::Sub unit codes
    pub trial: bool,
    pub renews: Option<i32>,
    pub site: String,
    pub category: String,
    pub score_parts: u16,
    pub src: u8, // 0 email, 1 PayPal CSV
    pub method: String, // payment method ("Mastercard 1234", "PayPal"), "" if unknown
}

// ---------- text scanning ----------

fn is_w(c: char) -> bool {
    c.is_alphanumeric()
}

/// Next occurrence of `pat` at or after `from`; `sb`/`eb` require a word boundary at the start/end
/// (only checked where the pattern itself starts/ends with a letter or digit).
fn find_at(h: &str, pat: &str, from: usize, sb: bool, eb: bool) -> Option<usize> {
    let mut f = from;
    while let Some(p) = h.get(f..)?.find(pat) {
        let (i, e) = (f + p, f + p + pat.len());
        let ok_s = !sb || !pat.starts_with(is_w) || !h[..i].chars().next_back().is_some_and(is_w);
        let ok_e = !eb || !pat.ends_with(is_w) || !h[e..].chars().next().is_some_and(is_w);
        if ok_s && ok_e {
            return Some(i);
        }
        f = i + pat.chars().next().map_or(1, char::len_utf8);
    }
    None
}

fn has(h: &str, words: &[&str], sb: bool, eb: bool) -> bool {
    words.iter().any(|w| find_at(h, w, 0, sb, eb).is_some())
}

/// A word stated as a fact about this charge, not advice ("we encourage you to select
/// auto-renewal", "if you choose recurring billing").
fn stated(h: &str, words: &[&str]) -> bool {
    words.iter().any(|w| {
        let mut f = 0;
        while let Some(p) = find_at(h, w, f, true, false) {
            f = p + w.len();
            let mut s = p.saturating_sub(60);
            while !h.is_char_boundary(s) {
                s += 1;
            }
            let before = &h[s..p];
            let sentence = before.rsplit(['.', '\n']).next().unwrap_or(before);
            if !["encourage", "we offer", "if you", "you can", "option", "select"].iter().any(|x| sentence.contains(x)) {
                return true;
            }
        }
        false
    })
}

const RECEIPT_EN: [&str; 9] = ["receipt", "invoice", "payment", "subscription", "renew", "charged", "order", "billing", "purchase"];
const RECEIPT_HE: [&str; 4] = ["קבלה", "חשבונית", "חיוב", "תשלום"];
const MARKETING_W: [&str; 19] = [
    "% off", "sale", "deal", "offer", "discount", "promo", "coupon", "limited time", "don't miss",
    "don’t miss", "black friday", "cyber monday", "upgrade now", "try it free", "newsletter", "webinar",
    "מבצע", "הנחה", "הטבה",
];
const REFUND_SUBJ: [&str; 5] = ["refund", "credit note", "reversal", "החזר", "זיכוי"];
const REFUND_TEXT: [&str; 8] = [
    "has been refunded", "refund of", "refund has been", "issued a refund", "you've received a refund",
    "refunded to your", "קיבלת החזר", "זיכוי בסך",
];
const TRIAL_W: [&str; 7] = ["free trial", "trial ends", "trial will end", "trial period", "your trial", "תקופת ניסיון", "ניסיון חינם"];
/// Explicit "this repeats" wording. Bare "renewal"/"מנוי" are too common in shop boilerplate
/// ("if you purchased a renewal SSL…"), so only phrases that state this charge repeats.
const RECUR_W: [&str; 24] = [
    "automatic payment", "recurring", "pre-approved payment", "auto-renew", "auto renew", "automatically renew",
    "will renew", "renews on", "subscription renewal", "membership fee", "subscription fee", "subscription payment",
    "billing period", "next billing", "next payment", "next charge", "your subscription continues", "until cancelled",
    "until canceled", "הוראת קבע", "תשלום אוטומטי", "חידוש מנוי", "חידוש אוטומטי", "דמי מנוי",
];
const PERSONAL: [&str; 10] = [
    "gmail.com", "googlemail.com", "yahoo.com", "outlook.com", "hotmail.com", "live.com", "icloud.com", "me.com",
    "aol.com", "walla.co.il",
];

/// Subject words for the mailbox search (English + Hebrew receipt words).
pub fn search_keywords() -> Vec<&'static str> {
    RECEIPT_EN.iter().chain(RECEIPT_HE.iter()).copied().collect()
}

fn receipt_word(lt: &str) -> bool {
    has(lt, &RECEIPT_EN, true, false) || has(lt, &RECEIPT_HE, false, false)
}

fn marketing_count(lt: &str) -> usize {
    MARKETING_W.iter().filter(|w| find_at(lt, w, 0, true, true).is_some()).count()
}

// ---------- amounts ----------

/// Port of ui.html `parsePrice`: "9,99", "1,234", "1.234,56", "1 234,50", "₪54.90". Negative,
/// empty or garbage gives None.
pub fn parse_price(t: &str) -> Option<f64> {
    let cs: Vec<char> = t.chars().collect();
    for (i, &c) in cs.iter().enumerate() {
        if matches!(c, '-' | '−' | '–') {
            let j = cs[i + 1..].iter().position(|c| !c.is_whitespace()).map(|p| p + i + 1);
            if j.is_some_and(|j| cs[j].is_ascii_digit() || cs[j] == '.' || cs[j] == ',') {
                return None;
            }
        }
    }
    let t: String = cs.into_iter().filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',').collect();
    if !t.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let (dots, commas) = (t.matches('.').count(), t.matches(',').count());
    let mut dec = None;
    if dots > 0 && commas > 0 {
        dec = Some(if t.rfind('.') > t.rfind(',') { '.' } else { ',' });
    } else if dots + commas == 1 {
        let sep = if dots > 0 { '.' } else { ',' };
        let (before, after) = t.split_once(sep).unwrap_or(("", ""));
        let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
        let thousands = after.len() == 3 && digits(after) && (1..=3).contains(&before.len()) && digits(before) && !before.starts_with('0');
        if !thousands {
            dec = Some(sep);
        }
    }
    let strip = |s: &str| s.replace(['.', ','], "");
    let s = match dec {
        Some(d) => {
            let i = t.rfind(d).unwrap_or(0);
            format!("{}.{}", strip(&t[..i]), strip(&t[i + 1..]))
        }
        None => strip(&t),
    };
    s.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Currency marks, longest first. `kr` is deliberately absent (SEK/NOK/DKK are ambiguous).
const SYMS: [(&str, &str); 16] = [
    ("US$", "USD"), ("CA$", "CAD"), ("AU$", "AUD"), ("NZ$", "NZD"), ("HK$", "HKD"), ("A$", "AUD"),
    ("C$", "CAD"), ("R$", "BRL"), ("ש\"ח", "ILS"), ("ש״ח", "ILS"), ("NIS", "ILS"), ("$", "USD"),
    ("€", "EUR"), ("£", "GBP"), ("₪", "ILS"), ("¥", "JPY"),
];

fn iso_code(s: &str) -> Option<&'static str> {
    CURRENCIES.iter().copied().find(|c| *c == s)
}

fn strip_space_end(s: &str) -> &str {
    match s.chars().next_back() {
        Some(c) if c == ' ' || c == '\u{a0}' || c == '\u{202f}' => &s[..s.len() - c.len_utf8()],
        _ => s,
    }
}

fn strip_space_start(s: &str) -> &str {
    match s.chars().next() {
        Some(c) if c == ' ' || c == '\u{a0}' || c == '\u{202f}' => &s[c.len_utf8()..],
        _ => s,
    }
}

/// Currency right before a number: (code, byte index where the mark starts).
fn cur_before(pre: &str) -> Option<(&'static str, usize)> {
    let p = strip_space_end(pre);
    for (s, c) in SYMS {
        if let Some(b) = p.strip_suffix(s) {
            if s.starts_with(|c: char| c.is_ascii_alphabetic()) && b.chars().next_back().is_some_and(is_w) {
                continue;
            }
            return Some((c, b.len()));
        }
    }
    let n = p.len();
    if n >= 3 && p.is_char_boundary(n - 3) && !p[..n - 3].chars().next_back().is_some_and(is_w) {
        return iso_code(&p[n - 3..]).map(|c| (c, n - 3));
    }
    None
}

/// Currency right after a number; a mark followed by another number belongs to that number.
fn cur_after(post: &str) -> Option<(&'static str, bool)> {
    let p = strip_space_start(post);
    let next_is_num = |r: &str| strip_space_start(r).starts_with(|c: char| c.is_ascii_digit());
    if p.len() >= 3 && p.is_char_boundary(3) && !p[3..].starts_with(is_w) && !next_is_num(&p[3..]) {
        if let Some(c) = iso_code(&p[..3]) {
            return Some((c, true));
        }
    }
    for (s, c) in SYMS {
        if let Some(r) = p.strip_prefix(s) {
            if (s.ends_with(|c: char| c.is_ascii_alphabetic()) && r.starts_with(is_w)) || next_is_num(r) {
                return None;
            }
            return Some((c, false));
        }
    }
    None
}

struct Amt {
    pos: usize,
    end: usize,
    v: f64,
    cur: &'static str,
}

fn space_len(b: &[u8]) -> usize {
    match b {
        [b' ', ..] => 1,
        [0xC2, 0xA0, ..] => 2,
        [0xE2, 0x80, 0xAF | 0x89, ..] => 3,
        _ => 0,
    }
}

/// End of the number starting at `i`: digits with `.`/`,` separators and, before any separator,
/// space-grouped thousands ("1 234,50").
fn number_end(b: &[u8], i: usize) -> usize {
    let (mut j, mut sep, mut group) = (i, false, 0);
    loop {
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
            group += 1;
        }
        if j + 1 < b.len() && (b[j] == b'.' || b[j] == b',') && b[j + 1].is_ascii_digit() {
            j += 1;
            sep = true;
            group = 0;
            continue;
        }
        let w = space_len(&b[j..]);
        if !sep && group <= 3 && w > 0 && b.len() >= j + w + 3 && b[j + w..j + w + 3].iter().all(u8::is_ascii_digit)
            && !b.get(j + w + 3).is_some_and(u8::is_ascii_digit)
        {
            j += w;
            group = 0;
            continue;
        }
        return j;
    }
}

/// Every non-negative number with a currency mark next to it.
fn amounts(t: &str) -> Vec<Amt> {
    let b = t.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && (b[i - 1].is_ascii_digit() || b[i - 1] == b'.' || b[i - 1] == b',')) {
            i += 1;
            continue;
        }
        let end = number_end(b, i);
        // "$65k TTM revenue", "$2M": magnitudes, not prices.
        if matches!(b.get(end), Some(b'k' | b'K' | b'm' | b'M' | b'b' | b'B')) && !b.get(end + 1).is_some_and(u8::is_ascii_alphanumeric) {
            i = end + 1;
            continue;
        }
        let before = cur_before(&t[..i]);
        let after = cur_after(&t[end..]);
        let cur = match (before, after) {
            (_, Some((c, true))) => Some((c, before.map_or(i, |b| b.1))),
            (Some((c, s)), _) => Some((c, s)),
            (None, Some((c, false))) => Some((c, i)),
            _ => None,
        };
        if let Some((cur, start)) = cur {
            let neg = t[..start].ends_with(['-', '−']);
            if let (false, Some(v)) = (neg, parse_price(&t[i..end])) {
                out.push(Amt { pos: i, end, v, cur });
            }
        }
        i = end;
    }
    out
}

const AMOUNT_KEYS: [&str; 16] = [
    "you paid", "amount paid", "total charged", "amount charged", "charged", "grand total", "total",
    "amount due", "amount", "payment of", "you sent", "שילמת", "סה\"כ", "סה״כ", "סכום", "לתשלום",
];

/// The amount after the strongest "total/charged/you paid" keyword (within 40 characters), else
/// the first amount.
fn pick<'a>(amts: &'a [Amt], lt: &str) -> Option<&'a Amt> {
    for k in AMOUNT_KEYS {
        let (mut best, mut f) = (None, 0);
        while let Some(p) = find_at(lt, k, f, true, false) {
            let e = p + k.len();
            if let Some(a) = amts.iter().find(|a| a.pos >= e && lt[e..a.pos].chars().count() <= 40) {
                best = Some(a);
            }
            f = e;
        }
        if best.is_some() {
            return best;
        }
    }
    amts.first()
}

// ---------- period and dates ----------

const PERIODS: [(&str, u32, u8); 41] = [
    ("/wk", 1, 1), ("/week", 1, 1), ("per week", 1, 1), ("weekly", 1, 1), ("every week", 1, 1), ("לשבוע", 1, 1), ("שבועי", 1, 1),
    ("/mo", 1, 2), ("/month", 1, 2), ("per month", 1, 2), ("monthly", 1, 2), ("every month", 1, 2), ("each month", 1, 2),
    ("לחודש", 1, 2), ("חודשי", 1, 2), ("חודשית", 1, 2),
    ("quarterly", 3, 2), ("per quarter", 3, 2), ("/quarter", 3, 2), ("every 3 months", 3, 2), ("every three months", 3, 2), ("רבעוני", 3, 2),
    ("every 6 months", 6, 2), ("every six months", 6, 2), ("semi-annual", 6, 2), ("semiannual", 6, 2), ("half-year", 6, 2), ("חצי שנתי", 6, 2),
    ("/yr", 1, 3), ("/year", 1, 3), ("per year", 1, 3), ("yearly", 1, 3), ("annual", 1, 3), ("annually", 1, 3),
    ("every year", 1, 3), ("each year", 1, 3), ("every 12 months", 1, 3), ("לשנה", 1, 3), ("שנתי", 1, 3), ("שנתית", 1, 3),
    ("every 2 weeks", 2, 1),
];

/// Earliest period keyword in the (ASCII-lowercased) text; "every N months/weeks/years" too.
fn period_in(lt: &str) -> Option<(u32, u8)> {
    let mut best: Option<(usize, usize, (u32, u8))> = None;
    let mut consider = |pos: usize, len: usize, p: (u32, u8)| {
        if best.map_or(true, |(bp, bl, _)| pos < bp || (pos == bp && len > bl)) {
            best = Some((pos, len, p));
        }
    };
    for (w, e, u) in PERIODS {
        if let Some(p) = find_at(lt, w, 0, true, true) {
            consider(p, w.len(), (e, u));
        }
    }
    let mut f = 0;
    while let Some(p) = find_at(lt, "every ", f, true, false) {
        let rest = &lt[p + 6..];
        let nd = rest.bytes().take_while(u8::is_ascii_digit).count();
        if let Ok(n @ 1..=36) = rest[..nd].parse::<u32>() {
            let r = rest[nd..].trim_start();
            let unit = [("week", 1u8), ("month", 2), ("year", 3)].into_iter().find(|(w, _)| r.starts_with(w)).map(|x| x.1);
            if let Some(u) = unit {
                consider(p, 6 + nd + 6, (n, u));
            }
        }
        f = p + 6;
    }
    best.map(|b| b.2)
}

const MONTHS: [&str; 12] = [
    "january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december",
];

fn month_of(w: &str) -> Option<u32> {
    let w = w.to_ascii_lowercase();
    if w.len() < 3 {
        return None;
    }
    MONTHS.iter().position(|m| m.starts_with(&w)).map(|i| i as u32 + 1)
}

fn ymd(y: i32, m: u32, d: u32) -> Option<i32> {
    let z = data::days_from_civil(y, m, d);
    ((1..=12).contains(&m) && (1..=31).contains(&d) && data::civil_from_days(z) == (y, m, d)).then_some(z)
}

/// "5th" → 5 (a day number with an optional ordinal suffix).
fn day_num(w: &str) -> Option<u32> {
    let nd = w.bytes().take_while(u8::is_ascii_digit).count();
    let suf = &w[nd..];
    if !(1..=2).contains(&nd) || !matches!(suf, "" | "st" | "nd" | "rd" | "th") {
        return None;
    }
    w[..nd].parse().ok().filter(|d| (1..=31).contains(d))
}

/// First unambiguous date in `s`: ISO, "December 5, 2026", "5 Dec 2026", "Dec 5" (year inferred
/// from `near`), or d/m/y only when a part is over 12. Ambiguous "05/06/2026" is skipped.
pub(crate) fn first_date(s: &str, near: i32) -> Option<i32> {
    let mut toks: Vec<(usize, usize)> = Vec::new();
    let mut st = None;
    for (i, c) in s.char_indices().chain(std::iter::once((s.len(), ' '))) {
        match (c.is_ascii_alphanumeric(), st) {
            (true, None) => st = Some(i),
            (false, Some(a)) => {
                toks.push((a, i));
                st = None;
            }
            _ => {}
        }
    }
    let w = |k: usize| toks.get(k).map(|&(a, b)| &s[a..b]);
    let sep = |k: usize| toks.get(k + 1).map(|&(_, _)| &s[toks[k].1..toks[k + 1].0]);
    let num = |x: &str| x.bytes().all(|b| b.is_ascii_digit()).then(|| x.parse::<i32>().ok()).flatten();
    let year_of = |m: u32, d: u32| {
        let y = data::civil_from_days(near).0;
        ymd(y, m, d).map(|z| if z < near - 7 { ymd(y + 1, m, d) } else { Some(z) }).flatten()
    };
    for k in 0..toks.len() {
        let t0 = w(k)?;
        // numeric: y-m-d or a/b/y with one repeated separator
        if let (Some(a), Some(b), Some(c)) = (num(t0), w(k + 1).and_then(num), w(k + 2).and_then(num)) {
            let (s1, s2) = (sep(k).unwrap_or(""), sep(k + 1).unwrap_or(""));
            if s1 == s2 && matches!(s1, "-" | "/" | ".") {
                let (la, lc) = (t0.len(), w(k + 2).map_or(0, str::len));
                if la == 4 {
                    if let Some(z) = ymd(a, b as u32, c as u32) {
                        return Some(z);
                    }
                } else if la <= 2 && (lc == 4 || lc == 2) {
                    let y = if lc == 2 { 2000 + c } else { c };
                    let dm = if a > 12 && b <= 12 { Some((b, a)) } else if b > 12 && a <= 12 { Some((a, b)) } else if a == b { Some((a, a)) } else { None };
                    if let Some(z) = dm.and_then(|(m, d)| ymd(y, m as u32, d as u32)) {
                        return Some(z);
                    }
                }
                continue;
            }
        }
        let year_at = |k: usize| w(k).and_then(num).filter(|y| (1990..=2100).contains(y));
        if let (Some(m), Some(d)) = (month_of(t0), w(k + 1).and_then(day_num)) {
            if let Some(z) = year_at(k + 2).map_or_else(|| year_of(m, d), |y| ymd(y, m, d)) {
                return Some(z);
            }
        }
        if let (Some(d), Some(m)) = (day_num(t0), w(k + 1).and_then(month_of)) {
            if let Some(z) = year_at(k + 2).map_or_else(|| year_of(m, d), |y| ymd(y, m, d)) {
                return Some(z);
            }
        }
    }
    None
}

const RENEW_KEYS: [&str; 13] = [
    "renews", "renew on", "renewal date", "next billing", "next payment", "next charge", "billing date",
    "trial ends", "will be charged on", "יתחדש", "החיוב הבא", "מועד החיוב", "חידוש",
];

fn renew_date(lt: &str, near: i32) -> Option<i32> {
    for k in RENEW_KEYS {
        let mut f = 0;
        while let Some(p) = find_at(lt, k, f, true, false) {
            let e = p + k.len();
            let w: String = lt[e..].chars().take(60).collect();
            if let Some(z) = first_date(&w, near) {
                return Some(z);
            }
            f = e;
        }
    }
    None
}

// ---------- merchant ----------

const STOPS: [&str; 10] = [" for ", " on ", " via ", " - ", " – ", " | ", " עבור", " בסך", " בתאריך", "  "];

/// Merchant name at the start of `t` (lowercased copy `lt`): up to a delimiter, max 40 chars.
fn clean_merchant(t: &str, lt: &str) -> Option<String> {
    let skip = t.len() - t.trim_start_matches(|c: char| c.is_whitespace() || "-־:\"'“”".contains(c)).len();
    let (t, lt) = (&t[skip..], &lt[skip..]);
    let mut end = t.len();
    for (n, (i, c)) in t.char_indices().enumerate() {
        let dot_end = c == '.' && !t[i + 1..].starts_with(|c: char| c.is_alphanumeric());
        if n >= 40 || dot_end || "\n([|#<,;!?$€£₪".contains(c) || STOPS.iter().any(|s| lt[i..].starts_with(s)) {
            end = i;
            break;
        }
    }
    let m = t[..end].trim().trim_end_matches(|c: char| !c.is_alphanumeric() && c != '+' && c != ')');
    (m.chars().count() >= 2).then(|| m.to_string())
}

/// Merchant after `trig`; with `conn`, after `conn` found within 60 bytes of `trig`.
/// Hebrew " ל" must be followed by a hyphen/maqaf or a non-Hebrew character.
fn merchant_after(t: &str, lt: &str, trig: &str, conn: &str) -> Option<String> {
    let mut f = 0;
    while let Some(p) = find_at(lt, trig, f, true, false) {
        let mut s = p + trig.len();
        f = s;
        if !conn.is_empty() {
            let win = &lt[s..];
            let Some(o) = win.find(conn).filter(|&o| o <= 60) else { continue };
            s += o + conn.len();
            if conn == " ל" && lt[s..].starts_with(|c: char| ('\u{5d0}'..='\u{5ea}').contains(&c)) {
                continue;
            }
        }
        if let Some(m) = clean_merchant(&t[s..], &lt[s..]) {
            return Some(m);
        }
    }
    None
}

/// "Your Acme receipt" / "Your Acme order" in the subject.
fn merchant_between(subj: &str) -> Option<String> {
    let ls = subj.to_ascii_lowercase();
    let p = find_at(&ls, "your ", 0, true, false)? + 5;
    let e = [" receipt", " order", " invoice"].iter().filter_map(|w| ls[p..].find(w)).min()? + p;
    let m = subj[p..e].trim();
    (m.len() >= 2 && !m.contains("google play")).then(|| m.to_string())
}

const PAYPAL_TRIGS: [(&str, &str); 8] = [
    ("payment to ", ""), ("paid to ", ""), ("you paid", " to "), ("you sent", " to "),
    ("שילמת", " ל"), ("התשלום שלך", " ל"), ("שלחת", " ל"), ("תשלום", " ל"),
];
const STORE_TRIGS: [(&str, &str); 6] = [
    ("receipt from ", ""), ("purchase from ", ""), ("order from ", ""), ("order with ", ""), ("invoice from ", ""), ("payment to ", ""),
];

/// Google Play: the item line after the "Item" header; the app name in parentheses wins.
fn play_item(t: &str, lt: &str) -> Option<String> {
    let mut p = find_at(lt, "item", 0, true, true)? + 4;
    loop {
        let rest = lt[p..].trim_start();
        p = lt.len() - rest.len();
        match ["price", "qty", "quantity"].iter().find(|w| rest.starts_with(*w)) {
            Some(w) => p += w.len(),
            None => break,
        }
    }
    let line = &t[p..p + t[p..].find('\n').unwrap_or(t.len() - p)];
    if let (Some(a), Some(b)) = (line.find('('), line.find(')')) {
        if a < b && b - a > 2 {
            return Some(line[a + 1..b].trim().to_string());
        }
    }
    clean_merchant(line, &line.to_ascii_lowercase())
}

/// Earliest known-service name mentioned in the text (Apple/Google receipts).
fn service_in(lt: &str) -> Option<&'static Service> {
    SERVICES
        .iter()
        .filter_map(|s| {
            let n = s.name.trim_end_matches('+').to_ascii_lowercase();
            (n.len() >= 4).then(|| find_at(lt, &n, 0, true, true).map(|p| (p, usize::MAX - n.len(), s))).flatten()
        })
        .min_by_key(|x| (x.0, x.1))
        .map(|x| x.2)
}

fn merchant(a: Aggregator, p: &Parsed, t: &str, lt: &str) -> Option<String> {
    let ls = p.subject.to_ascii_lowercase();
    let trigs: &[(&str, &str)] = match a {
        Aggregator::PayPal => &PAYPAL_TRIGS,
        Aggregator::Stripe | Aggregator::Paddle | Aggregator::FastSpring => &STORE_TRIGS,
        Aggregator::GooglePlay => return play_item(t, lt),
        Aggregator::Apple => return None,
    };
    // Subject first (most reliable), then the body.
    let from = |t: &str, lt: &str| trigs.iter().find_map(|(a, c)| merchant_after(t, lt, a, c));
    from(&p.subject, &ls).or_else(|| (a != Aggregator::PayPal).then(|| merchant_between(&p.subject)).flatten()).or_else(|| from(t, lt))
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

/// One charge from one mail (the first of `charges_from_mail`).
#[cfg(test)]
pub fn charge_from_mail(p: &Parsed, date: i32) -> Option<Charge> {
    charges_from_mail(p, date).into_iter().next()
}

/// Apple mails: one charge per subscription item (one-off in-app purchases are skipped).
fn apple_charges(p: &Parsed, date: i32, method: &str) -> Vec<Charge> {
    extract::apple_items(&p.subject, &p.text, date)
        .into_iter()
        .filter_map(|it| {
            let key = norm(&it.name);
            let mut flags = KNOWN | RECEIPT | AMOUNT | RECUR | APPLE;
            flags |= if it.period.is_some() { PERIOD } else { 0 } | if it.ended { ENDED } else { 0 } | if it.notice { NOTICE } else { 0 };
            (!key.is_empty()).then(|| Charge {
                key, name: it.name, date, price: it.price, cur: it.cur.into(), period: it.period, trial: it.trial,
                renews: it.renews, site: it.site.into(), category: it.category.into(), score_parts: flags, src: 0,
                method: method.into(),
            })
        })
        .collect()
}

/// Charges from one mail: none (not a receipt, marketing, refund, declined payment, delivery
/// update, payout, no amount), one, or one per item (Apple receipts). A cancellation or expiry
/// notice gives an ENDED marker (price 0, currency XXX) instead of a charge.
pub fn charges_from_mail(p: &Parsed, date: i32) -> Vec<Charge> {
    let t = format!("{}\n{}", p.subject, p.text);
    let lt = t.to_ascii_lowercase();
    let ls = p.subject.to_ascii_lowercase();
    let none = Vec::new;
    if has(&ls, &REFUND_SUBJ, true, false) || has(&lt, &REFUND_TEXT, true, false) || extract::not_a_charge(&ls, &lt) {
        return none();
    }
    let subj_receipt = receipt_word(&ls);
    let any_receipt = subj_receipt || receipt_word(&lt);
    let subj_mkt = marketing_count(&ls) > 0 || extract::promo_subject(&ls);
    let mkt = subj_mkt || (!subj_receipt && marketing_count(&lt) >= 2);
    if (subj_mkt && !extract::strong_subject(&ls)) || (mkt && !subj_receipt) {
        return none();
    }
    let Some((_, domain)) = p.from_addr.rsplit_once('@') else { return none() };
    let agg = services::aggregator(domain);
    let method = extract::payment_method(&lt, &p.alts, agg == Some(Aggregator::PayPal));
    if agg == Some(Aggregator::Apple) {
        return apple_charges(p, date, &method);
    }
    let ended = extract::ended(&ls);
    let svc_of = |s: &'static Service| (s.name.to_string(), s.site.to_string(), s.category.to_string());
    let mut svc: Option<&'static Service> = None;
    let ((name, site, category), known) = if let Some(s) = services::by_sender(domain) {
        svc = Some(s);
        (svc_of(s), true)
    } else if let Some(a) = agg {
        match merchant(a, p, &t, &lt) {
            Some(m) => match services::by_merchant(&m) {
                Some(s) => {
                    svc = Some(s);
                    (svc_of(s), true)
                }
                None => ((m, String::new(), String::new()), false),
            },
            None => match (service_in(&lt), a) {
                (Some(s), _) => {
                    svc = Some(s);
                    (svc_of(s), true)
                }
                (None, Aggregator::GooglePlay) => (("Google Play".into(), "play.google.com".into(), String::new()), false),
                _ => return none(),
            },
        }
    } else {
        let r = services::registrable(domain);
        if !(subj_receipt || ended) || PERSONAL.contains(&r.as_str()) {
            return none();
        }
        let label = r.split('.').next().unwrap_or("");
        let name = if !p.from_name.is_empty() && norm(&p.from_name) == norm(label) { p.from_name.clone() } else { capitalize(label) };
        ((name, r.clone(), String::new()), false)
    };
    if !known && !any_receipt && !ended {
        return none();
    }
    // Grouping key: the service (plan changes stay one subscription); name: the receipt's plan.
    let key = norm(&name);
    if key.is_empty() {
        return none();
    }
    let plan = svc.and_then(|s| extract::plan_name(s, &lt));
    let name = plan.as_ref().map_or(name, |p| p.0.clone());
    let renews = renew_date(&lt, date);
    let base = Charge { key, name, date, site, category, src: 0, method, renews, ..Default::default() };
    if ended {
        return vec![Charge { cur: "XXX".into(), score_parts: ENDED | if known { KNOWN } else { 0 }, ..base }];
    }
    let amts = amounts(&t);
    let Some(mut a) = pick(&amts, &lt) else { return none() };
    if let Some(pp) = plan.as_ref().and_then(|(_, e)| extract::plan_price(&amts, &lt, *e)) {
        a = pp;
    } else if let Some(l) = extract::local_charge(&amts, &lt, a) {
        a = l;
    }
    let mut trial = has(&lt, &TRIAL_W, true, false);
    let (mut price, mut cur) = (a.v, a.cur);
    let recur = stated(&lt, &RECUR_W) || renews.is_some();
    if price == 0.0 {
        // A 0.00 order is not a trial; a free start of a subscription is.
        if !(trial || (known && recur)) {
            return none();
        }
        trial = true;
        if let Some(n) = amts.iter().find(|x| x.v > 0.0) {
            (price, cur) = (n.v, n.cur);
        }
    }
    let period = period_in(&lt);
    let mut flags = AMOUNT;
    for (on, bit) in [(known, KNOWN), (any_receipt, RECEIPT), (period.is_some(), PERIOD), (mkt, MARKETING), (recur, RECUR)] {
        if on {
            flags |= bit;
        }
    }
    vec![Charge { price: round2(price), cur: cur.to_string(), period, trial, score_parts: flags, ..base }]
}

// ---------- grouping ----------

/// Period from charge dates (sorted): median gap 6-8 weekly, 26-33 monthly, 85-95 quarterly,
/// 170-190 half-yearly, 350-380 yearly; whole-month gaps with a skipped month stay monthly.
pub fn period_from_gaps(dates: &[i32]) -> Option<(u32, u8)> {
    let mut g: Vec<i32> = dates.windows(2).map(|w| w[1] - w[0]).filter(|&d| d > 0).collect();
    if g.is_empty() {
        return None;
    }
    g.sort_unstable();
    let n = g.len();
    let med = if n % 2 == 1 { g[n / 2] } else { (g[n / 2 - 1] + g[n / 2]) / 2 };
    let r = match med {
        6..=8 => Some((1, 1)),
        26..=33 => Some((1, 2)),
        85..=95 => Some((3, 2)),
        170..=190 => Some((6, 2)),
        350..=380 => Some((1, 3)),
        _ => None,
    };
    if r.is_some() {
        return r;
    }
    let mut ones = 0;
    for &d in &g {
        let k = (d as f64 / 30.44).round();
        if k < 1.0 || (d as f64 - k * 30.44).abs() > 3.0 + k {
            return None;
        }
        ones += (k == 1.0) as usize;
    }
    (ones * 2 >= n).then_some((1, 2))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Candidate {
    pub key: String,
    pub name: String,
    pub price: f64,
    pub cur: String,
    pub every: u32,
    pub unit: u8,
    pub first: i32,  // first real charge of the current schedule (never in the future): Sub.first
    pub anchor: i32, // schedule point `first` belongs to (Apple renew day): Sub.start when added
    pub last: i32,   // last charge
    pub next: i32,   // expected next charge (Sub.start instead of `anchor` while a trial runs)
    pub category: String,
    pub site: String,
    pub score: u8,
    pub count: u32,
    pub src: u8,
    pub trial: bool,
    pub tracked: bool,
    pub price_changed: bool,
    pub skipped: bool,
    pub sub_id: u32,    // the tracked subscription, 0 if none
    pub method: String, // payment method of the latest charge that names one
    pub icon: String,   // App Store artwork URL (https://*.mzstatic.com), "" if none
    pub apple: bool,    // an App Store item: site/icon may come from an App Store lookup
}

impl Candidate {
    /// `start` is what Add should store as the subscription's schedule start: `anchor`, or `next`
    /// during a free trial (the first charge is still ahead).
    pub fn to_json(&self) -> Value {
        let level = if self.score >= 75 { "high" } else if self.score >= 55 { "medium" } else { "low" };
        let start = if self.trial { self.next } else { self.anchor };
        json!({
            "key": self.key, "name": self.name, "price": round2(self.price), "cur": self.cur,
            "every": self.every, "unit": self.unit, "first": data::iso(self.first), "anchor": data::iso(self.anchor), "start": data::iso(start),
            "last": data::iso(self.last), "next": data::iso(self.next), "payment": self.method,
            "category": self.category, "url": self.site, "score": self.score, "level": level,
            "count": self.count, "src": self.src, "trial": self.trial, "tracked": self.tracked,
            "priceChanged": self.price_changed, "skipped": self.skipped, "subId": self.sub_id, "icon": self.icon,
        })
    }
}

fn same_price(a: &Charge, b: &Charge) -> bool {
    a.cur == b.cur && (a.price - b.price).abs() <= a.price.max(b.price) * 0.01
}

/// `d` moved back `k` periods.
fn back(d: i32, k: i32, every: u32, unit: u8) -> i32 {
    let n = every.max(1) as i32 * k;
    match unit {
        0 => d - n,
        1 => d - 7 * n,
        2 => data::add_months(d, -n),
        _ => data::add_months(d, -12 * n),
    }
}

/// `(anchor, first)`. `anchor`: the earliest point of the schedule that ends in `next` with a
/// charge within 3 days of it, so `Sub { start: anchor }` reproduces `next`; a plan change (charge
/// off the schedule) starts anew. `first`: the earliest real charge (no trial, notice or other
/// price) within 3 days of the anchor, e.g. Apple charges the day before its renew day; matched
/// only by a notice's renewal date → the anchor. No fitting point: both the last charge. Never after `today`.
fn first_on_schedule(m: &[Charge], next: i32, every: u32, unit: u8, today: i32) -> (i32, i32) {
    let (earliest, last) = (m[0].date, &m[m.len() - 1]);
    let mut anchor = None;
    for k in 1..=400 {
        let pt = back(next, k, every, unit);
        if pt < earliest - 3 {
            break;
        }
        // A past renewal date from a notice counts as a charge on that day; a trial start does not.
        if m.iter().any(|c| (!c.trial && (c.date - pt).abs() <= 3) || c.renews.is_some_and(|r| r > c.date && r <= today && (r - pt).abs() <= 3)) {
            anchor = Some(pt);
        }
    }
    let Some(a) = anchor.filter(|&a| a <= today) else {
        let d = last.date.min(today);
        return (d, d);
    };
    let first = m
        .iter()
        .filter(|c| !c.trial && c.score_parts & (NOTICE | ENDED) == 0 && c.date <= today && (c.date - a).abs() <= 3 && same_price(c, last))
        .map(|c| c.date)
        .min();
    (a, first.unwrap_or(a))
}

/// No charge came for the expected one (`due`: the promised renewal date, else the last charge
/// plus one period): past `due` by more than a grace (10 days for monthly/weekly, 20 for
/// quarterly/half-yearly, 30 for yearly) the subscription is over. A renewal/confirmation notice
/// or a trial is no charge itself: only its own date counts, with 3 days for the receipt to come.
fn is_stale(last: &Charge, due: i32, every: u32, unit: u8, today: i32) -> bool {
    let days = every.max(1) as i32 * [1, 7, 30, 365][unit.min(3) as usize];
    let grace = if last.score_parts & NOTICE != 0 || last.trial {
        3
    } else if days >= 300 {
        30
    } else if days >= 80 {
        20
    } else {
        10
    };
    due + grace < today
}

/// Charges → candidates (score ≥ 40), best first. The same charge seen twice within 3 days
/// (service receipt + PayPal receipt + CSV row) counts once. A merchant needs recurring evidence:
/// a known service, subscription wording or a period keyword, or ≥ 3 regularly spaced charges of
/// similar amounts (±10%). A cancellation/expiry notice after the last charge drops the group.
pub fn group(charges: &[Charge], today: i32) -> Vec<Candidate> {
    let mut by: HashMap<&str, Vec<&Charge>> = HashMap::new();
    for c in charges {
        by.entry(c.key.as_str()).or_default().push(c);
    }
    let mut out = Vec::new();
    for (key, mut cs) in by {
        cs.sort_by_key(|c| c.date);
        let ended_at = cs.iter().filter(|c| c.score_parts & ENDED != 0).map(|c| c.date).max();
        let mut m: Vec<Charge> = Vec::new();
        for c in cs.into_iter().filter(|c| c.score_parts & ENDED == 0) {
            match m.last_mut() {
                Some(l) if c.date - l.date <= 3 && same_price(l, c) => {
                    if c.score_parts & KNOWN != 0 && l.score_parts & KNOWN == 0 {
                        (l.name, l.site, l.category) = (c.name.clone(), c.site.clone(), c.category.clone());
                    }
                    // Merged with a receipt, a notice is no longer only a notice.
                    let notice = l.score_parts & c.score_parts & NOTICE;
                    l.score_parts = (l.score_parts | c.score_parts) & !NOTICE | notice;
                    l.period = l.period.or(c.period);
                    l.renews = c.renews.or(l.renews);
                    l.trial |= c.trial;
                    if l.method.is_empty() {
                        l.method = c.method.clone();
                    }
                }
                _ => m.push(c.clone()),
            }
        }
        let Some(last) = m.last() else { continue };
        if ended_at.is_some_and(|e| e >= last.date) {
            continue;
        }
        let flags = m.iter().fold(0, |f, c| f | c.score_parts);
        let explicit = m.iter().rev().find_map(|c| c.period);
        let gaps = period_from_gaps(&m.iter().map(|c| c.date).collect::<Vec<_>>());
        let similar = m.iter().all(|c| c.cur == last.cur && (c.price - last.price).abs() <= last.price * 0.10);
        if flags & (KNOWN | RECUR | PERIOD) == 0 && !(m.len() >= 3 && gaps.is_some() && similar) {
            continue;
        }
        let svc = SERVICES.iter().find(|s| norm(s.name) == key);
        let (every, unit) = explicit.or(gaps).or(svc.map(|s| (s.every, s.unit))).unwrap_or((1, 2));
        let has_period = explicit.is_some() || gaps.is_some() || flags & RECUR != 0;
        let score = [(KNOWN, 40), (RECEIPT, 15), (AMOUNT, 20)].iter().filter(|(b, _)| flags & b != 0).map(|x| x.1).sum::<i32>()
            + if has_period { 15 } else { 0 }
            + if m.len() >= 3 { 10 } else { 0 }
            - if flags & MARKETING != 0 { 30 } else { 0 };
        if score < 40 {
            continue;
        }
        // A month-end charge after a longer month (31 Jan → 28 Feb) keeps the 31st as anchor.
        let mut anchor = last.date;
        if unit >= 2 && data::civil_from_days(last.date + 1).2 == 1 {
            let d = data::civil_from_days(last.date).2;
            if let Some(c) = m.iter().rev().find(|c| data::civil_from_days(c.date).2 > d) {
                anchor = c.date;
            }
        }
        let sched = Sub { start: anchor, every, unit, ..Default::default() };
        let promised = last.renews.filter(|&r| r > last.date);
        if is_stale(last, promised.unwrap_or_else(|| data::next_due(&sched, last.date + 1)), every, unit, today) {
            continue;
        }
        let mut next = promised.unwrap_or_else(|| data::next_due(&sched, today.max(last.date + 1)));
        let trial = last.trial && next >= today;
        if next < today {
            // A renewal date from an old notice: roll it forward on the same schedule.
            next = data::next_due(&Sub { start: next, every, unit, ..Default::default() }, today);
        }
        let (point, first) = first_on_schedule(&m, next, every, unit, today);
        let best = m.iter().rev().find(|c| c.score_parts & KNOWN != 0).unwrap_or(last);
        let method = m.iter().rev().find(|c| !c.method.is_empty()).map_or(String::new(), |c| c.method.clone());
        out.push(Candidate {
            key: key.to_string(), name: best.name.clone(), price: round2(last.price), cur: last.cur.clone(), every, unit,
            first, anchor: point, last: last.date, next, category: best.category.clone(), site: best.site.clone(), score: score.min(100) as u8,
            count: m.len() as u32, src: last.src, trial, method, apple: flags & APPLE != 0, ..Default::default()
        });
    }
    out.sort_by(|a, b| b.score.cmp(&a.score).then(a.next.cmp(&b.next)).then(a.key.cmp(&b.key)));
    out
}

/// `https://www.Netflix.com/path` → `netflix.com`.
fn host_of(u: &str) -> String {
    let u = u.trim();
    let u = u.split_once("://").map_or(u, |x| x.1);
    let h = u.split(['/', ':', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    h.strip_prefix("www.").map_or(h.clone(), str::to_string)
}

/// Flags candidates already tracked (name equal or contained, ≥4 chars, or same site), price
/// changes (> 1%, same currency), and skipped keys.
pub fn mark(c: &mut [Candidate], subs: &[Sub], skipped: &HashSet<String>) {
    for x in c.iter_mut() {
        x.skipped = skipped.contains(&x.key);
        (x.tracked, x.price_changed, x.sub_id) = (false, false, 0);
        let n = norm(&x.name);
        let host = host_of(&x.site);
        for s in subs {
            let sn = norm(&s.name);
            let short = n.chars().count().min(sn.chars().count());
            let by_name = !n.is_empty() && (n == sn || (short >= 4 && (n.contains(&sn) || sn.contains(&n))));
            let sh = host_of(&s.url);
            let by_site = !host.is_empty() && !sh.is_empty()
                && (host == sh || host.ends_with(&format!(".{}", sh)) || sh.ends_with(&format!(".{}", host)));
            if by_name || by_site {
                x.tracked = true;
                x.sub_id = s.id;
                x.price_changed = s.cur == x.cur && (s.price - x.price).abs() > s.price.abs() * 0.01;
                break;
            }
        }
    }
}

#[cfg(test)]
#[path = "detect_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "detect_eval.rs"]
mod eval;
