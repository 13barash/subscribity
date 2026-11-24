// Data model, dates, money formatting and plain-text storage.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const CURRENCIES: [&str; 30] = [
    "ILS", "USD", "EUR", "GBP", "JPY", "CAD", "AUD", "CHF", "CNY", "INR", "RUB", "UAH", "PLN",
    "SEK", "NOK", "DKK", "CZK", "HUF", "TRY", "BRL", "MXN", "KRW", "SGD", "HKD", "NZD", "ZAR",
    "AED", "THB", "RON", "BGN",
];

#[derive(Clone, Default)]
pub struct Sub {
    pub id: u32,
    pub name: String,
    pub price: f64,
    pub cur: String,
    pub every: u32,
    pub unit: u8, // 0 day, 1 week, 2 month, 3 year
    pub start: i32, // first payment date, days since 1970-01-01
    pub category: String,
    pub payment: String,
    pub remind: i32, // days before renewal to notify
    pub active: bool,
    pub notes: String,
    pub url: String,
}

pub struct Settings {
    pub main_cur: String,
    pub default_remind: i32,
    pub rates_base: String,
    pub rates_date: i32,
    pub theme: String,
    pub update_freq: String, // weekly, monthly, off
    pub update_last: i32,    // day of the last update check
    pub latest_version: String,
    pub update_notified: String, // version we already showed a notification for
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            main_cur: "ILS".into(), default_remind: 3, rates_base: String::new(), rates_date: 0, theme: "system".into(),
            update_freq: "weekly".into(), update_last: 0, latest_version: String::new(), update_notified: String::new(),
        }
    }
}

pub struct Store {
    pub dir: PathBuf,
    pub subs: Vec<Sub>,
    pub settings: Settings,
    pub rates: HashMap<String, f64>, // units of currency per 1 main currency
    pub notified: HashSet<String>,
    pub ack: HashSet<String>,
    pub first_run: bool,
}

// ---------- dates (proleptic Gregorian, days since 1970-01-01) ----------

pub fn days_from_civil(y: i32, m: u32, d: u32) -> i32 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i32 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i32) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        2 => if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 29 } else { 28 },
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn add_months(start: i32, k: i32) -> i32 {
    let (y, m, d) = civil_from_days(start);
    let total = y * 12 + m as i32 - 1 + k;
    let ny = total.div_euclid(12);
    let nm = (total.rem_euclid(12) + 1) as u32;
    days_from_civil(ny, nm, d.min(days_in_month(ny, nm)))
}

/// First payment date on or after `today`.
pub fn next_due(s: &Sub, today: i32) -> i32 {
    if s.start >= today {
        return s.start;
    }
    let n = s.every.max(1) as i32;
    match s.unit {
        0 | 1 => {
            let step = if s.unit == 0 { n } else { 7 * n };
            s.start + (today - s.start + step - 1) / step * step
        }
        _ => {
            let months = if s.unit == 2 { n } else { 12 * n };
            let (y0, m0, _) = civil_from_days(s.start);
            let (y1, m1, _) = civil_from_days(today);
            let mut k = (((y1 - y0) * 12 + m1 as i32 - m0 as i32) / months).max(0);
            loop {
                let d = add_months(s.start, k * months);
                if d >= today {
                    return d;
                }
                k += 1;
            }
        }
    }
}

pub fn fmt_date(z: i32) -> String {
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, m, d) = civil_from_days(z);
    format!("{:02} {} {}", d, M[(m - 1) as usize], y)
}

pub fn iso(z: i32) -> String {
    let (y, m, d) = civil_from_days(z);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

pub fn parse_iso(s: &str) -> Option<i32> {
    let mut p = s.trim().split('-');
    let y = p.next()?.parse().ok()?;
    let m = p.next()?.parse().ok()?;
    let d = p.next()?.parse().ok()?;
    Some(days_from_civil(y, m, d))
}

pub fn due_text(left: i32) -> String {
    match left {
        0 => "Today".into(),
        1 => "Tomorrow".into(),
        n => format!("{} days", n),
    }
}

pub fn cycle_text(s: &Sub) -> String {
    let n = s.every.max(1);
    if n == 1 {
        return ["Daily", "Weekly", "Monthly", "Yearly"][s.unit as usize % 4].into();
    }
    let u = ["days", "weeks", "months", "years"][s.unit as usize % 4];
    format!("Every {} {}", n, u)
}

/// Cost per month in the subscription's own currency.
pub fn monthly(s: &Sub) -> f64 {
    let n = s.every.max(1) as f64;
    s.price * match s.unit {
        0 => 30.436875 / n,
        1 => 4.348125 / n,
        2 => 1.0 / n,
        _ => 1.0 / (12.0 * n),
    }
}

// ---------- money ----------

pub fn symbol(cur: &str) -> &str {
    match cur {
        "ILS" => "â‚ª", "USD" => "$", "EUR" => "â‚¬", "GBP" => "Â£", "JPY" | "CNY" => "Â¥",
        "INR" => "â‚¹", "RUB" => "â‚½", "UAH" => "â‚´", "KRW" => "â‚©", "TRY" => "â‚º", "THB" => "à¸¿",
        "PLN" => "zÅ‚ ", "BRL" => "R$", "CAD" => "C$", "AUD" => "A$", "NZD" => "NZ$",
        "HKD" => "HK$", "SGD" => "S$", "MXN" => "MX$", "CHF" => "CHF ",
        _ => "",
    }
}

pub fn money(v: f64, cur: &str) -> String {
    let neg = v < 0.0;
    let cents = (v.abs() * 100.0).round() as u64;
    let int = (cents / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let num = format!("{}{}.{:02}", if neg { "-" } else { "" }, grouped, cents % 100);
    let sym = symbol(cur);
    if sym.is_empty() { format!("{} {}", num, cur) } else { format!("{}{}", sym, num) }
}

#[allow(dead_code)] // used by tests
pub fn parse_num(s: &str) -> Option<f64> {
    let t: String = s.trim().chars().filter(|c| !c.is_whitespace()).collect();
    let t = if t.contains('.') { t.replace(',', "") } else { t.replace(',', ".") };
    t.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)
}

// ---------- storage ----------

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "")
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push_str("\r\n"),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn write_atomic(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

impl Store {
    pub fn load(dir: PathBuf) -> Store {
        let mut st = Store {
            first_run: !dir.join("settings.ini").exists(),
            dir,
            subs: Vec::new(),
            settings: Settings::default(),
            rates: HashMap::new(),
            notified: HashSet::new(),
            ack: HashSet::new(),
        };
        if let Ok(t) = fs::read_to_string(st.dir.join("settings.ini")) {
            for line in t.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    let v = v.trim();
                    match k.trim() {
                        "main_currency" => st.settings.main_cur = v.into(),
                        "default_remind_days" => st.settings.default_remind = v.parse().unwrap_or(3),
                        "rates_base" => st.settings.rates_base = v.into(),
                        "theme" => st.settings.theme = v.into(),
                        "update_check" => st.settings.update_freq = v.into(),
                        "update_last" => st.settings.update_last = parse_iso(v).unwrap_or(0),
                        "latest_version" => st.settings.latest_version = v.into(),
                        "update_notified" => st.settings.update_notified = v.into(),
                        "rates_date" => st.settings.rates_date = parse_iso(v).unwrap_or(0),
                        _ => {}
                    }
                }
            }
        }
        if let Ok(t) = fs::read_to_string(st.dir.join("subscriptions.tsv")) {
            for line in t.lines().skip(1) {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() < 12 {
                    continue;
                }
                st.subs.push(Sub {
                    id: f[0].parse().unwrap_or(0),
                    name: unesc(f[1]),
                    price: f[2].parse().unwrap_or(0.0),
                    cur: f[3].into(),
                    every: f[4].parse().unwrap_or(1),
                    unit: f[5].parse().unwrap_or(2),
                    start: parse_iso(f[6]).unwrap_or(0),
                    category: unesc(f[7]),
                    payment: unesc(f[8]),
                    remind: f[9].parse().unwrap_or(3),
                    active: f[10] != "0",
                    notes: unesc(f[11]),
                    url: f.get(12).map(|u| unesc(u)).unwrap_or_default(),
                });
            }
        }
        if let Ok(t) = fs::read_to_string(st.dir.join("rates.tsv")) {
            for line in t.lines() {
                if let Some((k, v)) = line.split_once('\t') {
                    if let Ok(r) = v.parse::<f64>() {
                        st.rates.insert(k.into(), r);
                    }
                }
            }
        }
        if let Ok(t) = fs::read_to_string(st.dir.join("state.tsv")) {
            for line in t.lines() {
                match line.split_once('\t') {
                    Some(("n", k)) => { st.notified.insert(k.into()); }
                    Some(("a", k)) => { st.ack.insert(k.into()); }
                    _ => {}
                }
            }
        }
        st
    }

    pub fn save_subs(&self) {
        let mut t = String::from("id\tname\tprice\tcurrency\tevery\tunit\tfirst_payment\tcategory\tpayment_method\tremind_days\tactive\tnotes\turl\n");
        for s in &self.subs {
            t.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                s.id, esc(&s.name), s.price, s.cur, s.every, s.unit, iso(s.start),
                esc(&s.category), esc(&s.payment), s.remind, s.active as u8, esc(&s.notes), esc(&s.url)
            ));
        }
        write_atomic(&self.dir.join("subscriptions.tsv"), &t);
    }

    pub fn save_settings(&self) {
        let s = &self.settings;
        let t = format!(
            "main_currency={}\ndefault_remind_days={}\ntheme={}\nrates_base={}\nrates_date={}\nupdate_check={}\nupdate_last={}\nlatest_version={}\nupdate_notified={}\n",
            s.main_cur, s.default_remind, s.theme, s.rates_base,
            if s.rates_date > 0 { iso(s.rates_date) } else { String::new() },
            s.update_freq,
            if s.update_last > 0 { iso(s.update_last) } else { String::new() },
            s.latest_version, s.update_notified
        );
        write_atomic(&self.dir.join("settings.ini"), &t);
    }

    pub fn save_rates(&self) {
        let mut t = String::new();
        for (k, v) in &self.rates {
            t.push_str(&format!("{}\t{}\n", k, v));
        }
        write_atomic(&self.dir.join("rates.tsv"), &t);
    }

    pub fn save_state(&self) {
        let mut t = String::new();
        for k in &self.notified {
            t.push_str(&format!("n\t{}\n", k));
        }
        for k in &self.ack {
            t.push_str(&format!("a\t{}\n", k));
        }
        write_atomic(&self.dir.join("state.tsv"), &t);
    }

    pub fn next_id(&self) -> u32 {
        self.subs.iter().map(|s| s.id).max().unwrap_or(0) + 1
    }

    /// Price converted to the main currency, if a rate is known.
    pub fn to_main(&self, v: f64, cur: &str) -> Option<f64> {
        if cur == self.settings.main_cur {
            return Some(v);
        }
        if self.settings.rates_base != self.settings.main_cur {
            return None;
        }
        self.rates.get(cur).filter(|r| **r > 0.0).map(|r| v / r)
    }
}

/// Parses {"rates":{"USD":1.0,...}} from open.er-api.com without a JSON library.
pub fn parse_rates(json: &str) -> Option<HashMap<String, f64>> {
    if !json.contains("\"success\"") {
        return None;
    }
    let start = json.find("\"rates\"")?;
    let body = &json[start..];
    let open = body.find('{')?;
    let close = body.find('}')?;
    let mut map = HashMap::new();
    for pair in body[open + 1..close].split(',') {
        if let Some((k, v)) = pair.split_once(':') {
            let k = k.trim().trim_matches('"');
            if let Ok(r) = v.trim().parse::<f64>() {
                map.insert(k.to_string(), r);
            }
        }
    }
    if map.is_empty() { None } else { Some(map) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_roundtrip() {
        for z in -1000..30000 {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn monthly_due_clamps_without_drift() {
        let s = Sub { every: 1, unit: 2, start: days_from_civil(2026, 1, 31), ..Default::default() };
        assert_eq!(next_due(&s, days_from_civil(2026, 2, 1)), days_from_civil(2026, 2, 28));
        assert_eq!(next_due(&s, days_from_civil(2026, 3, 1)), days_from_civil(2026, 3, 31));
    }

    #[test]
    fn weekly_and_yearly_due() {
        let w = Sub { every: 2, unit: 1, start: days_from_civil(2026, 11, 1), ..Default::default() };
        assert_eq!(next_due(&w, days_from_civil(2026, 11, 16)), days_from_civil(2026, 11, 29));
        assert_eq!(next_due(&w, days_from_civil(2026, 11, 15)), days_from_civil(2026, 11, 15));
        let y = Sub { every: 1, unit: 3, start: days_from_civil(2024, 2, 29), ..Default::default() };
        assert_eq!(next_due(&y, days_from_civil(2025, 1, 1)), days_from_civil(2025, 2, 28));
    }

    #[test]
    fn money_and_rates() {
        assert_eq!(money(1234.5, "USD"), "$1,234.50");
        assert_eq!(money(5.0, "XYZ"), "5.00 XYZ");
        assert_eq!(parse_num("12,5"), Some(12.5));
        assert_eq!(parse_num("1,234.50"), Some(1234.5));
        let r = parse_rates(r#"{"result":"success","base_code":"ILS","rates":{"ILS":1,"USD":0.27}}"#).unwrap();
        assert_eq!(r["USD"], 0.27);
    }
}
