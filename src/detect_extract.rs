//! Receipt details beyond who and how much: the payment method, the plan/tier name, the
//! local-currency charge, Apple receipt items (one per purchased subscription), and mails that
//! are about a charge but aren't one (declined, shipped, payouts, cancellations).
use super::{amounts, find_at, first_date, has, period_in, Amt};
use crate::services::{self, Service};

/// Rounds to cents (59.14999999999999 → 59.15).
pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

// ---------- mails that are not charges ----------

/// Subject: the payment did not (yet) happen, or it's a delivery update for an order already seen.
const NOT_CHARGE_SUBJ: [&str; 24] = [
    "payment declined", "payment was declined", "payment failed", "failed payment", "unable to process",
    "couldn't process", "could not process", "update your payment", "complete payment", "complete your payment",
    "awaiting payment", "payment pending", "shipped", "delivered", "on its way", "on the way", "out for delivery",
    "shipment", "התשלום נכשל", "החיוב נכשל", "נשלח", "נמסר", "בדרך אליך", "incomplete order",
];
/// Text: declined payments and money received (payouts, credits) are never subscription charges.
const NOT_CHARGE_TEXT: [&str; 14] = [
    "payment was declined", "failed payment", "payment failed", "payment declined", "credits were added",
    "credits was added", "in api credits", "received credits", "payout", "you've been paid", "you have been paid",
    "you got paid", "eligible for withdrawal", "קיבלת תשלום",
];

pub fn not_a_charge(ls: &str, lt: &str) -> bool {
    has(ls, &NOT_CHARGE_SUBJ, true, false) || has(lt, &NOT_CHARGE_TEXT, true, false)
}

/// Subject-only promo wording (on top of the shared marketing words).
const PROMO_SUBJ: [&str; 14] = [
    "gift", "gifts", "last chance", "hurry", "redeem", "off your", "off a ", "earn ", "free with", "for free",
    "with purchase", "purchase required", "value)", "special offer",
];
/// Subject words that make a mail a receipt even when it also sounds like a promo.
const STRONG_SUBJ: [&str; 10] = ["receipt", "invoice", "charged", "billing", "payment", "renewal", "קבלה", "חשבונית", "חיוב", "תשלום"];

pub fn promo_subject(ls: &str) -> bool {
    has(ls, &PROMO_SUBJ, true, false) || dollar_off(ls)
}

pub fn strong_subject(ls: &str) -> bool {
    has(ls, &STRONG_SUBJ, true, false)
}

/// "$30 off" / "10% off" style subjects.
fn dollar_off(ls: &str) -> bool {
    amounts(ls).iter().any(|a| ls[a.end..].trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ',').starts_with(" off"))
}

/// The subscription ended or is about to (cancelled, expiring, expired).
const ENDED_SUBJ: [&str; 16] = [
    "subscription has ended", "subscription is expiring", "subscription will expire", "has expired", "will expire",
    "has been cancelled", "has been canceled", "subscription cancelled", "subscription canceled", "is about to end",
    "membership has ended", "has been withdrawn", "membership has expired", "המנוי בוטל", "המנוי הסתיים", "ביטול מנוי",
];

pub fn ended(ls: &str) -> bool {
    has(ls, &ENDED_SUBJ, true, false)
}

// ---------- payment method ----------

const BRANDS: [(&str, &str); 17] = [
    ("american express", "Amex"), ("amex", "Amex"), ("mastercard", "Mastercard"), ("master card", "Mastercard"),
    ("visa", "Visa"), ("isracard", "Isracard"), ("diners", "Diners"), ("discover", "Discover"), ("maestro", "Maestro"),
    ("ויזה", "Visa"), ("מסטרקארד", "Mastercard"), ("מאסטרקארד", "Mastercard"), ("ישראכרט", "Isracard"),
    ("דיינרס", "Diners"), ("אמריקן אקספרס", "Amex"), ("jcb", "JCB"), ("unionpay", "UnionPay"),
];
const ENDING: [&str; 8] = ["ending in", "ending with", "ending", "ends in", "המסתיים ב", "שמסתיים ב", "מסתיים ב", "ספרות אחרונות"];
const PAYPAL_PAID: [&str; 5] = ["paid with paypal", "payment method: paypal", "via paypal", "paypal balance", "שולם באמצעות paypal"];

/// Brand at the end of `pre` (after trimming separators): "Mastercard - " → Mastercard.
fn brand_suffix(pre: &str) -> Option<&'static str> {
    let p = pre.trim_end_matches(|c: char| c.is_whitespace() || "-–:(#".contains(c));
    BRANDS.iter().find(|(w, _)| p.ends_with(w) && !p[..p.len() - w.len()].chars().next_back().is_some_and(char::is_alphanumeric)).map(|b| b.1)
}

/// Mask characters (•, *, ., x) right before the digits, at least two.
fn masked(pre: &str) -> bool {
    let p = pre.trim_end_matches(|c: char| c.is_whitespace() || "-–:(".contains(c));
    let n = p.chars().rev().take_while(|c| "•*·.xX●∙ ".contains(*c)).filter(|c| *c != ' ').count();
    n >= 2
}

/// The last four digits of a card, with the brand when the mail names one.
fn card(lt: &str, alts: &str) -> Option<(Option<&'static str>, String)> {
    let b = lt.as_bytes();
    let mut i = 0;
    while i + 4 <= b.len() {
        let four = b[i..i + 4].iter().all(u8::is_ascii_digit) && (i == 0 || !b[i - 1].is_ascii_digit()) && !b.get(i + 4).is_some_and(u8::is_ascii_digit);
        if !four || !lt.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let mut s = i.saturating_sub(40);
        while !lt.is_char_boundary(s) {
            s += 1;
        }
        let pre = &lt[s..i];
        let pre_t = pre.trim_end_matches(|c: char| c.is_whitespace() || "-–:(".contains(c));
        let digits = lt[i..i + 4].to_string();
        let post = &lt[i + 4..];
        let after_brand = |r: &str| {
            let r = r.trim_start_matches(|c: char| ".•* ".contains(c));
            BRANDS.iter().find(|(w, _)| r.starts_with(w)).map(|x| x.1)
        };
        let ending = ENDING.iter().any(|e| pre_t.ends_with(e));
        let stripe = pre_t.ends_with(" with") && pre.trim_end().ends_with('-');
        let dots_after = post.starts_with("...") || post.starts_with("•");
        if ending || masked(pre) || stripe || brand_suffix(pre).is_some() || (dots_after && after_brand(post).is_some()) {
            let near = brand_suffix(pre.trim_end_matches(|c: char| "•*·.xX●∙ -–:(".contains(c)))
                .or_else(|| after_brand(post))
                .or_else(|| BRANDS.iter().filter_map(|(w, n)| pre.rfind(w).map(|p| (p, *n))).max().map(|x| x.1))
                .or_else(|| {
                    let la = alts.to_ascii_lowercase();
                    BRANDS.iter().find(|(w, _)| find_at(&la, w, 0, true, true).is_some()).map(|x| x.1)
                });
            return Some((near, digits));
        }
        i += 4;
    }
    None
}

/// "Mastercard 1234", "Card 1234", "Visa", "Apple Pay", "Google Pay", "PayPal" or "". Never more
/// than the last four digits.
pub fn payment_method(lt: &str, alts: &str, paypal_sender: bool) -> String {
    if let Some((brand, d)) = card(lt, alts) {
        return format!("{} {}", brand.unwrap_or("Card"), d);
    }
    for (w, n) in [("apple pay", "Apple Pay"), ("google pay", "Google Pay")] {
        if find_at(lt, w, 0, true, true).is_some() {
            return n.into();
        }
    }
    if paypal_sender || has(lt, &PAYPAL_PAID, true, false) {
        return "PayPal".into();
    }
    for k in ["paid with", "payment method", "billed to", "charged to", "אמצעי תשלום"] {
        if let Some(p) = find_at(lt, k, 0, true, false) {
            let w: String = lt[p + k.len()..].chars().take(40).collect();
            if let Some(b) = BRANDS.iter().find(|(x, _)| find_at(&w, x, 0, true, true).is_some()) {
                return b.1.into();
            }
        }
    }
    String::new()
}

// ---------- plan name, prorations, local charge ----------

const PLAN_W: [&str; 19] = [
    "pro", "plus", "max", "ultra", "premium", "team", "teams", "business", "enterprise", "basic", "standard",
    "family", "individual", "duo", "student", "starter", "personal", "professional", "unlimited",
];
const PRORATION: [&str; 4] = ["unused time", "remaining time", "prorated", "proration"];

/// "Claude Ultra" from "… Claude Ultra Qty 1 ₪579.00 Unused time on Claude Max …": the service's
/// first word followed by a plan word, skipping proration lines. Returns (name, end byte).
pub fn plan_name(s: &Service, lt: &str) -> Option<(String, usize)> {
    let first = s.name.split(' ').next()?;
    let base = first.trim_end_matches('+').to_ascii_lowercase();
    if base.len() < 3 {
        return None;
    }
    let mut f = 0;
    while let Some(p) = find_at(lt, &base, f, true, true) {
        f = p + base.len();
        let before = &lt[..p];
        if ["time on ", "upgrade to ", "upgraded to ", "switch to ", "try "].iter().any(|w| before.ends_with(w)) {
            continue;
        }
        let rest = &lt[f..];
        let Some(r) = rest.strip_prefix(' ') else { continue };
        if let Some(w) = PLAN_W.iter().find(|w| r.starts_with(*w) && !r[w.len()..].starts_with(char::is_alphanumeric)) {
            let mut c = w.chars();
            let cap: String = c.next().map(|x| x.to_uppercase().chain(c).collect()).unwrap_or_default();
            return Some((format!("{} {}", first, cap), f + 1 + w.len()));
        }
    }
    None
}

/// With a proration line ("Unused time on …"), the plan's own price (first amount within 40
/// characters after the plan name) is the recurring price, not the prorated total.
pub fn plan_price<'a>(amts: &'a [Amt], lt: &str, plan_end: usize) -> Option<&'a Amt> {
    if !has(lt, &PRORATION, true, false) {
        return None;
    }
    amts.iter().find(|a| a.pos >= plan_end && lt[plan_end..a.pos].chars().count() <= 40)
}

const CHARGED_KEYS: [&str; 12] = [
    "amount charged", "charged to your", "total charged", "you were charged", "was charged", "charged", "total in ",
    "billed in ", "חויבת", "סכום החיוב", "סך החיוב", "חיוב בסך",
];

/// The card's local-currency charge when the mail shows one next to the list price: an amount in
/// another currency after a "charged" keyword, or in parentheses right after the picked amount.
pub fn local_charge<'a>(amts: &'a [Amt], lt: &str, picked: &Amt) -> Option<&'a Amt> {
    for k in CHARGED_KEYS {
        let mut f = 0;
        while let Some(p) = find_at(lt, k, f, true, false) {
            let e = p + k.len();
            f = e;
            if let Some(a) = amts.iter().find(|a| a.pos >= e && lt[e..a.pos].chars().count() <= 40) {
                if a.cur != picked.cur && a.v > 0.0 {
                    return Some(a);
                }
            }
        }
    }
    amts.iter().find(|a| {
        a.pos > picked.end && a.cur != picked.cur && a.v > 0.0 && {
            let gap = lt[picked.end..a.pos].trim();
            gap.len() <= 6 && (gap.starts_with('(') || gap.starts_with('≈') || gap.starts_with('~') || gap.starts_with('/'))
        }
    })
}

// ---------- Apple ----------

#[derive(Debug, PartialEq)]
pub struct Item {
    pub name: String,
    pub price: f64,
    pub cur: &'static str,
    pub period: Option<(u32, u8)>,
    pub renews: Option<i32>,
    pub trial: bool,
    pub ended: bool,
    pub notice: bool, // confirmation / renewal notice, not a receipt
    pub category: &'static str,
    pub site: &'static str,
}

#[derive(Clone, Copy, PartialEq)]
enum AppleKind {
    Receipt,
    Confirmed,
    Renewal,
    Ended,
}

fn apple_kind(ls: &str) -> Option<AppleKind> {
    if ls.contains("receipt") || ls.contains("קבלה") {
        Some(AppleKind::Receipt)
    } else if ls.contains("subscription is confirmed") || ls.contains("subscription confirmed") {
        Some(AppleKind::Confirmed)
    } else if ls.contains("subscription renewal") {
        Some(AppleKind::Renewal)
    } else if ls.contains("expiring") || ls.contains("will expire") || ls.contains("has expired") {
        Some(AppleKind::Ended)
    } else {
        None
    }
}

const GENERIC_W: [&str; 22] = [
    "subscription", "yearly", "annual", "monthly", "weekly", "membership", "premium", "plan", "pro", "plus",
    "standard", "basic", "year", "month", "week", "the", "access", "full", "auto", "renewable", "1", "12",
];

/// "Gleam: Social Intelligence" → "Gleam"; a cut shorter than 3 characters keeps the full name.
fn short_app(app: &str) -> &str {
    let cut = [":", " - ", " – ", " | "].iter().filter_map(|s| app.find(s)).min().unwrap_or(app.len());
    let s = app[..cut].trim();
    if s.chars().count() >= 3 { s } else { app.trim() }
}

/// Subscription name from the app and the subscription line: a generic subscription ("Yearly
/// Subscription") gives the app's name; one that doesn't name the app gets the app prefixed.
fn combine(app: &str, sub: &str) -> String {
    let sub = sub.trim();
    let app = if app.eq_ignore_ascii_case("app store") || app.contains('@') { "" } else { short_app(app) };
    let lsub = sub.to_lowercase();
    let words: Vec<&str> = lsub.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let specific = words.iter().filter(|w| !GENERIC_W.contains(w)).count();
    let app_word = app.split(|c: char| !c.is_alphanumeric()).find(|w| !w.is_empty()).unwrap_or("").to_lowercase();
    if app.is_empty() {
        return sub.to_string();
    }
    if specific == 0 {
        return app.to_string();
    }
    if (!app_word.is_empty() && words.contains(&app_word.as_str())) || specific >= 2 {
        return sub.to_string();
    }
    format!("{} {}", app, sub)
}

/// "iCloud+ with 200 GB of storage" → "iCloud+ 200 GB"; a parenthesised period is dropped.
fn apple_name(s: &str) -> String {
    let base = s.split('(').next().unwrap_or(s).trim();
    let l = base.to_ascii_lowercase();
    if let (true, Some(mid)) = (l.starts_with("icloud+ with "), l.find(" of storage")) {
        return format!("iCloud+ {}", base[13..mid].trim());
    }
    base.to_string()
}

/// Category and site for an Apple item, from its name; never a blanket category.
fn apple_kind_of(name: &str) -> (&'static str, &'static str) {
    let l = name.to_ascii_lowercase();
    const T: [(&str, &str, &str); 7] = [
        ("icloud", "Cloud", "icloud.com"), ("apple developer", "Dev", "developer.apple.com"),
        ("apple music", "Music", "music.apple.com"), ("apple tv", "Streaming", "tv.apple.com"),
        ("apple arcade", "Gaming", "apple.com/apple-arcade"), ("apple news", "News", "apple.com/apple-news"),
        ("apple fitness", "Fitness", "apple.com/apple-fitness-plus"),
    ];
    if let Some(t) = T.iter().find(|t| l.contains(t.0)) {
        return (t.1, t.2);
    }
    services::by_merchant(name).map_or(("", ""), |s| (s.category, s.site))
}

/// Period from a receipt date and the renewal date ("Renews 13 Dec 2026").
fn period_from_span(date: i32, renews: i32) -> Option<(u32, u8)> {
    match renews - date {
        6..=8 => Some((1, 1)),
        26..=33 => Some((1, 2)),
        85..=95 => Some((3, 2)),
        170..=190 => Some((6, 2)),
        350..=380 => Some((1, 3)),
        _ => None,
    }
}

fn amount_in(lines: &[&str]) -> Option<(f64, &'static str)> {
    lines.iter().find_map(|l| amounts(l).first().map(|a| (a.v, a.cur)))
}

fn item(name: String, price: (f64, &'static str), period: Option<(u32, u8)>, renews: Option<i32>, trial: bool, ended: bool) -> Item {
    let (category, site) = apple_kind_of(&name);
    Item { name, price: round2(price.0), cur: price.1, period, renews, trial, ended, notice: false, category, site }
}

/// Apple mails → one item per purchased subscription. Receipts list items as "<name>\n<detail
/// (Monthly)>\nRenews <date>\n<price>"; one-off in-app purchases (no "Renews") are skipped.
/// Confirmation / renewal / expiry notices name one subscription each.
pub fn apple_items(subject: &str, text: &str, date: i32) -> Vec<Item> {
    let ls = subject.to_ascii_lowercase();
    // Apple Developer Program: "Your enrollment has been withdrawn" / "membership has expired".
    if (ls.contains("withdrawn") || ls.contains("membership has expired") || ls.contains("membership expired"))
        && text.to_ascii_lowercase().contains("apple developer program")
    {
        return vec![item("Apple Developer Program".into(), (0.0, "XXX"), None, None, false, true)];
    }
    let Some(kind) = apple_kind(&ls) else { return Vec::new() };
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let lower: Vec<String> = lines.iter().map(|l| l.to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    match kind {
        AppleKind::Receipt => {
            for i in 1..lines.len() {
                if !lower[i].starts_with("renews") {
                    continue;
                }
                let renews = first_date(&lower[i], date);
                let Some(price) = amount_in(&lines[i + 1..(i + 3).min(lines.len())]) else { continue };
                let detail = lines[i - 1];
                let (name, mut period) = if detail.contains('(') {
                    let app = if i >= 2 { lines[i - 2] } else { "" };
                    let base = apple_name(detail);
                    let name = if base.starts_with("iCloud+") { base } else { combine(app, &base) };
                    (name, period_in(&lower[i - 1]))
                } else if i >= 2 {
                    (apple_name(lines[i - 2]), period_in(&lower[i - 1]))
                } else {
                    continue;
                };
                if period.is_none() {
                    period = renews.and_then(|r| period_from_span(date, r));
                }
                if !out.iter().any(|x: &Item| x.name == name) {
                    out.push(item(name, price, period, renews, false, false));
                }
            }
        }
        _ => {
            let field = |label: &str| lower.iter().position(|l| l == label).and_then(|k| lines.get(k + 1).copied());
            // The subscription line: "Subscription" field, else the "<name> (1 year)" line.
            let paren = lines.iter().position(|l| {
                let l = l.to_ascii_lowercase();
                l.ends_with(" year)") || l.ends_with(" month)") || l.ends_with(" week)") || l.ends_with(" years)") || l.ends_with(" months)")
            });
            let app = field("app").or_else(|| lines.get(1).copied()).unwrap_or("");
            let sub = match (kind, field("subscription")) {
                (AppleKind::Confirmed, Some(s)) => s.to_string(),
                _ => match paren {
                    Some(k) => lines[k].split('(').next().unwrap_or("").trim().to_string(),
                    None => field("subscription").unwrap_or("").to_string(),
                },
            };
            if sub.is_empty() && app.is_empty() {
                return out;
            }
            let name = apple_name(&combine(app, &sub));
            // Price: the renewal price line, else the line after the subscription line.
            let price_at = lower.iter().position(|l| l == "renewal price").map(|k| k + 1).or(paren.map(|k| k + 1));
            let Some(pa) = price_at.filter(|&k| k < lines.len()) else { return out };
            let Some(price) = amount_in(&lines[pa..(pa + 1).min(lines.len())]) else { return out };
            let lt = text.to_ascii_lowercase();
            let period = period_in(&lower[pa]).or_else(|| paren.and_then(|k| period_in(&lower[k])));
            // "₪249.90/year, starting 07 April 2026" on the price line wins over a trial's start.
            let after = |h: &str, k: &str| find_at(h, k, 0, true, false).and_then(|p| first_date(&h[p + k.len()..].chars().take(40).collect::<String>(), date));
            let starting = after(&lower[pa], "starting ").or_else(|| ["starting from ", "starting "].iter().find_map(|k| after(&lt, k)));
            let trial = kind == AppleKind::Confirmed && (has(&lt, &["free trial", "free for", "trial"], true, false));
            let mut it = item(name, price, period, starting, trial, kind == AppleKind::Ended);
            it.notice = true;
            out.push(it);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data;

    fn day(s: &str) -> i32 {
        data::parse_iso(s).unwrap()
    }

    #[test]
    fn extract_payment_methods() {
        let pm = |t: &str| payment_method(&t.to_ascii_lowercase(), "", false);
        assert_eq!(pm("Billing and Payment\nMastercard •••• 0000\n₪11.90"), "Mastercard 0000");
        assert_eq!(pm("BILLED TO\nMasterCard .... 0000\nJane Doe"), "Mastercard 0000");
        assert_eq!(pm("Paid with\nCard (MasterCard - 0042) [Default]"), "Mastercard 0042");
        assert_eq!(pm("Payment source : Credit Card ***(* * * * 0042)"), "Card 0042");
        assert_eq!(pm("Visa ending in 1234, total $5"), "Visa 1234");
        assert_eq!(pm("Amex **** 0005"), "Amex 0005");
        assert_eq!(pm("אשראי: 0042...Mastercard"), "Mastercard 0042");
        assert_eq!(pm("כרטיס ויזה המסתיים ב-0042"), "Visa 0042");
        assert_eq!(pm("Paid with Apple Pay"), "Apple Pay");
        assert_eq!(pm("Order 2026 total $5 on 12/2026"), "");
        assert_eq!(pm("Payment method: Visa"), "Visa");
        assert_eq!(payment_method("paid on november 8, 2026 with - 0042 (invoice", "logo Mastercard", false), "Mastercard 0042");
        assert_eq!(payment_method("you paid $5.00", "", true), "PayPal");
    }

    #[test]
    fn extract_plan_and_proration() {
        let s = services::by_sender("anthropic.com").unwrap();
        let lt = "receipt nov 8–dec 8, 2026 claude ultra qty 1 ₪579.00 unused time on claude max after 08 nov 2026 qty 1 -₪278.28 total ₪300.72";
        let (name, end) = plan_name(s, lt).unwrap();
        assert_eq!(name, "Claude Ultra");
        let a = amounts(lt);
        assert_eq!(plan_price(&a, lt, end).map(|a| a.v), Some(579.0));
        assert_eq!(plan_name(s, "upgrade to claude max today"), None);
        let lt = "total $200.00 (₪579.00)";
        let a = amounts(lt);
        assert_eq!(local_charge(&a, lt, &a[0]).map(|a| (a.v, a.cur)), Some((579.0, "ILS")));
        let lt = "price $20.00 amount charged to your card: 74.10 ILS";
        let a = amounts(lt);
        assert_eq!(local_charge(&a, lt, &a[0]).map(|a| (a.v, a.cur)), Some((74.1, "ILS")));
        assert_eq!(round2(59.14999999999999), 59.15);
    }

    #[test]
    fn extract_apple_names() {
        assert_eq!(combine("Gleam: Social Intelligence", "Yearly Subscription"), "Gleam");
        assert_eq!(combine("Chess - Play & Learn Online", "Diamond Membership"), "Chess Diamond Membership");
        assert_eq!(combine("Instagram", "Meta Verified Standard"), "Meta Verified Standard");
        assert_eq!(combine("Apple Developer", "Apple Developer Program"), "Apple Developer Program");
        assert_eq!(combine("Me - Caller ID", "Me PRO Monthly"), "Me PRO Monthly");
        assert_eq!(apple_name("iCloud+ with 2 TB of storage (Monthly)"), "iCloud+ 2 TB");
        let it = apple_items("Subscription Renewal", "Subscription renewal\nAcme Tracker - Food Log\nSubscription\nAnnual Subscription (1 year)\n₪99.90/year\nStarting from 09 November 2026, your subscription will continue", day("2026-10-09"));
        assert_eq!((it[0].name.as_str(), it[0].price, it[0].period, it[0].renews), ("Acme Tracker", 99.9, Some((1, 3)), Some(day("2026-11-09"))));
        assert!(it[0].notice && !it[0].trial);
        let it = apple_items("Your Subscription is Expiring", "Subscription Expiring\nAcme Tracker - Food Log\nAnnual Subscription (1 year)\n₪99.90/year", day("2026-10-09"));
        assert!(it[0].ended && it[0].name == "Acme Tracker");
        assert!(apple_items("Your iCloud storage is full", "Upgrade now", 0).is_empty());
        let it = apple_items("Your enrollment has been withdrawn.", "Your membership in the Apple Developer Program has been withdrawn.", 0);
        assert!(it[0].ended && it[0].name == "Apple Developer Program");
    }
}
