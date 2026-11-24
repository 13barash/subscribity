//! Email scanner glue. The window process runs scans on a thread (progress and results come
//! back as WM_SCAN); the tray's weekly auto-scan starts a headless `--scan` process. The tray
//! process itself never connects, parses or groups anything.
//! Storage (mail.key, scan.tsv): `scan_store.rs`. Connection, scan, headless run: `scan_mail.rs`.
use crate::data::iso;
use crate::detect::paypal;
use crate::imap::ImapError;
use crate::{app, data_dir, send_js, state_json, today};
use serde_json::{json, Value};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

#[path = "scan_store.rs"]
pub mod store;
#[path = "scan_mail.rs"]
mod mail;
#[path = "appstore.rs"]
pub(crate) mod appstore;
pub use mail::{dump, headless, test_connect};
pub(crate) use mail::same_exe;
pub(crate) use mail::ScanLock;
use mail::{apply, host_for, scan_mail, test_login, Outcome};
use store::{Login, ScanFile};

const PROGRESS_GAP: Duration = Duration::from_millis(200);
const OPEN_URLS: [(&str, &str); 4] = [
    ("gmail", "https://myaccount.google.com/apppasswords"),
    ("yahoo", "https://login.yahoo.com/account/security/app-passwords"),
    ("icloud", "https://account.apple.com/account/manage/section/security"),
    ("paypal", "https://www.paypal.com/myaccount/activities/"),
];
const BUSY: &str = "A scan is already running. Try again in a minute.";
const NO_LOGIN: &str = "Connect your email first.";

// WM_SCAN wParam
const K_PROGRESS: usize = 1;
const K_DONE: usize = 2;
const K_CONNECTED: usize = 3;
const K_APPS: usize = 4;

// ---------- window process ----------

#[derive(Default)]
pub struct Runtime {
    busy: bool,
    connecting: bool,
    looking_up: bool, // App Store lookups running
    cancel: Arc<AtomicBool>,
    sock: Arc<Mutex<Option<TcpStream>>>,
    file: Option<ScanFile>,
    account: Option<Option<(String, String)>>, // (user, host) of mail.key, read once
}

fn rt() -> &'static mut Runtime {
    &mut app().scan
}

fn post_to<T>(host: isize, kind: usize, v: T) {
    unsafe { PostMessageW(host as HWND, crate::WM_SCAN, kind, Box::into_raw(Box::new(v)) as isize) };
}

fn account() -> Option<(String, String)> {
    let r = rt();
    r.account.get_or_insert_with(|| store::load_key(&data_dir()).map(|l| (l.user.clone(), l.host.clone()))).clone()
}

/// The cached scan.tsv, regrouped for today and marked against the current subscriptions.
fn file() -> &'static mut ScanFile {
    let a = app();
    let f = a.scan.file.get_or_insert_with(|| {
        let mut f = ScanFile::load(&data_dir());
        f.regroup(&a.store.subs, today());
        f
    });
    f.mark(&a.store.subs);
    f
}

/// Read-modify-write of scan.tsv (a headless scan may have written it meanwhile).
fn edit(change: impl FnOnce(&mut ScanFile)) {
    let a = app();
    let mut f = ScanFile::load(&data_dir());
    change(&mut f);
    f.regroup(&a.store.subs, today());
    f.save(&data_dir());
    a.scan.file = Some(f);
}

fn candidates() -> Vec<Value> {
    file().c.iter().map(|c| c.to_json()).collect()
}

fn send_list() {
    send_js(&json!({"type": "scan_list", "candidates": candidates()}));
}

/// Window process: a scan, login check or App Store lookup is running.
pub fn busy() -> bool {
    let r = rt();
    r.busy || r.connecting || r.looking_up
}

/// `state.scan` (window process only).
pub fn state() -> Value {
    let a = app();
    if !a.window_mode {
        return Value::Null;
    }
    let acc = account();
    let f = file();
    let s = &a.store.settings;
    json!({
        "connected": acc.is_some(),
        "user": acc.as_ref().map_or("", |x| x.0.as_str()),
        "host": acc.as_ref().map_or("", |x| x.1.as_str()),
        "auto": s.scan_auto,
        "period": s.scan_period,
        "last": if s.scan_last > 0 { iso(s.scan_last) } else { String::new() },
        "found": f.c.len(),
        "pending": f.pending().count(),
        "skipped": f.c.iter().filter(|c| c.skipped).count(),
        "newFound": s.scan_new,
        "error": s.scan_error,
        "busy": a.scan.busy,
    })
}

/// Subscriptions were added, edited or deleted: candidates re-mark as tracked or not, and the
/// scanner's price-increase list (`scan_price_up`) follows (an updated price drops its entry).
pub fn subs_changed() {
    let a = app();
    if a.scan.file.is_some() || !a.store.settings.price_up.is_empty() {
        let _ = sync_price_up(); // callers post WM_RELOAD right after
    }
    if a.scan.file.is_some() {
        send_list();
    }
}

/// Writes `scan_price_up` when the cached scan results say something else than settings.
/// True when it changed (the tray should reload to announce new increases).
fn sync_price_up() -> bool {
    let a = app();
    let ups = file().price_ups(&a.store.subs, today());
    let changed = ups != a.store.settings.price_up;
    if changed {
        a.store.edit_settings(|s| s.price_up = ups);
    }
    changed
}

fn s_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// Scanner IPC commands; false if `cmd` isn't one of them.
pub fn handle_ipc(cmd: &str, msg: &Value) -> bool {
    let a = app();
    match cmd {
        "mail_connect" => {
            if a.scan.connecting || a.scan.busy {
                send_js(&json!({"type": "mail", "connected": false, "error": BUSY}));
                return true;
            }
            let user = s_of(msg, "user").trim().to_string();
            let pass = s_of(msg, "pass");
            let host = if !user.contains('@') || pass.trim().is_empty() {
                Err("Enter your email address and app password.".to_string())
            } else {
                host_for(&user, &s_of(msg, "host"))
            };
            let host = match host {
                Ok(h) => h,
                Err(e) => {
                    send_js(&json!({"type": "mail", "connected": false, "error": e}));
                    return true;
                }
            };
            a.scan.connecting = true;
            let host_wnd = a.host as isize;
            std::thread::spawn(move || {
                let l = Login { host, user, pass };
                let r = test_login(&l).map(|_| l).map_err(|e| e.message());
                post_to(host_wnd, K_CONNECTED, r);
            });
        }
        "mail_disconnect" => {
            store::delete_key(&data_dir());
            a.scan.account = Some(None);
            edit(|f| f.u = None);
            a.store.edit_settings(|s| s.scan_error.clear());
            send_js(&json!({"type": "mail", "connected": false}));
            send_js(&state_json());
            crate::after_change();
        }
        "scan_start" => start(msg.get("months").and_then(|m| m.as_u64()).unwrap_or(0) as u32),
        "scan_cancel" => {
            a.scan.cancel.store(true, Ordering::Relaxed);
            if let Some(s) = a.scan.sock.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
        "scan_skip" => {
            let key = s_of(msg, "key");
            let on = msg.get("on").and_then(|x| x.as_bool()).unwrap_or(true);
            if !key.is_empty() {
                edit(|f| {
                    if on {
                        f.skipped.insert(key);
                    } else {
                        f.skipped.remove(&key);
                    }
                });
            }
            send_list();
            send_js(&state_json());
        }
        "scan_reset_skipped" => {
            edit(|f| f.skipped.clear());
            send_list();
            send_js(&state_json());
        }
        "scan_auto" => {
            let on = msg.get("on").and_then(|x| x.as_bool()).unwrap_or(false);
            a.store.edit_settings(|s| s.scan_auto = on);
            send_js(&state_json());
        }
        "scan_period" => {
            let m = msg.get("months").and_then(|m| m.as_u64()).unwrap_or(0) as u32;
            if [3, 6, 12, 24].contains(&m) {
                a.store.edit_settings(|s| s.scan_period = m);
            }
            send_js(&state_json());
        }
        "scan_seen" => {
            if a.store.settings.scan_new > 0 {
                a.store.edit_settings(|s| s.scan_new = 0);
                crate::after_change(); // the tray drops its popup row
            }
            send_js(&state_json());
        }
        "scan_list" => {
            backfill_subs();
            send_list()
        }
        "paypal_import" => {
            let rows: Vec<paypal::PaypalRow> = msg
                .get("rows")
                .and_then(|r| r.as_array())
                .map(|v| {
                    v.iter()
                        .map(|r| paypal::PaypalRow {
                            date: s_of(r, "date"), name: s_of(r, "name"), typ: s_of(r, "type"),
                            status: s_of(r, "status"), cur: s_of(r, "currency"), gross: s_of(r, "gross"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            match paypal::charges_from_paypal(&rows) {
                Ok(ch) => {
                    let n = ch.len();
                    edit(|f| f.merge(ch, false));
                    send_js(&json!({"type": "paypal_done", "charges": n, "candidates": candidates()}));
                }
                Err(e) => send_js(&json!({"type": "paypal_done", "charges": 0, "error": e, "candidates": candidates()})),
            }
            send_js(&state_json());
        }
        "open_url" => {
            let which = s_of(msg, "which");
            if let Some((_, url)) = OPEN_URLS.iter().find(|(k, _)| *k == which) {
                crate::shell_open(url, None);
            }
        }
        _ => return false,
    }
    true
}

fn start(months: u32) {
    let a = app();
    let done_err = |e: &str| send_js(&json!({"type": "scan_done", "candidates": candidates(), "scanned": 0, "error": e}));
    if a.scan.busy || a.scan.connecting {
        return done_err(BUSY);
    }
    let Some(login) = store::load_key(&data_dir()) else {
        a.scan.account = Some(None);
        return done_err(NO_LOGIN);
    };
    let months = if [3, 6, 12, 24].contains(&months) { months } else { a.store.settings.scan_period };
    if months != a.store.settings.scan_period {
        a.store.edit_settings(|s| s.scan_period = months);
    }
    a.scan.busy = true;
    a.scan.cancel.store(false, Ordering::Relaxed);
    let (cancel, sock, host) = (a.scan.cancel.clone(), a.scan.sock.clone(), a.host as isize);
    std::thread::spawn(move || {
        let Some(_lock) = ScanLock::take() else {
            let o = Outcome { charges: Vec::new(), scanned: 0, u: None, full: false, error: Some(ImapError::Proto(String::new())) };
            return post_to(host, K_DONE, (o, true));
        };
        let mut last: Option<Instant> = None;
        // A scan started from the window is a full one: it searches the whole period again.
        let o = scan_mail(&login, months, None, &cancel, &sock, &mut |phase, done, total| {
            if last.is_none_or(|t| t.elapsed() >= PROGRESS_GAP) {
                last = Some(Instant::now());
                post_to(host, K_PROGRESS, (phase, done, total));
            }
        });
        drop(login);
        sock.lock().unwrap_or_else(|e| e.into_inner()).take();
        post_to(host, K_DONE, (o, false));
    });
    send_js(&json!({"type": "scan_progress", "phase": "connecting", "done": 0, "total": 0}));
    send_js(&state_json());
}

/// Saved subscriptions that match a candidate (`subId`) take its App Store icon and real first
/// charge when they have none (`first` only if the sub's schedule start is that candidate's
/// anchor, within 3 days). Never overwrites.
fn backfill_subs() {
    let a = app();
    let found: Vec<(u32, String, i32, i32)> = file().c.iter().filter(|c| c.sub_id != 0).map(|c| (c.sub_id, c.icon.clone(), c.first, c.anchor)).collect();
    let mut changed = false;
    for (id, icon, first, anchor) in found {
        let Some(s) = a.store.subs.iter_mut().find(|s| s.id == id) else { continue };
        if s.icon.is_empty() && !icon.is_empty() {
            s.icon = icon;
            changed = true;
        }
        if s.first == 0 && first > 0 && first <= today() && (s.start - anchor).abs() <= 3 {
            s.first = first;
            changed = true;
        }
    }
    if changed {
        a.store.save_subs();
        send_js(&state_json());
    }
}

/// After a window scan: App Store lookups (icon, developer site) for App Store items without a
/// site, on a thread; results come back as K_APPS and are cached in scan.tsv.
fn look_up_apps() {
    let a = app();
    let todo = file().to_look_up(appstore::MAX_PER_SCAN);
    if a.scan.looking_up || todo.is_empty() {
        return;
    }
    a.scan.looking_up = true;
    let (host, country) = (a.host as isize, appstore::country(&a.store.settings.main_cur));
    std::thread::spawn(move || {
        let found: Vec<(String, (String, String))> = todo.into_iter().filter_map(|(key, name)| Some((key, appstore::lookup(&name, country)?))).collect();
        post_to(host, K_APPS, found);
    });
}

/// WM_SCAN in the window process.
pub fn on_message(kind: usize, lp: isize) {
    let a = app();
    match kind {
        K_PROGRESS => {
            let p = unsafe { Box::from_raw(lp as *mut (&'static str, usize, usize)) };
            if a.scan.busy {
                send_js(&json!({"type": "scan_progress", "phase": p.0, "done": p.1, "total": p.2}));
            }
        }
        K_DONE => {
            let (o, locked) = *unsafe { Box::from_raw(lp as *mut (Outcome, bool)) };
            a.scan.busy = false;
            let scanned = o.scanned;
            let err = match &o.error {
                None => None,
                Some(_) if locked => Some(BUSY.to_string()),
                Some(e) => Some(e.message()),
            };
            let cancelled = matches!(o.error, Some(ImapError::Cancelled));
            let ok = o.error.is_none();
            if !locked {
                edit(|f| apply(f, o));
                backfill_subs();
                look_up_apps();
                if sync_price_up() {
                    crate::after_change();
                }
            }
            if ok {
                let day = today();
                a.store.edit_settings(|s| {
                    s.scan_last = day;
                    s.scan_error.clear();
                });
            }
            let mut m = json!({"type": "scan_done", "candidates": candidates(), "scanned": scanned});
            if cancelled {
                m["cancelled"] = json!(true);
            } else if let Some(e) = err {
                m["error"] = json!(e);
            }
            send_js(&m);
            send_js(&state_json());
        }
        K_APPS => {
            a.scan.looking_up = false;
            let found = *unsafe { Box::from_raw(lp as *mut Vec<(String, (String, String))>) };
            if !found.is_empty() {
                edit(|f| f.apps.extend(found));
                backfill_subs();
                send_list();
            }
        }
        K_CONNECTED => {
            a.scan.connecting = false;
            let r = *unsafe { Box::from_raw(lp as *mut Result<Login, String>) };
            match r {
                Ok(l) => {
                    let dir = data_dir();
                    let old = store::load_key(&dir).map(|o| (o.user.clone(), o.host.clone()));
                    if !store::save_key(&dir, &l) {
                        send_js(&json!({"type": "mail", "connected": false, "error": "Could not save the login on this PC."}));
                        return;
                    }
                    if old.as_ref().is_some_and(|o| o.0 != l.user || o.1 != l.host) {
                        edit(|f| {
                            f.u = None;
                            f.h.retain(|c| c.src != 0); // another mailbox: its mail results go
                        });
                    }
                    a.scan.account = Some(Some((l.user.clone(), l.host.clone())));
                    a.store.edit_settings(|s| s.scan_error.clear());
                    send_js(&json!({"type": "mail", "connected": true, "user": l.user, "host": l.host}));
                    send_js(&state_json());
                    crate::after_change();
                }
                Err(e) => send_js(&json!({"type": "mail", "connected": false, "error": e})),
            }
        }
        _ => {}
    }
}
