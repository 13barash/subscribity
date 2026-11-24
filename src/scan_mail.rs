//! Mail side of the scanner: connection (TLS, or plain TCP to the mock server in test copies),
//! the scan itself, the one-scan-at-a-time lock and the headless `--scan` run.
//! Declared from scan.rs (`scan::mail`).
use super::store::{self, Login, ScanFile};
use crate::data::Store;
use crate::detect;
use crate::imap::{ImapError, Mail, Session};
use crate::{data_dir, mime, services, tls, today, w};
use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::ptr::null;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, QueryFullProcessImageNameW, ReleaseMutex, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId, PostMessageW};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// imap::SESSION_CAP (40 MB) holds ~800 messages at the 48 KB fetch size; stay well below it.
const MAX_UIDS: usize = 500;
/// Payment platforms whose receipts name the merchant in the text (see services::aggregator).
const AGGREGATOR_SENDERS: [&str; 9] = [
    "paypal.com", "paypal.co.il", "stripe.com", "paddle.com", "fastspring.com", "googleplay-noreply@google.com",
    "payments-noreply@google.com", "email.apple.com", "itunes.com",
];

// ---------- connection ----------

/// `SUBSCRIBITY_MAIL_TEST=127.0.0.1:<port>` (or localhost) plain TCP to the mock server, honoured
/// only together with `SUBSCRIBITY_NO_INTEGRATION` (test copies). Anything else is ignored.
fn test_addr() -> Option<String> {
    if std::env::var_os("SUBSCRIBITY_NO_INTEGRATION").is_none_or(|v| v.is_empty()) {
        return None;
    }
    let v = std::env::var("SUBSCRIBITY_MAIL_TEST").ok()?;
    let (h, p) = v.rsplit_once(':')?;
    (matches!(h, "127.0.0.1" | "localhost") && p.parse::<u16>().is_ok_and(|p| p > 0)).then_some(v)
}

enum Conn {
    Tls(tls::Tls),
    Plain(TcpStream),
}

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Tls(t) => t.read(b),
            Conn::Plain(s) => s.read(b),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Tls(t) => t.write(b),
            Conn::Plain(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Tls(t) => t.flush(),
            Conn::Plain(s) => s.flush(),
        }
    }
}

/// Connects (10 s), then 30 s read/write timeouts. Also returns a socket handle for cancel.
fn connect(host: &str) -> Result<(Conn, TcpStream), ImapError> {
    let net = |e: &str| ImapError::Net(e.into());
    let conn = if let Some(addr) = test_addr() {
        let a = addr.to_socket_addrs().ok().and_then(|mut a| a.next()).ok_or_else(|| net("server not found"))?;
        Conn::Plain(TcpStream::connect_timeout(&a, CONNECT_TIMEOUT).map_err(|_| net("could not connect"))?)
    } else {
        let (h, port) = match host.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse().unwrap_or(993)),
            _ => (host, 993),
        };
        let t = tls::Tls::connect(h, port, CONNECT_TIMEOUT).map_err(|e| {
            if e.contains("certificate") || e.contains("TLS") || e.starts_with("secure") { ImapError::Tls(e) } else { ImapError::Net(e) }
        })?;
        Conn::Tls(t)
    };
    let sock = match &conn {
        Conn::Tls(t) => t.socket(),
        Conn::Plain(s) => s.try_clone(),
    }
    .map_err(|_| net("connection lost"))?;
    let _ = sock.set_read_timeout(Some(IO_TIMEOUT));
    let _ = sock.set_write_timeout(Some(IO_TIMEOUT));
    Ok((conn, sock))
}

/// IMAP server for an address: presets for Gmail, Yahoo and iCloud, else the custom server field.
pub fn host_for(user: &str, custom: &str) -> Result<String, String> {
    let dom = user.rsplit_once('@').map(|x| x.1.trim().to_ascii_lowercase()).unwrap_or_default();
    let base = dom.split('.').next().unwrap_or("");
    let custom = custom.trim().to_ascii_lowercase();
    if matches!(base, "outlook" | "hotmail" | "live" | "msn") || custom.contains("outlook.") || custom.contains("office365.") {
        return Err("Outlook.com is not supported".into());
    }
    match dom.as_str() {
        "gmail.com" | "googlemail.com" => return Ok("imap.gmail.com".into()),
        "icloud.com" | "me.com" | "mac.com" => return Ok("imap.mail.me.com".into()),
        _ if base == "yahoo" || dom == "ymail.com" => return Ok("imap.mail.yahoo.com".into()),
        _ => {}
    }
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':');
    if custom.is_empty() || custom.len() > 253 || !custom.chars().all(ok) || !custom.contains('.') {
        return Err("Enter your mail server (IMAP) address, for example imap.example.com.".into());
    }
    Ok(custom)
}

pub fn test_login(l: &Login) -> Result<(), ImapError> {
    let (conn, _sock) = connect(&l.host)?;
    Session::login(conn, &l.user, &l.pass)?.logout();
    Ok(())
}

// ---------- scanning (thread or headless process) ----------

pub struct Outcome {
    pub charges: Vec<detect::Charge>,
    pub scanned: usize,
    pub u: Option<store::Uid>, // set only when the scan completed
    pub full: bool,            // searched from the start (not after the saved last UID)
    pub error: Option<ImapError>,
}

fn senders() -> Vec<&'static str> {
    let mut seen = HashSet::new();
    services::SERVICES.iter().flat_map(|s| s.senders.iter().copied()).chain(AGGREGATOR_SENDERS).filter(|s| seen.insert(*s)).collect()
}

fn charge_of(m: &Mail) -> Vec<detect::Charge> {
    if m.date <= 0 {
        return Vec::new();
    }
    let p = mime::parse(&mime::RawMail { header: &m.header, body: &m.body });
    detect::charges_from_mail(&p, m.date)
}

/// Searches the last `months` (after `prev.last_uid` when the mailbox is unchanged), fetches up
/// to 500 newest matches and turns them into charges. Bodies are dropped right after parsing.
pub fn scan_mail(l: &Login, months: u32, prev: Option<&store::Uid>, cancel: &AtomicBool, sock: &Mutex<Option<TcpStream>>, progress: &mut dyn FnMut(&'static str, usize, usize)) -> Outcome {
    let mut out = Outcome { charges: Vec::new(), scanned: 0, u: None, full: true, error: None };
    let mut run = || -> Result<(), ImapError> {
        progress("connecting", 0, 0);
        let (conn, s) = connect(&l.host)?;
        *sock.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
        if cancel.load(Ordering::Relaxed) {
            return Err(ImapError::Cancelled);
        }
        let mut ses = Session::login(conn, &l.user, &l.pass)?;
        progress("searching", 0, 0);
        let (mailbox, uidvalidity) = ses.examine_all()?;
        let after = match prev {
            Some(u) if u.mailbox == mailbox && u.uidvalidity == uidvalidity && uidvalidity != 0 => u.last_uid,
            _ => 0,
        };
        out.full = after == 0;
        let since = today() - (months.clamp(1, 24) * 365).div_ceil(12) as i32;
        let mut uids = ses.search(since as i64, &senders(), &detect::search_keywords(), after)?;
        let last_uid = uids.first().copied().unwrap_or(0).max(after);
        uids.truncate(MAX_UIDS);
        let total = uids.len();
        progress("reading", 0, total);
        let (charges, scanned) = (&mut out.charges, &mut out.scanned);
        ses.fetch(&uids, cancel, &mut |m| {
            *scanned += 1;
            charges.extend(charge_of(&m));
            progress("reading", *scanned, total);
        })?;
        ses.logout();
        out.u = Some(store::Uid { mailbox, uidvalidity, last_uid });
        Ok(())
    };
    if let Err(e) = run() {
        out.error = Some(if cancel.load(Ordering::Relaxed) { ImapError::Cancelled } else { e });
    }
    out
}

/// Charges into the cache; `last_uid` moves only after a complete scan.
pub fn apply(f: &mut ScanFile, o: Outcome) {
    let ok = o.error.is_none();
    f.merge(o.charges, ok && o.full);
    if let Some(u) = o.u {
        f.u = Some(u);
    }
}

/// `Local\SubscribityScan`: one scan at a time (window thread or headless process).
pub struct ScanLock(HANDLE);

impl ScanLock {
    pub fn take() -> Option<ScanLock> {
        unsafe {
            let h = CreateMutexW(null(), 1, w("Local\\SubscribityScan").as_ptr());
            if h.is_null() {
                return None;
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(h);
                return None;
            }
            Some(ScanLock(h))
        }
    }
}

impl Drop for ScanLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

/// `--scan`: the weekly auto-scan, started by the tray. No window; results go to scan.tsv and
/// settings (`scan_last`, `scan_new`, `scan_error=auth`), then the tray is told to reload.
pub fn headless() {
    let Some(_lock) = ScanLock::take() else { return };
    let dir = data_dir();
    let mut st = Store::load(dir.clone());
    if st.first_run {
        return;
    }
    let Some(login) = store::load_key(&dir) else { return };
    let mut f = ScanFile::load(&dir);
    let day = today();
    f.regroup(&st.subs, day);
    let before: HashSet<String> = f.c.iter().map(|c| c.key.clone()).collect();
    let o = scan_mail(&login, st.settings.scan_period, f.u.as_ref(), &AtomicBool::new(false), &Mutex::new(None), &mut |_, _, _| {});
    drop(login);
    let (ok, auth) = (o.error.is_none(), matches!(o.error, Some(ImapError::Auth(_))));
    apply(&mut f, o);
    f.regroup(&st.subs, day);
    f.save(&dir);
    let new = f.pending().filter(|c| !before.contains(&c.key)).count() as u32;
    let ups = f.price_ups(&st.subs, day);
    if ok {
        st.edit_settings(|s| {
            s.scan_last = day;
            s.scan_new += new;
            s.scan_error.clear();
            s.price_up = ups;
        });
    } else if auth {
        st.edit_settings(|s| s.scan_error = "auth".into());
    }
    unsafe {
        let tray = FindWindowW(w("SubscribityHost").as_ptr(), null());
        if !tray.is_null() && same_exe(tray) {
            PostMessageW(tray, crate::WM_RELOAD, 0, 0);
        }
    }
}

/// The window belongs to a process started from this same exe (a test copy never pokes the
/// installed tray).
pub(crate) unsafe fn same_exe(h: HWND) -> bool {
    use std::os::windows::ffi::OsStringExt;
    let mut pid = 0;
    GetWindowThreadProcessId(h, &mut pid);
    let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if p.is_null() {
        return false;
    }
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n) != 0;
    CloseHandle(p);
    let path = std::ffi::OsString::from_wide(&buf[..n as usize]);
    ok && path.to_string_lossy().to_lowercase() == crate::system::exe().to_string_lossy().to_lowercase()
}

/// `--scan-dump <dir>` (developer only, needs SUBSCRIBITY_NO_INTEGRATION): full 24-month scan with
/// mail.key; writes each fetched message as `<uid>.eml` (header + CRLF + body, as fetched) and
/// `index.tsv` (uid, day, detected charge). Touches neither scan.tsv nor settings.ini.
pub fn dump(args: &[String]) {
    if std::env::var_os("SUBSCRIBITY_NO_INTEGRATION").is_none_or(|v| v.is_empty()) {
        return;
    }
    let Some(dir) = args.iter().position(|a| a == "--scan-dump").and_then(|i| args.get(i + 1)) else { return };
    let dir = std::path::PathBuf::from(dir);
    let _ = std::fs::create_dir_all(&dir);
    if let Err(e) = dump_run(&dir) {
        let _ = std::fs::write(dir.join("error.txt"), e);
    }
}

fn dump_run(out: &std::path::Path) -> Result<(), String> {
    let l = store::load_key(&data_dir()).ok_or("no mail.key in the data folder")?;
    let cancel = AtomicBool::new(false);
    let ie = |e: ImapError| match e {
        ImapError::Auth(s) | ImapError::Tls(s) | ImapError::Net(s) | ImapError::Proto(s) => s,
        ImapError::ImapOff => "IMAP is off".to_string(),
        ImapError::Cancelled => "cancelled".to_string(),
    };
    let (conn, _s) = connect(&l.host).map_err(ie)?;
    let mut ses = Session::login(conn, &l.user, &l.pass).map_err(ie)?;
    ses.examine_all().map_err(ie)?;
    let since = today() - (24u32 * 365).div_ceil(12) as i32;
    let mut uids = ses.search(since as i64, &senders(), &detect::search_keywords(), 0).map_err(ie)?;
    uids.truncate(MAX_UIDS);
    let mut index = String::from("uid\tdate\tkey\tname\tprice\tcur\tperiod\n");
    let mut werr = None;
    ses.fetch(&uids, &cancel, &mut |m| {
        let mut raw = m.header.clone();
        raw.extend_from_slice(b"\r\n");
        raw.extend_from_slice(&m.body);
        if let Err(e) = std::fs::write(out.join(format!("{}.eml", m.uid)), raw) {
            werr = Some(e.to_string());
        }
        let det = match charge_of(&m).first() {
            Some(c) => {
                let p = c.period.map_or("-".to_string(), |(n, u)| format!("{n}/{u}"));
                format!("{}\t{}\t{}\t{}\t{}", c.key, c.name, c.price, c.cur, p)
            }
            None => "-\t-\t-\t-\t-".to_string(),
        };
        index.push_str(&format!("{}\t{}\t{}\n", m.uid, crate::data::iso(m.date), det));
    })
    .map_err(ie)?;
    ses.logout();
    std::fs::write(out.join("index.tsv"), index).map_err(|e| e.to_string())?;
    werr.map_or(Ok(()), Err)
}

/// `--mail-test-connect user pass` (test copies only, same gate as the mock server hook): logs
/// in to the mock server and saves mail.key, so `--scan` can be tested without the window.
pub fn test_connect(args: &[String]) {
    let Some(i) = args.iter().position(|a| a == "--mail-test-connect") else { return };
    let (Some(user), Some(pass)) = (args.get(i + 1), args.get(i + 2)) else { return };
    if test_addr().is_none() {
        return;
    }
    let Ok(host) = host_for(user, "imap.example.com") else { return };
    let l = Login { host, user: user.clone(), pass: pass.clone() };
    if test_login(&l).is_ok() {
        store::save_key(&data_dir(), &l);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imap_host_presets_custom_and_outlook() {
        let h = |u: &str, c: &str| host_for(u, c);
        assert_eq!(h("a@gmail.com", "").unwrap(), "imap.gmail.com");
        assert_eq!(h("a@GoogleMail.com", "imap.other.com").unwrap(), "imap.gmail.com");
        assert_eq!(h("a@yahoo.co.uk", "").unwrap(), "imap.mail.yahoo.com");
        assert_eq!(h("a@me.com", "").unwrap(), "imap.mail.me.com");
        assert_eq!(h("a@example.org", " IMAP.Example.org:993 ").unwrap(), "imap.example.org:993");
        assert!(h("a@example.org", "").is_err() && h("a@example.org", "bad host/x").is_err());
        for u in ["a@outlook.com", "a@hotmail.co.uk", "a@live.com", "a@msn.com"] {
            assert_eq!(h(u, "").unwrap_err(), "Outlook.com is not supported");
        }
        assert!(h("a@example.org", "outlook.office365.com").is_err());
    }
}
