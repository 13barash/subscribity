// Data model, dates, money formatting, plain-text storage, payment log and backups.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const CURRENCIES: [&str; 30] = [
    "ILS", "USD", "EUR", "GBP", "JPY", "CAD", "AUD", "CHF", "CNY", "INR", "RUB", "UAH", "PLN",
    "SEK", "NOK", "DKK", "CZK", "HUF", "TRY", "BRL", "MXN", "KRW", "SGD", "HKD", "NZD", "ZAR",
    "AED", "THB", "RON", "BGN",
];

const SUBS_FILE: &str = "subscriptions.tsv";
const SETTINGS_FILE: &str = "settings.ini";
const PAYMENTS_FILE: &str = "payments.tsv";
const RATES_FILE: &str = "rates.tsv";
const STATE_FILE: &str = "state.tsv";
pub const EXPORT_FILE: &str = "subscriptions-export.csv";
pub const BACKUP_DIR: &str = "backups";
/// Every file the app writes in its folder, including temp files a crash can leave behind
/// (`<name>.tmp` now, `<stem>.tmp` in v0.1.0). Uninstall deletes exactly these + `backups\`.
pub const OWN_FILES: [&str; 15] = [
    SUBS_FILE, SETTINGS_FILE, PAYMENTS_FILE, RATES_FILE, STATE_FILE, EXPORT_FILE,
    "subscriptions.tsv.tmp", "settings.ini.tmp", "payments.tsv.tmp", "rates.tsv.tmp", "state.tsv.tmp",
    "subscriptions.tmp", "settings.tmp", "rates.tmp", "state.tmp",
];
const SETTINGS_KEYS: [&str; 11] = [
    "main_currency", "default_remind_days", "budget", "budget_alerted", "rates_base", "rates_date",
    "update_check", "update_last", "latest_version", "update_notified", "window",
];
const BACKUP_EVERY_DAYS: i32 = 7;
const BACKUP_KEEP_DAYS: i32 = 30;

#[derive(Clone, Default)]
pub struct Sub {
    pub id: u32,
    pub name: String,
    pub price: f64,
    pub cur: String,
    pub every: u32,
    pub unit: u8,   // 0 day, 1 week, 2 month, 3 year
    pub start: i32, // first payment date (anchor), days since 1970-01-01
    pub category: String,
    pub payment: String,
    pub remind: i32, // days before renewal to notify
    pub active: bool,
    pub notes: String,
    pub url: String,
    pub trial: bool,               // free trial: the first charge is on `start`
    pub history: Vec<(i32, f64)>,  // (day the price changed, price before the change)
}

impl Sub {
    /// In a free trial that hasn't converted yet.
    pub fn in_trial(&self, today: i32) -> bool {
        self.trial && today <= self.start
    }
}

#[derive(Clone)]
pub struct Settings {
    pub main_cur: String,
    pub default_remind: i32,
    pub budget: f64, // monthly budget in the main currency, 0 = off
    pub budget_alerted: bool,
    pub rates_base: String,
    pub rates_date: i32,
    pub update_freq: String, // weekly, monthly, off
    pub update_last: i32,
    pub latest_version: String,
    pub update_notified: String,
    pub window: (i32, i32, i32, i32), // x, y, width, height; width 0 = not saved
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            main_cur: "ILS".into(),
            default_remind: 3,
            budget: 0.0,
            budget_alerted: false,
            rates_base: String::new(),
            rates_date: 0,
            update_freq: "weekly".into(),
            update_last: 0,
            latest_version: String::new(),
            update_notified: String::new(),
            window: (0, 0, 0, 0),
        }
    }
}

#[derive(Clone)]
pub struct Payment {
    pub sub_id: u32,
    pub date: i32,
    pub amount: f64,
    pub cur: String,
    pub name: String,
}

pub struct Store {
    pub dir: PathBuf,
    pub subs: Vec<Sub>,
    pub settings: Settings,
    pub rates: HashMap<String, f64>, // units of currency per 1 main currency
    pub notified: HashSet<String>,
    pub ack: HashSet<String>,
    pub payments: Vec<Payment>,
    pub cursors: HashMap<u32, i32>, // last day checked for payments, per subscription
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
    format!("{} {} {}", d, M[(m - 1) as usize], y)
}

pub fn iso(z: i32) -> String {
    let (y, m, d) = civil_from_days(z);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

pub fn parse_iso(s: &str) -> Option<i32> {
    let s = s.trim();
    if s.len() < 10 {
        return None;
    }
    let y: i32 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| days_from_civil(y, m, d))
}

pub fn due_text(left: i32) -> String {
    match left {
        0 => "today".into(),
        1 => "tomorrow".into(),
        n => format!("in {} days", n),
    }
}

pub fn cycle_text(s: &Sub) -> String {
    let n = s.every.max(1);
    let u = s.unit.min(3) as usize;
    if n == 1 {
        return ["Daily", "Weekly", "Monthly", "Yearly"][u].into();
    }
    format!("Every {} {}", n, ["days", "weeks", "months", "years"][u])
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

pub fn symbol(cur: &str) -> &'static str {
    match cur {
        "ILS" => "₪", "USD" => "$", "EUR" => "€", "GBP" => "£", "JPY" | "CNY" => "¥",
        "INR" => "₹", "RUB" => "₽", "UAH" => "₴", "KRW" => "₩", "TRY" => "₺", "THB" => "฿",
        "PLN" => "zł ", "BRL" => "R$", "CAD" => "C$", "AUD" => "A$", "NZD" => "NZ$",
        "HKD" => "HK$", "SGD" => "S$", "MXN" => "MX$", "CHF" => "CHF ",
        _ => "",
    }
}

pub fn money(v: f64, cur: &str) -> String {
    let cents = (v.abs() * 100.0).round() as u64;
    let neg = v < 0.0 && cents > 0;
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

// ---------- storage ----------

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\\n")
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

/// Temp file + flush to disk + rename, so a crash or power cut leaves the old file or the
/// new one, never a half-written or empty one.
fn write_atomic(path: &Path, text: &str) -> bool {
    use std::io::Write;
    let tmp = tmp_path(path);
    let written = fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(text.as_bytes())?;
        f.sync_all()
    });
    written.is_ok() && fs::rename(&tmp, path).is_ok()
}

/// Copies `from` over `to` the same way (copy to temp, flush, rename).
fn copy_atomic(from: &Path, to: &Path) -> bool {
    let tmp = tmp_path(to);
    let copied = fs::copy(from, &tmp).and_then(|_| fs::OpenOptions::new().write(true).open(&tmp)?.sync_all());
    copied.is_ok() && fs::rename(&tmp, to).is_ok()
}

fn parse_history(s: &str) -> Vec<(i32, f64)> {
    s.split(';')
        .filter_map(|e| {
            let (d, p) = e.split_once(':')?;
            Some((parse_iso(d)?, p.parse().ok()?))
        })
        .collect()
}

fn read_settings(dir: &Path) -> Settings {
    parse_settings(&fs::read_to_string(dir.join(SETTINGS_FILE)).unwrap_or_default())
}

/// Lines with keys this version doesn't know (e.g. written by a newer version), kept as-is.
fn unknown_settings(t: &str) -> String {
    t.lines()
        .filter(|l| l.split_once('=').is_some_and(|(k, _)| !k.trim().is_empty() && !SETTINGS_KEYS.contains(&k.trim())))
        .map(|l| format!("{}\n", l))
        .collect()
}

fn parse_settings(t: &str) -> Settings {
    let mut s = Settings::default();
    for line in t.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "main_currency" => s.main_cur = v.into(),
            "default_remind_days" => s.default_remind = v.parse().unwrap_or(3),
            "budget" => s.budget = v.parse().unwrap_or(0.0),
            "budget_alerted" => s.budget_alerted = v == "1",
            "rates_base" => s.rates_base = v.into(),
            "rates_date" => s.rates_date = parse_iso(v).unwrap_or(0),
            "update_check" => s.update_freq = v.into(),
            "update_last" => s.update_last = parse_iso(v).unwrap_or(0),
            "latest_version" => s.latest_version = v.into(),
            "update_notified" => s.update_notified = v.into(),
            "window" => {
                let n: Vec<i32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                if n.len() == 4 {
                    s.window = (n[0], n[1], n[2], n[3]);
                }
            }
            _ => {}
        }
    }
    s
}

impl Store {
    pub fn load(dir: PathBuf) -> Store {
        let mut st = Store {
            first_run: !dir.join(SETTINGS_FILE).exists(),
            settings: read_settings(&dir),
            dir,
            subs: Vec::new(),
            rates: HashMap::new(),
            notified: HashSet::new(),
            ack: HashSet::new(),
            payments: Vec::new(),
            cursors: HashMap::new(),
        };
        if let Ok(t) = fs::read_to_string(st.dir.join(SUBS_FILE)) {
            for line in t.lines().skip(1) {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() < 12 {
                    continue;
                }
                let Ok(id) = f[0].parse::<u32>() else { continue };
                st.subs.push(Sub {
                    id,
                    name: unesc(f[1]),
                    price: f[2].parse().unwrap_or(0.0),
                    cur: f[3].into(),
                    every: f[4].parse().unwrap_or(1),
                    unit: f[5].parse::<u8>().unwrap_or(2).min(3),
                    start: parse_iso(f[6]).unwrap_or(0),
                    category: unesc(f[7]),
                    payment: unesc(f[8]),
                    remind: f[9].parse().unwrap_or(3),
                    active: f[10] != "0",
                    notes: unesc(f[11]),
                    url: f.get(12).map(|u| unesc(u)).unwrap_or_default(),
                    trial: f.get(13).is_some_and(|t| *t == "1"),
                    history: f.get(14).map(|h| parse_history(h)).unwrap_or_default(),
                });
            }
        }
        if let Ok(t) = fs::read_to_string(st.dir.join(RATES_FILE)) {
            for line in t.lines() {
                if let Some((k, v)) = line.split_once('\t') {
                    if let Ok(r) = v.parse::<f64>() {
                        st.rates.insert(k.into(), r);
                    }
                }
            }
        }
        if let Ok(t) = fs::read_to_string(st.dir.join(STATE_FILE)) {
            for line in t.lines() {
                match line.split_once('\t') {
                    Some(("n", k)) => { st.notified.insert(k.into()); }
                    Some(("a", k)) => { st.ack.insert(k.into()); }
                    _ => {}
                }
            }
        }
        st.reload_payments();
        st
    }

    pub fn save_subs(&self) {
        let mut t = String::from("id\tname\tprice\tcurrency\tevery\tunit\tfirst_payment\tcategory\tpayment_method\tremind_days\tactive\tnotes\turl\ttrial\tprice_history\n");
        for s in &self.subs {
            let hist: Vec<String> = s.history.iter().map(|(d, p)| format!("{}:{}", iso(*d), p)).collect();
            t.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                s.id, esc(&s.name), s.price, s.cur, s.every, s.unit, iso(s.start),
                esc(&s.category), esc(&s.payment), s.remind, s.active as u8, esc(&s.notes),
                esc(&s.url), s.trial as u8, hist.join(";")
            ));
        }
        write_atomic(&self.dir.join(SUBS_FILE), &t);
    }

    /// Two processes (tray and window) write settings, so every change re-reads the file
    /// first and only changes what it means to change.
    pub fn edit_settings(&mut self, f: impl FnOnce(&mut Settings)) {
        let old = fs::read_to_string(self.dir.join(SETTINGS_FILE)).unwrap_or_default();
        let mut s = parse_settings(&old);
        f(&mut s);
        let opt = |d: i32| if d > 0 { iso(d) } else { String::new() };
        let mut t = format!(
            "main_currency={}\ndefault_remind_days={}\nbudget={}\nbudget_alerted={}\nrates_base={}\nrates_date={}\n\
             update_check={}\nupdate_last={}\nlatest_version={}\nupdate_notified={}\nwindow={},{},{},{}\n",
            s.main_cur, s.default_remind, s.budget, s.budget_alerted as u8, s.rates_base, opt(s.rates_date),
            s.update_freq, opt(s.update_last), s.latest_version, s.update_notified,
            s.window.0, s.window.1, s.window.2, s.window.3
        );
        t.push_str(&unknown_settings(&old));
        write_atomic(&self.dir.join(SETTINGS_FILE), &t);
        self.settings = s;
    }

    pub fn reload_settings(&mut self) {
        self.settings = read_settings(&self.dir);
    }

    pub fn save_rates(&self) {
        let mut t = String::new();
        for (k, v) in &self.rates {
            t.push_str(&format!("{}\t{}\n", k, v));
        }
        write_atomic(&self.dir.join(RATES_FILE), &t);
    }

    pub fn save_state(&self) {
        let mut t = String::new();
        for k in &self.notified {
            t.push_str(&format!("n\t{}\n", k));
        }
        for k in &self.ack {
            t.push_str(&format!("a\t{}\n", k));
        }
        write_atomic(&self.dir.join(STATE_FILE), &t);
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

    /// Monthly cost of all active subscriptions in the main currency, and whether any rate was missing.
    pub fn monthly_total(&self) -> (f64, bool) {
        let mut total = 0.0;
        let mut missing = false;
        for s in self.subs.iter().filter(|s| s.active) {
            match self.to_main(monthly(s), &s.cur) {
                Some(v) => total += v,
                None => missing = true,
            }
        }
        (total, missing)
    }

    // ---------- payment log ----------

    pub fn reload_payments(&mut self) {
        self.payments.clear();
        self.cursors.clear();
        let Ok(t) = fs::read_to_string(self.dir.join(PAYMENTS_FILE)) else { return };
        for line in t.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f.first() {
                Some(&"p") if f.len() >= 6 => {
                    if let (Ok(id), Some(date), Ok(amount)) = (f[1].parse(), parse_iso(f[2]), f[3].parse()) {
                        self.payments.push(Payment { sub_id: id, date, amount, cur: f[4].into(), name: unesc(f[5]) });
                    }
                }
                Some(&"c") if f.len() >= 3 => {
                    if let (Ok(id), Some(date)) = (f[1].parse(), parse_iso(f[2])) {
                        self.cursors.insert(id, date);
                    }
                }
                _ => {}
            }
        }
    }

    fn save_payments(&self) {
        let mut t = String::new();
        for p in &self.payments {
            t.push_str(&format!("p\t{}\t{}\t{}\t{}\t{}\n", p.sub_id, iso(p.date), p.amount, p.cur, esc(&p.name)));
        }
        for (id, d) in &self.cursors {
            t.push_str(&format!("c\t{}\t{}\n", id, iso(*d)));
        }
        write_atomic(&self.dir.join(PAYMENTS_FILE), &t);
    }

    /// Logs every renewal that happened since the last check. History starts on the day a
    /// subscription is first seen; paused subscriptions are skipped. Returns true if changed.
    pub fn log_payments(&mut self, today: i32) -> bool {
        self.reload_payments();
        let mut changed = false;
        let known: HashSet<u32> = self.subs.iter().map(|s| s.id).collect();
        let before = self.cursors.len();
        self.cursors.retain(|id, _| known.contains(id));
        changed |= self.cursors.len() != before;
        for s in &self.subs {
            // First sight (new sub, or upgrade from v0.1.0): no backfill of past
            // renewals, but one due today is logged.
            let cursor = *self.cursors.get(&s.id).unwrap_or(&(today - 1));
            if cursor >= today {
                continue;
            }
            if s.active {
                let mut d = next_due(s, cursor + 1);
                let mut guard = 0;
                while d <= today && guard < 400 {
                    self.payments.push(Payment { sub_id: s.id, date: d, amount: s.price, cur: s.cur.clone(), name: s.name.clone() });
                    d = next_due(s, d + 1);
                    guard += 1;
                }
            }
            self.cursors.insert(s.id, today);
            changed = true;
        }
        if changed {
            self.payments.sort_by_key(|p| p.date);
            self.save_payments();
        }
        changed
    }

    pub fn delete_payment(&mut self, sub_id: u32, date: i32) {
        self.reload_payments();
        if let Some(i) = self.payments.iter().position(|p| p.sub_id == sub_id && p.date == date) {
            self.payments.remove(i);
            self.save_payments();
        }
    }

    // ---------- backups ----------

    fn backup_root(&self) -> PathBuf {
        self.dir.join(BACKUP_DIR)
    }

    /// Backup folder names, newest first.
    pub fn backups(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.backup_root())
            .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.retain(|n| parse_iso(n).is_some());
        v.sort();
        v.reverse();
        v
    }

    fn copy_files(from: &Path, to: &Path) -> bool {
        let _ = fs::create_dir_all(to);
        let mut any = false;
        for f in [SUBS_FILE, SETTINGS_FILE, PAYMENTS_FILE] {
            if from.join(f).exists() {
                any |= copy_atomic(&from.join(f), &to.join(f));
            }
        }
        any
    }

    /// Weekly backup, kept for 30 days.
    pub fn backup_if_due(&self, today: i32) {
        if !self.dir.join(SUBS_FILE).exists() {
            return;
        }
        let list = self.backups();
        let due = list.first().and_then(|n| parse_iso(n)).map_or(true, |newest| today - newest >= BACKUP_EVERY_DAYS);
        if due {
            Self::copy_files(&self.dir, &self.backup_root().join(iso(today)));
        }
        for name in self.backups().iter().skip(1) {
            if parse_iso(name).is_some_and(|d| today - d > BACKUP_KEEP_DAYS) {
                let _ = fs::remove_dir_all(self.backup_root().join(name));
            }
        }
    }

    /// Restores a backup. The current files are first saved as a backup named
    /// "<today> before restore", so a restore can be undone.
    pub fn restore(&mut self, name: &str, today: i32) -> bool {
        if parse_iso(name).is_none() || name.contains(['/', '\\']) || name.contains("..") {
            return false;
        }
        let src = self.backup_root().join(name);
        if !src.is_dir() {
            return false;
        }
        // A second restore on the same day must not overwrite the first "before restore".
        let mut keep = format!("{} before restore", iso(today));
        for n in 2.. {
            if !self.backup_root().join(&keep).exists() {
                break;
            }
            keep = format!("{} before restore {}", iso(today), n);
        }
        if !Self::copy_files(&self.dir, &self.backup_root().join(keep)) && self.dir.join(SUBS_FILE).exists() {
            return false; // couldn't save the current data first: don't overwrite it
        }
        let ok = Self::copy_files(&src, &self.dir);
        if ok {
            *self = Store::load(self.dir.clone());
        }
        ok
    }
}

/// Parses {"rates":{"USD":1.0,...}} from open.er-api.com.
pub fn parse_rates(json: &str) -> Option<HashMap<String, f64>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if v["result"] != "success" {
        return None;
    }
    let map: HashMap<String, f64> = v["rates"].as_object()?.iter().filter_map(|(k, r)| Some((k.clone(), r.as_f64()?))).collect();
    (!map.is_empty()).then_some(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_roundtrip() {
        for z in -100_000..100_000 {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(parse_iso("2026-02-30x"), Some(days_from_civil(2026, 2, 30)));
        assert_eq!(parse_iso("bad"), None);
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
        assert_eq!(money(54.9, "ILS"), "₪54.90");
        assert_eq!(money(5.0, "XYZ"), "5.00 XYZ");
        assert_eq!(money(-0.004, "EUR"), "€0.00");
        let r = parse_rates(r#"{"result":"success","base_code":"ILS","rates":{"ILS":1,"USD":0.27}}"#).unwrap();
        assert_eq!(r["USD"], 0.27);
        assert!(parse_rates(r#"{"result":"error"}"#).is_none());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("subscribity-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn subs_roundtrip_with_trial_history_and_notes() {
        let dir = temp_dir("subs");
        let mut st = Store::load(dir.clone());
        st.subs.push(Sub {
            id: 1, name: "A\tB".into(), price: 9.5, cur: "USD".into(), every: 1, unit: 2, start: 20000,
            notes: "line1\r\nline2".into(), trial: true, history: vec![(19990, 7.5)], active: true, ..Default::default()
        });
        st.save_subs();
        let back = Store::load(dir.clone());
        let s = &back.subs[0];
        assert_eq!((s.name.as_str(), s.notes.as_str(), s.trial, s.history.clone()), ("A\tB", "line1\nline2", true, vec![(19990, 7.5)]));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_edits_do_not_clobber_each_other() {
        let dir = temp_dir("settings");
        let mut tray = Store::load(dir.clone());
        let mut window = Store::load(dir.clone());
        tray.edit_settings(|s| s.rates_date = 123);
        window.edit_settings(|s| s.budget = 200.0);
        let s = read_settings(&dir);
        assert_eq!((s.rates_date, s.budget), (123, 200.0));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn payments_are_logged_once_per_renewal() {
        let dir = temp_dir("payments");
        let mut st = Store::load(dir.clone());
        let d0 = days_from_civil(2026, 1, 1);
        st.subs.push(Sub { id: 7, name: "X".into(), price: 10.0, cur: "ILS".into(), every: 1, unit: 1, start: d0, active: true, ..Default::default() });
        st.log_payments(d0); // first sight: the renewal due today, nothing earlier
        assert_eq!(st.payments.iter().map(|p| p.date - d0).collect::<Vec<_>>(), vec![0]);
        st.log_payments(d0 + 15); // two weekly renewals: d0+7, d0+14
        assert_eq!(st.payments.iter().map(|p| p.date - d0).collect::<Vec<_>>(), vec![0, 7, 14]);
        st.log_payments(d0 + 15);
        assert_eq!(st.payments.len(), 3);
        st.delete_payment(7, d0 + 7);
        assert_eq!(Store::load(dir.clone()).payments.len(), 2);
        // A sub first seen long after its start is not backfilled.
        st.subs.push(Sub { id: 8, name: "Y".into(), price: 5.0, cur: "ILS".into(), every: 1, unit: 1, start: d0, active: true, ..Default::default() });
        st.log_payments(d0 + 16);
        assert!(st.payments.iter().all(|p| p.sub_id != 8));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backups_weekly_and_pruned() {
        let dir = temp_dir("backups");
        let st = Store::load(dir.clone());
        fs::write(dir.join(SUBS_FILE), "id\n").unwrap();
        let d = days_from_civil(2026, 1, 1);
        st.backup_if_due(d);
        st.backup_if_due(d + 3); // too soon
        assert_eq!(st.backups().len(), 1);
        st.backup_if_due(d + 7);
        st.backup_if_due(d + 40); // new one, both old ones pruned
        assert_eq!(st.backups(), vec![iso(d + 40)]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn v010_settings_load_and_unknown_keys_survive_edits() {
        let dir = temp_dir("v010");
        // settings.ini exactly as v0.1.0 wrote it (theme line included).
        fs::write(
            dir.join(SETTINGS_FILE),
            "main_currency=EUR\ndefault_remind_days=5\ntheme=system\nrates_base=EUR\nrates_date=2026-03-01\n\
             update_check=monthly\nupdate_last=2026-03-02\nlatest_version=0.1.0\nupdate_notified=\nfuture_key=a=b\n",
        )
        .unwrap();
        let mut st = Store::load(dir.clone());
        assert!(!st.first_run);
        let s = &st.settings;
        assert_eq!((s.main_cur.as_str(), s.default_remind, s.update_freq.as_str()), ("EUR", 5, "monthly"));
        assert_eq!((s.rates_date, s.update_last), (days_from_civil(2026, 3, 1), days_from_civil(2026, 3, 2)));
        st.edit_settings(|s| s.budget = 50.0);
        st.edit_settings(|s| s.window = (1, 2, 800, 600));
        let t = fs::read_to_string(dir.join(SETTINGS_FILE)).unwrap();
        assert!(t.contains("theme=system\n") && t.contains("future_key=a=b\n"));
        assert_eq!(t.matches("future_key").count(), 1);
        let s = read_settings(&dir);
        assert_eq!((s.main_cur.as_str(), s.budget, s.window), ("EUR", 50.0, (1, 2, 800, 600)));
        assert!(!dir.join("settings.ini.tmp").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn restore_saves_current_data_first_every_time() {
        let dir = temp_dir("restore");
        let d = days_from_civil(2026, 1, 1);
        fs::write(dir.join(SUBS_FILE), "v1").unwrap();
        let mut st = Store::load(dir.clone());
        st.backup_if_due(d);
        fs::write(dir.join(SUBS_FILE), "v2").unwrap();
        assert!(st.restore(&iso(d), d + 1));
        assert_eq!(fs::read_to_string(dir.join(SUBS_FILE)).unwrap(), "v1");
        assert!(st.restore(&iso(d), d + 1)); // second restore the same day
        let root = dir.join(BACKUP_DIR);
        assert_eq!(fs::read_to_string(root.join(format!("{} before restore", iso(d + 1))).join(SUBS_FILE)).unwrap(), "v2");
        assert_eq!(fs::read_to_string(root.join(format!("{} before restore 2", iso(d + 1))).join(SUBS_FILE)).unwrap(), "v1");
        assert!(!st.restore("../x", d + 1) && !st.restore("2026-05-05", d + 1));
        let _ = fs::remove_dir_all(dir);
    }
}
