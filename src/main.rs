// Subscribity: a tiny Windows tray app for tracking subscriptions.
//
// Two processes, one exe:
// - The tray process (plain Win32, ~2 MB of RAM) keeps the tray icon with its badge, shows the
//   compact popup, sends renewal and trial reminders, logs payments, refreshes exchange rates,
//   makes weekly backups and checks for updates.
// - `--window` opens the full window, drawn by the WebView2 runtime that ships with Windows.
//   It runs in its own process, which exits when the window closes, so all of its memory is
//   given back.
#![windows_subsystem = "windows"]
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

mod data;
mod icons;
mod detect;
mod imap;
mod mime;
mod popup;
mod scan;
mod services;
mod system;
mod tls;
mod update;

use data::*;
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::num::NonZeroIsize;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Networking::WinHttp::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::SystemInformation::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use wry::WebViewBuilderExtWindows;

pub const APP_NAME: &str = "Subscribity";
pub const SUPPORT_URL: &str = "https://buymeacoffee.com/13barash";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const UI_HTML: &str = include_str!("ui.html");

// Measured: GPU off + a single renderer roughly halves the window's memory (182 -> ~94 MB).
// The msWebOOUI/msPdfOOUI/msSmartScreenProtection part repeats wry's own defaults.
const BROWSER_ARGS: &str = "--disable-gpu --in-process-gpu --renderer-process-limit=1 \
    --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,SpareRendererForSitePerProcess";

const WM_TRAY: u32 = 0x8000 + 1;
const WM_RATES: u32 = 0x8000 + 2;
const WM_OPEN: u32 = 0x8000 + 3;
const WM_IPC: u32 = 0x8000 + 4;
const WM_RELOAD: u32 = 0x8000 + 5; // window process -> tray: data changed on disk
const WM_OPENPAGE: u32 = 0x8000 + 7; // tray -> window: 1 settings, 2 add, 1000+id edit
const WM_UPDATE: u32 = 0x8000 + 8;
const WM_INSTALLED: u32 = 0x8000 + 9;
const WM_SCAN: u32 = 0x8000 + 10; // scan thread -> window: wParam kind, lParam boxed result
const WM_ICONS: u32 = 0x8000 + 11; // --icons helper -> tray: new icons cached
const TIMER_TICK: usize = 1;

const M_OPEN: usize = 1;
const M_ADD: usize = 2;
const M_SETTINGS: usize = 3;
const M_AUTOSTART: usize = 4;
const M_EXIT: usize = 5;

// ---------- app state (single UI thread per process) ----------

struct App {
    hinst: HINSTANCE,
    host: HWND, // receives background results (tray host, or the window itself)
    tray: HWND, // the tray host window (in the window process: the running tray's)
    window_mode: bool,
    win: HWND,
    webview: Option<wry::WebView>,
    pending: Vec<String>,
    page_ready: bool,
    store: Store,
    icon: HICON,
    badge: i32,
    taskbar_msg: u32,
    rates_busy: bool,
    rates_try: (i32, i32),
    update_busy: bool,
    today: i32,
    scale: f32,
    scan: scan::Runtime,  // window process: email scan state
    scan_new_seen: u32,   // tray: scan_new already announced
    scan_err_seen: bool,  // tray: scan_error already announced
    scan_try: i32,        // tray: day the auto-scan was last started
    notice_page: usize,   // tray: open_window code for a click on the last notification
}

static mut APP: *mut App = null_mut();
fn app() -> &'static mut App {
    unsafe { &mut *APP }
}

pub fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn copy_w(dst: &mut [u16], s: &str) {
    let v: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..v.len()].copy_from_slice(&v);
    dst[v.len()] = 0;
}

fn local_time() -> SYSTEMTIME {
    unsafe {
        let mut t: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    }
}

fn today() -> i32 {
    let t = local_time();
    days_from_civil(t.wYear as i32, t.wMonth as u32, t.wDay as u32)
}

pub fn trim_memory() {
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

fn data_dir() -> std::path::PathBuf {
    system::exe().parent().map(|d| d.to_path_buf()).unwrap_or_default()
}

fn shell_open(target: &str, args: Option<&str>) {
    unsafe {
        let a = args.map(w);
        ShellExecuteW(null_mut(), w("open").as_ptr(), w(target).as_ptr(), a.as_ref().map_or(null(), |v| v.as_ptr()), null(), SW_SHOWNORMAL);
    }
}

/// DwmSetWindowAttribute, loaded lazily so dwmapi.dll is not a startup import.
pub fn dwm_set(h: HWND, attr: u32, value: u32) {
    unsafe {
        let lib = LoadLibraryW(w("dwmapi.dll").as_ptr());
        if let Some(f) = GetProcAddress(lib, b"DwmSetWindowAttribute\0".as_ptr()) {
            let set: unsafe extern "system" fn(HWND, u32, *const c_void, u32) -> i32 = std::mem::transmute(f);
            set(h, attr, &value as *const u32 as *const c_void, 4);
        }
    }
}

// ---------- tray icon ----------

fn make_icon(count: i32) -> HICON {
    unsafe {
        let sz = GetSystemMetrics(SM_CXSMICON).max(16);
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = sz;
        bi.bmiHeader.biHeight = -sz;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(null_mut(), screen);
        let mut bits: *mut c_void = null_mut();
        let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let old = SelectObject(dc, bmp);
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (sz * sz) as usize);
        px.fill(0);

        SelectObject(dc, GetStockObject(NULL_PEN));
        SetBkMode(dc, TRANSPARENT as _);
        let base = CreateSolidBrush(0x00F05B5B); // #5B5BF0 (BGR)
        let ob = SelectObject(dc, base);
        RoundRect(dc, 0, 0, sz + 1, sz + 1, sz / 2, sz / 2);
        SelectObject(dc, ob);
        DeleteObject(base);
        draw_text(dc, "S", 0, 0, sz, sz, (sz as f32 * 0.78) as i32, 0x00FFFFFF);

        if count > 0 {
            let text = if count > 9 { "9+".to_string() } else { format!("+{}", count) };
            let bh = (sz as f32 * 0.62).round() as i32;
            let bw = sz.min((bh as f32 * 1.5) as i32);
            let (x0, y0) = (sz - bw, sz - bh);
            let red = CreateSolidBrush(0x004D48E5); // #E5484D
            let ob = SelectObject(dc, red);
            RoundRect(dc, x0, y0, sz + 1, sz + 1, bh, bh);
            SelectObject(dc, ob);
            DeleteObject(red);
            draw_text(dc, &text, x0, y0, bw, bh, (bh as f32 * 0.95) as i32, 0x00FFFFFF);
        }
        // GDI leaves alpha at 0: make every drawn pixel opaque.
        for p in px.iter_mut() {
            if *p != 0 {
                *p |= 0xFF00_0000;
            }
        }
        SelectObject(dc, old);
        DeleteDC(dc);
        let mask = CreateBitmap(sz, sz, 1, 1, null());
        let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: bmp };
        let icon = CreateIconIndirect(&ii);
        DeleteObject(mask);
        DeleteObject(bmp);
        icon
    }
}

unsafe fn draw_text(dc: HDC, text: &str, x: i32, y: i32, cw: i32, ch: i32, height: i32, color: u32) {
    let f = CreateFontW(-height, 0, 0, 0, 700, 0, 0, 0, 1, 0, 0, ANTIALIASED_QUALITY as u32, 0, w("Segoe UI").as_ptr());
    let of = SelectObject(dc, f);
    SetTextColor(dc, color);
    let mut r = RECT { left: x, top: y, right: x + cw, bottom: y + ch };
    let mut t: Vec<u16> = text.encode_utf16().collect();
    DrawTextW(dc, t.as_mut_ptr(), t.len() as i32, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(dc, of);
    DeleteObject(f);
}

fn tray_data() -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = app().host;
    nid.uID = 1;
    nid
}

fn tray_add() {
    let mut nid = tray_data();
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = app().icon;
    copy_w(&mut nid.szTip, APP_NAME);
    unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
}

fn set_badge(count: i32, tip: &str) {
    let a = app();
    let mut nid = tray_data();
    nid.uFlags = NIF_TIP;
    copy_w(&mut nid.szTip, tip);
    let old = a.icon;
    if count != a.badge {
        a.icon = make_icon(count);
        a.badge = count;
        nid.uFlags |= NIF_ICON;
        nid.hIcon = a.icon;
    }
    unsafe {
        Shell_NotifyIconW(NIM_MODIFY, &nid);
        if old != a.icon {
            DestroyIcon(old);
        }
    }
}

/// Windows shows this as a toast with the notification sound.
fn notify(title: &str, body: &str) {
    if app().window_mode {
        return;
    }
    app().notice_page = 0;
    let mut nid = tray_data();
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    copy_w(&mut nid.szInfoTitle, title);
    copy_w(&mut nid.szInfo, body);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
}

// ---------- reminders, budget, payments (tray) ----------

struct Due {
    key: String,
    text: String,
    trial: bool,
}

fn due_list() -> Vec<Due> {
    let a = app();
    let t = a.today;
    let mut v: Vec<(i32, Due)> = a
        .store
        .subs
        .iter()
        .filter(|s| s.active)
        .filter_map(|s| {
            let d = next_due(s, t);
            let left = d - t;
            if left > s.remind.max(0) {
                return None;
            }
            let trial = s.in_trial(t);
            let text = if trial {
                format!("{} trial ends {} – then {}", s.name, due_text(left), money(s.price, &s.cur))
            } else {
                format!("{} {} – {}", s.name, money(s.price, &s.cur), due_text(left))
            };
            Some((d, Due { key: format!("{}|{}", s.id, d), text, trial }))
        })
        .collect();
    v.sort_by_key(|x| x.0);
    v.into_iter().map(|x| x.1).collect()
}

fn window_open() -> bool {
    unsafe { !FindWindowW(w("SubscribityWindow").as_ptr(), null()).is_null() }
}

fn check_reminders(show_toast: bool) {
    let a = app();
    a.today = today();
    let due = due_list();
    let keys: HashSet<String> = due.iter().map(|d| d.key.clone()).collect();
    a.store.notified.retain(|k| keys.contains(k));
    a.store.ack.retain(|k| keys.contains(k));
    let fresh: Vec<&Due> = due.iter().filter(|d| !a.store.notified.contains(&d.key)).collect();
    if !fresh.is_empty() {
        for d in &fresh {
            a.store.notified.insert(d.key.clone());
        }
        if show_toast {
            let trials = fresh.iter().filter(|d| d.trial).count();
            let title = match (fresh.len(), trials) {
                (1, 1) => "Free trial ending soon".to_string(),
                (1, _) => "Subscription renewing soon".to_string(),
                (n, 0) => format!("{} subscriptions renewing soon", n),
                (n, _) => format!("{} renewals and trials coming up", n),
            };
            let mut body: Vec<String> = fresh.iter().take(4).map(|d| d.text.clone()).collect();
            if fresh.len() > 4 {
                body.push(format!("+{} more", fresh.len() - 4));
            }
            notify(&title, &body.join("\n"));
        }
    }
    if window_open() {
        a.store.ack.extend(keys.iter().cloned());
    }
    a.store.save_state();
    let unseen = due.iter().filter(|d| !a.store.ack.contains(&d.key)).count() as i32;
    let (total, _) = a.store.monthly_total();
    let mut tip = format!("{} – {}/month", APP_NAME, money(total, &a.store.settings.main_cur));
    if !due.is_empty() {
        tip.push_str(&format!("\n{} due soon", due.len()));
    }
    set_badge(unseen, &tip);
    check_budget();
}

fn check_budget() {
    let a = app();
    let budget = a.store.settings.budget;
    let (total, _) = a.store.monthly_total();
    let over = budget > 0.0 && total > budget + 0.005;
    if over && !a.store.settings.budget_alerted {
        let main = a.store.settings.main_cur.clone();
        notify("Over your monthly budget", &format!("Subscriptions now cost {}/month, your budget is {}.", money(total, &main), money(budget, &main)));
        a.store.edit_settings(|s| s.budget_alerted = true);
    } else if !over && a.store.settings.budget_alerted {
        a.store.edit_settings(|s| s.budget_alerted = false);
    }
}

/// Daily housekeeping in the tray: payment log and weekly backup.
fn housekeeping() {
    let a = app();
    a.store.log_payments(a.today);
    a.store.backup_if_due(a.today);
}

/// icons.rs cache key: App Store artwork URL, else the registrable domain of the site; "" = none.
fn icon_key(s: &Sub) -> String {
    if !s.icon.is_empty() {
        return s.icon.clone();
    }
    let u = s.url.trim();
    let u = u.split_once("://").map_or(u, |x| x.1);
    let host = u.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or("").split(':').next().unwrap_or("");
    if host.contains('.') { services::registrable(host) } else { String::new() }
}

/// Keys of every subscription, rows shown in the popup first, deduped, non-empty.
fn icon_keys(st: &Store) -> Vec<String> {
    let t = today();
    let mut soon: Vec<(i32, String)> = st.subs.iter().filter(|s| s.active).map(|s| (next_due(s, t), icon_key(s))).collect();
    soon.sort_by_key(|x| x.0);
    let mut keys: Vec<String> = soon.into_iter().map(|x| x.1).collect();
    keys.extend(st.subs.iter().map(icon_key));
    let mut seen = HashSet::new();
    keys.retain(|k| !k.is_empty() && seen.insert(k.clone()));
    keys
}

/// Popup opened: start the icon helper if something is missing (at most every 10 minutes).
fn maybe_fetch_icons() {
    static mut LAST: Option<std::time::Instant> = None;
    if std::env::var_os("SUBSCRIBITY_NO_INTEGRATION").is_some_and(|v| !v.is_empty()) {
        return;
    }
    unsafe {
        #[allow(static_mut_refs)]
        if LAST.is_some_and(|t| t.elapsed().as_secs() < 600) {
            return;
        }
        if icons::pending(&data_dir(), &icon_keys(&app().store)) {
            LAST = Some(std::time::Instant::now());
            shell_open(&system::exe().to_string_lossy(), Some("--icons"));
        }
    }
}

/// `--icons`: fetch missing icons, then tell the tray.
fn icons_helper() {
    unsafe {
        CreateMutexW(null(), 1, w(icons::MUTEX).as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
    }
    let dir = data_dir();
    let store = Store::load(dir.clone());
    if icons::fetch_missing(&dir, &icon_keys(&store)) > 0 {
        unsafe {
            let tray = FindWindowW(w("SubscribityHost").as_ptr(), null());
            if !tray.is_null() && scan::same_exe(tray) {
                PostMessageW(tray, WM_ICONS, 0, 0);
            }
        }
    }
}

fn popup_data() -> popup::Data {
    let a = app();
    let t = today();
    let st = &a.store;
    let main = st.settings.main_cur.clone();
    let (total, missing) = st.monthly_total();
    let active = st.subs.iter().filter(|s| s.active).count();
    let mut items: Vec<(i32, popup::Item)> = st
        .subs
        .iter()
        .filter(|s| s.active)
        .map(|s| {
            let d = next_due(s, t);
            let left = d - t;
            let trial = s.in_trial(t);
            let when = if trial { format!("Trial ends {}", due_text(left)) } else { format!("{} · {}", fmt_date(d), due_text(left)) };
            let tone = if trial { 3 } else if left == 0 { 2 } else if left <= s.remind { 1 } else { 0 };
            (d, popup::Item { id: s.id, name: s.name.clone(), when, price: money(s.price, &s.cur), tone, icon: icon_key(s) })
        })
        .filter(|(d, _)| d - t <= 30)
        .collect();
    items.sort_by_key(|x| x.0);
    popup::Data {
        total: money(total, &main) + if missing { "*" } else { "" },
        caption: format!("{} active", active),
        budget: (st.settings.budget > 0.0).then(|| {
            let b = st.settings.budget;
            let label = if total > b {
                format!("{} over your {} budget", money(total - b, &main), money(b, &main))
            } else {
                format!("{} left of your {} budget", money(b - total, &main), money(b, &main))
            };
            ((total / b) as f32, label)
        }),
        items: items.into_iter().take(6).map(|x| x.1).collect(),
        found: st.settings.scan_new,
    }
}

fn on_popup_action(action: popup::Action) {
    match action {
        popup::Action::Open => open_window(0),
        popup::Action::Add => open_window(2),
        popup::Action::Edit(id) => open_window(1000 + id as usize),
        popup::Action::Review => open_window(3),
    }
}

// ---------- email scan (tray side: start the weekly headless scan, announce results) ----------

fn maybe_auto_scan() {
    let a = app();
    let s = &a.store.settings;
    if s.scan_auto && s.scan_error.is_empty() && a.today - s.scan_last >= 7 && a.scan_try != a.today
        && scan::store::has_key(&data_dir()) && !window_open()
    {
        a.scan_try = a.today; // a failed scan (offline) is retried the next day
        shell_open(&system::exe().to_string_lossy(), Some("--scan"));
    }
}

fn scan_notice() {
    let a = app();
    let (found, auth) = (a.store.settings.scan_new, a.store.settings.scan_error == "auth");
    if found > a.scan_new_seen {
        let text = if found == 1 { "1 new subscription found in your email".to_string() } else { format!("{} new subscriptions found in your email", found) };
        notify(APP_NAME, &text);
        a.notice_page = 3;
    }
    a.scan_new_seen = found;
    if auth && !a.scan_err_seen {
        notify(APP_NAME, "Email scan stopped: reconnect your email in Subscribity");
        a.notice_page = 3;
    }
    a.scan_err_seen = auth;
}

// ---------- exchange rates ----------

fn maybe_fetch_rates(force: bool) {
    let a = app();
    let main = a.store.settings.main_cur.clone();
    let needs = a.store.subs.iter().any(|s| s.cur != main);
    let stale = a.store.settings.rates_date != a.today || a.store.settings.rates_base != main;
    let slot = (a.today, local_time().wHour as i32);
    if a.rates_busy || (!force && (!needs || !stale || a.rates_try == slot)) {
        return;
    }
    a.rates_busy = true;
    a.rates_try = slot;
    let host = a.host as isize;
    std::thread::spawn(move || {
        let result = http_get("open.er-api.com", &format!("/v6/latest/{}", main))
            .and_then(|body| parse_rates(&body))
            .map(|r| (main, r));
        unsafe { PostMessageW(host as HWND, WM_RATES, 0, Box::into_raw(Box::new(result)) as isize) };
    });
}

fn http_get(host: &str, path: &str) -> Option<String> {
    unsafe {
        let ses = WinHttpOpen(w(APP_NAME).as_ptr(), 4 /* automatic proxy */, null(), null(), 0);
        if ses.is_null() {
            return None;
        }
        let mut out = None;
        let con = WinHttpConnect(ses, w(host).as_ptr(), 443, 0);
        if !con.is_null() {
            let req = WinHttpOpenRequest(con, w("GET").as_ptr(), w(path).as_ptr(), null(), null(), null(), 0x0080_0000 /* secure */);
            if !req.is_null() {
                if WinHttpSendRequest(req, null(), 0, null(), 0, 0, 0) != 0 && WinHttpReceiveResponse(req, null_mut()) != 0 {
                    let mut body = Vec::new();
                    loop {
                        let mut avail = 0u32;
                        if WinHttpQueryDataAvailable(req, &mut avail) == 0 || avail == 0 || body.len() > 1 << 20 {
                            break;
                        }
                        let mut buf = vec![0u8; avail as usize];
                        let mut read = 0u32;
                        if WinHttpReadData(req, buf.as_mut_ptr() as *mut c_void, avail, &mut read) == 0 || read == 0 {
                            break;
                        }
                        body.extend_from_slice(&buf[..read as usize]);
                    }
                    out = String::from_utf8(body).ok();
                }
                WinHttpCloseHandle(req);
            }
            WinHttpCloseHandle(con);
        }
        WinHttpCloseHandle(ses);
        out
    }
}

fn rates_status() -> String {
    let a = app();
    let st = &a.store.settings;
    if a.rates_busy {
        "Updating…".into()
    } else if st.rates_date > 0 && st.rates_base == st.main_cur {
        format!("Updated {} · refreshed daily", fmt_date(st.rates_date))
    } else {
        "Downloaded when you add a subscription in another currency".into()
    }
}

fn on_rates(lp: LPARAM) {
    let a = app();
    a.rates_busy = false;
    let res = unsafe { Box::from_raw(lp as *mut Option<(String, HashMap<String, f64>)>) };
    match *res {
        Some((base, rates)) => {
            a.store.rates = rates;
            a.store.save_rates();
            let day = today();
            a.store.edit_settings(|s| {
                s.rates_base = base;
                s.rates_date = day;
            });
            send_js(&json!({"type": "toast", "text": "Exchange rates updated"}));
            if a.window_mode {
                after_change();
            } else {
                check_budget();
            }
        }
        None => send_js(&json!({"type": "toast", "text": "Could not download exchange rates"})),
    }
    send_js(&state_json());
    trim_memory();
}

// ---------- updates ----------

fn update_status() -> String {
    let a = app();
    let st = &a.store.settings;
    if a.update_busy {
        "Working…".into()
    } else if update::is_newer(&st.latest_version, VERSION) {
        format!("Version {} is available (you have {})", st.latest_version, VERSION)
    } else if st.update_last > 0 {
        format!("Version {} is the latest · checked {}", VERSION, fmt_date(st.update_last))
    } else {
        format!("Version {}", VERSION)
    }
}

fn maybe_check_updates(force: bool) {
    let a = app();
    let days = match a.store.settings.update_freq.as_str() {
        "monthly" => 30,
        "off" => i32::MAX,
        _ => 7,
    };
    if a.update_busy || (!force && (days == i32::MAX || a.today - a.store.settings.update_last < days)) {
        return;
    }
    a.update_busy = true;
    let host = a.host as isize;
    std::thread::spawn(move || {
        let r = update::latest().map(|rel| rel.version);
        unsafe { PostMessageW(host as HWND, WM_UPDATE, 0, Box::into_raw(Box::new(r)) as isize) };
    });
}

fn on_update(lp: LPARAM) {
    let a = app();
    a.update_busy = false;
    let res = unsafe { Box::from_raw(lp as *mut Result<String, String>) };
    let day = today();
    match *res {
        Ok(v) => {
            let newer = update::is_newer(&v, VERSION);
            let announce = newer && !a.window_mode && a.store.settings.update_notified != v;
            a.store.edit_settings(|s| {
                s.latest_version = v.clone();
                s.update_last = day;
                if announce {
                    s.update_notified = v.clone();
                }
            });
            if announce {
                notify(&format!("{} {} is available", APP_NAME, v), "Click to open the app and install it with one click.");
            }
            send_js(&json!({"type": "toast", "text": if newer { format!("Version {} is available", v) } else { "You have the latest version".to_string() }}));
        }
        Err(e) => {
            a.store.edit_settings(|s| s.update_last = day); // try again next period, not every few minutes
            send_js(&json!({"type": "toast", "text": format!("Could not check for updates: {}", e)}));
        }
    }
    send_js(&state_json());
}

fn install_update() {
    let a = app();
    if a.update_busy {
        return;
    }
    a.update_busy = true;
    send_js(&state_json());
    let host = a.host as isize;
    std::thread::spawn(move || {
        let r = update::latest()
            .and_then(|rel| {
                if !update::is_newer(&rel.version, VERSION) {
                    return Err("you already have the latest version".into());
                }
                update::download_verified(&rel).map(|exe| (rel.version, exe))
            })
            .and_then(|(v, exe)| update::swap_in(&system::exe(), &exe).map(|_| v));
        unsafe { PostMessageW(host as HWND, WM_INSTALLED, 0, Box::into_raw(Box::new(r)) as isize) };
    });
}

fn on_installed(lp: LPARAM) {
    let a = app();
    a.update_busy = false;
    let res = unsafe { Box::from_raw(lp as *mut Result<String, String>) };
    match *res {
        Ok(_) => unsafe {
            // Stop the old tray, start the new version, close this window.
            if !a.tray.is_null() {
                PostMessageW(a.tray, WM_CLOSE, 0, 0);
                for _ in 0..50 {
                    if FindWindowW(w("SubscribityHost").as_ptr(), null()).is_null() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            shell_open(&system::exe().to_string_lossy(), Some("--updated --open"));
            close_window();
        },
        Err(e) => {
            send_js(&json!({"type": "toast", "text": format!("Update failed: {}", e)}));
            send_js(&state_json());
        }
    }
}

// ---------- window (separate process) ----------

struct Win(HWND);
impl HasWindowHandle for Win {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let h = Win32WindowHandle::new(NonZeroIsize::new(self.0 as isize).ok_or(HandleError::Unavailable)?);
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(h)) })
    }
}

fn find_window() -> HWND {
    unsafe { FindWindowW(w("SubscribityWindow").as_ptr(), null()) }
}

/// Tray side: show the window (codes: 0 main, 1 settings, 2 add, 3 scan review, 1000+id edit).
fn open_window(code: usize) {
    let h = find_window();
    if !h.is_null() {
        unsafe {
            ShowWindow(h, SW_RESTORE);
            SetForegroundWindow(h);
            if code != 0 {
                PostMessageW(h, WM_OPENPAGE, code, 0);
            }
        }
        return;
    }
    let args = match code {
        0 => "--window".to_string(),
        1 => "--window --settings".to_string(),
        2 => "--window --add".to_string(),
        3 => "--window --found".to_string(),
        c => format!("--window --edit={}", c - 1000),
    };
    shell_open(&system::exe().to_string_lossy(), Some(&args));
}

fn open_msg(code: usize) -> Option<Value> {
    match code {
        1 => Some(json!({"type": "open", "page": "settings"})),
        2 => Some(json!({"type": "open", "page": "subs", "add": true})),
        3 => Some(json!({"type": "open", "page": "scan"})),
        c if c >= 1000 => Some(json!({"type": "open", "page": "subs", "edit": c - 1000})),
        _ => None,
    }
}

/// Window process: data changed, the tray reloads it.
fn after_change() {
    let a = app();
    if a.window_mode {
        unsafe { PostMessageW(a.tray, WM_RELOAD, 0, 0) };
    } else {
        check_reminders(true);
    }
}

fn create_window(code: usize) {
    let a = app();
    unsafe {
        // Always open centred on the monitor under the mouse (where the tray click was),
        // at the remembered size if there is one, never larger than that monitor's work area.
        let mut wa: RECT = std::mem::zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut _ as *mut c_void, 0);
        let mut pt = POINT { x: 0, y: 0 };
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetCursorPos(&mut pt) != 0 && GetMonitorInfoW(MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST), &mut mi) != 0 {
            wa = mi.rcWork;
        }
        let (_, _, sw, sh) = a.store.settings.window;
        let (dw, dh) = if sw > 200 && sh > 200 { (sw, sh) } else { ((1040.0 * a.scale) as i32, (720.0 * a.scale) as i32) };
        let cw = dw.min(wa.right - wa.left - 24);
        let ch = dh.min(wa.bottom - wa.top - 24);
        let x = wa.left + (wa.right - wa.left - cw) / 2;
        let y = wa.top + (wa.bottom - wa.top - ch) / 2;
        a.win = CreateWindowExW(0, w("SubscribityWindow").as_ptr(), w(APP_NAME).as_ptr(), WS_OVERLAPPEDWINDOW, x, y, cw, ch, null_mut(), null_mut(), a.hinst, null());
        dwm_set(a.win, 20, 1); // dark title bar
        dwm_set(a.win, 35, 0x00271D1B); // caption color = sidebar (Windows 11)
    }
    a.host = a.win;
    a.page_ready = false;
    a.pending.clear();
    if let Some(m) = open_msg(code) {
        a.pending.push(m.to_string());
    }

    let host = a.host as isize;
    let profile = std::env::var("LOCALAPPDATA")
        .map(|p| std::path::PathBuf::from(p).join("Subscribity").join("WebView2"))
        .unwrap_or_else(|_| data_dir().join("webview"));
    let mut ctx = wry::WebContext::new(Some(profile));
    let built = wry::WebViewBuilder::with_web_context(&mut ctx)
        .with_html(UI_HTML)
        .with_background_color((18, 19, 26, 255))
        .with_theme(wry::Theme::Dark)
        .with_additional_browser_args(BROWSER_ARGS)
        .with_devtools(cfg!(debug_assertions))
        .with_ipc_handler(move |req| {
            let body = Box::into_raw(Box::new(req.body().clone()));
            unsafe { PostMessageW(host as HWND, WM_IPC, 0, body as isize) };
        })
        .with_new_window_req_handler(|url| {
            if url.starts_with("https://") {
                shell_open(&url, None);
            }
            false
        })
        .build(&Win(a.win));
    match built {
        Ok(wv) => a.webview = Some(wv),
        Err(e) => unsafe {
            DestroyWindow(a.win);
            a.win = null_mut();
            let text = format!("Could not open the window: {}\n\nSubscribity needs the Microsoft Edge WebView2 Runtime, which is part of Windows 11.", e);
            MessageBoxW(null_mut(), w(&text).as_ptr(), w(APP_NAME).as_ptr(), 0x10);
            PostQuitMessage(1);
            return;
        },
    }
    unsafe {
        ShowWindow(a.win, SW_SHOW);
        SetForegroundWindow(a.win);
    }
}

fn close_window() {
    let a = app();
    if !a.win.is_null() {
        unsafe {
            let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
            wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            // Screen coordinates, as CreateWindowExW takes them. rcNormalPosition is in
            // workspace coordinates (offset by a top/left taskbar), so use it only when the
            // window is minimized or maximized and its own rect isn't the normal one.
            let mut r: RECT = std::mem::zeroed();
            let ok = if IsIconic(a.win) == 0 && IsZoomed(a.win) == 0 {
                GetWindowRect(a.win, &mut r) != 0
            } else if GetWindowPlacement(a.win, &mut wp) != 0 {
                r = wp.rcNormalPosition;
                true
            } else {
                false
            };
            if ok {
                a.store.edit_settings(|s| s.window = (r.left, r.top, r.right - r.left, r.bottom - r.top));
            }
        }
    }
    a.webview = None;
    a.page_ready = false;
    a.pending.clear();
    if !a.win.is_null() {
        let h = a.win;
        a.win = null_mut();
        unsafe { DestroyWindow(h) };
    }
    unsafe { PostQuitMessage(0) }; // the window process ends; Windows reclaims everything
}

fn eval(v: &str) {
    if let Some(wv) = &app().webview {
        let _ = wv.evaluate_script(&format!("window.__recv({})", v));
    }
}

fn send_js(v: &Value) {
    let a = app();
    if a.webview.is_none() {
        return;
    }
    if !a.page_ready {
        a.pending.push(v.to_string());
        return;
    }
    eval(&v.to_string());
}

fn state_json() -> Value {
    let a = app();
    let t = today();
    a.today = t;
    let st = &a.store;
    let subs: Vec<Value> = st
        .subs
        .iter()
        .map(|s| {
            let d = next_due(s, t);
            json!({
                "id": s.id, "name": s.name, "price": s.price, "cur": s.cur,
                "every": s.every.max(1), "unit": s.unit, "next": iso(d), "left": d - t,
                "category": s.category, "payment": s.payment, "remind": s.remind,
                "active": s.active, "notes": s.notes, "url": s.url,
                "icon": s.icon, "first": if s.first > 0 { iso(s.first) } else { String::new() },
                "trial": s.trial, "inTrial": s.in_trial(t),
                "history": s.history.iter().map(|(d, p)| json!({"date": iso(*d), "price": p})).collect::<Vec<_>>(),
                "monthly": monthly(s),
                "monthlyMain": st.to_main(monthly(s), &s.cur),
                "priceMain": st.to_main(s.price, &s.cur),
            })
        })
        .collect();
    let since = t - 800;
    let payments: Vec<Value> = st
        .payments
        .iter()
        .rev()
        .filter(|p| p.date >= since)
        .take(1000)
        .map(|p| json!({"id": p.sub_id, "date": iso(p.date), "amount": p.amount, "cur": p.cur, "name": p.name, "amountMain": st.to_main(p.amount, &p.cur)}))
        .collect();
    let backups: Vec<Value> = st.backups().into_iter().map(|n| json!({"name": n})).collect();
    let (total, _) = st.monthly_total();
    json!({
        "type": "state",
        "version": VERSION,
        "today": iso(t),
        "subs": subs,
        "payments": payments,
        "backups": backups,
        "monthTotal": total,
        "currencies": CURRENCIES,
        "templates": services::templates_json(),
        "paypalColumns": detect::paypal::paypal_column_aliases(),
        "scan": scan::state(),
        "settings": {
            "main": st.settings.main_cur,
            "remind": st.settings.default_remind,
            "budget": st.settings.budget,
            "autostart": system::autostart_enabled(),
            "ratesStatus": rates_status(),
            "updateFreq": st.settings.update_freq,
            "latest": if update::is_newer(&st.settings.latest_version, VERSION) { st.settings.latest_version.as_str() } else { "" },
            "updateStatus": update_status(),
            "updateBusy": a.update_busy,
        }
    })
}

fn calendar_json(y: i32, m: u32) -> Value {
    let a = app();
    let first = days_from_civil(y, m, 1);
    let last = if m == 12 { days_from_civil(y + 1, 1, 1) } else { days_from_civil(y, m + 1, 1) } - 1;
    let mut events = Vec::new();
    for s in a.store.subs.iter().filter(|s| s.active) {
        let mut d = next_due(s, first);
        let mut guard = 0;
        while d <= last && guard < 40 {
            let (_, _, day) = civil_from_days(d);
            events.push(json!({"day": day, "id": s.id, "name": s.name, "price": s.price, "cur": s.cur, "priceMain": a.store.to_main(s.price, &s.cur), "trial": s.in_trial(d)}));
            d = next_due(s, d + 1);
            guard += 1;
        }
    }
    json!({"type": "calendar", "y": y, "m": m, "events": events})
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string()
}

/// Builds a subscription from the window's form (or an import row).
fn sub_from_json(s: &Value, id: u32, default_cur: &str, default_remind: i32) -> Option<Sub> {
    let name: String = str_of(s, "name").chars().take(80).collect();
    let price = s.get("price").and_then(|p| p.as_f64()).filter(|p| p.is_finite() && *p >= 0.0)?;
    let start = parse_iso(&str_of(s, "start"))?;
    if name.is_empty() {
        return None;
    }
    let cur = str_of(s, "cur").to_uppercase();
    Some(Sub {
        id,
        name,
        price,
        cur: if cur.len() == 3 { cur } else { default_cur.to_string() },
        every: s.get("every").and_then(|x| x.as_u64()).unwrap_or(1).clamp(1, 999) as u32,
        unit: s.get("unit").and_then(|x| x.as_u64()).unwrap_or(2).min(3) as u8,
        start,
        category: str_of(s, "category").chars().take(40).collect(),
        payment: str_of(s, "payment").chars().take(40).collect(),
        remind: s.get("remind").and_then(|x| x.as_i64()).map_or(default_remind, |r| r.clamp(0, 365) as i32),
        active: s.get("active").and_then(|x| x.as_bool()).unwrap_or(true),
        notes: s.get("notes").and_then(|x| x.as_str()).unwrap_or("").chars().take(500).collect(),
        url: str_of(s, "url").chars().take(200).collect(),
        trial: s.get("trial").and_then(|x| x.as_bool()).unwrap_or(false),
        history: Vec::new(),
        icon: Some(str_of(s, "icon")).filter(|u| scan::appstore::is_artwork(u)).unwrap_or_default(),
        // A first charge in the future is no first charge.
        first: parse_iso(&str_of(s, "first")).filter(|&f| f > 0 && f <= today()).unwrap_or(0),
    })
}

fn handle_ipc(body: &str) {
    let Ok(msg) = serde_json::from_str::<Value>(body) else { return };
    let a = app();
    let main_cur = a.store.settings.main_cur.clone();
    let default_remind = a.store.settings.default_remind;
    match msg.get("cmd").and_then(|c| c.as_str()).unwrap_or("") {
        "ready" => {
            a.page_ready = true;
            eval(&state_json().to_string());
            for p in std::mem::take(&mut a.pending) {
                eval(&p);
            }
            after_change(); // lets the tray clear its badge
        }
        "save" => {
            let id = msg["sub"].get("id").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            let existing = a.store.subs.iter().position(|x| x.id == id && id != 0);
            let new_id = if existing.is_some() { id } else { a.store.next_id() };
            let Some(mut sub) = sub_from_json(&msg["sub"], new_id, &main_cur, default_remind) else {
                send_js(&json!({"type": "error", "text": "Please check the name, price and date."}));
                return;
            };
            if let Some(i) = existing {
                let old = &a.store.subs[i];
                // The form shows the next payment, not the original billing day. If it wasn't
                // changed, keep the original day (a bill on the 31st stays on the 31st).
                if sub.start == next_due(old, a.today) && sub.every == old.every && sub.unit == old.unit {
                    sub.start = old.start;
                }
                sub.history = old.history.clone();
                // The form resends both; a missing `first` keeps the stored one, "icon" = "" clears it.
                if msg["sub"].get("first").is_none() {
                    sub.first = old.first;
                }
                if msg["sub"].get("icon").is_none() {
                    sub.icon = old.icon.clone();
                }
                if sub.cur != old.cur {
                    sub.history.clear();
                } else if (sub.price - old.price).abs() > 0.004 {
                    sub.history.push((a.today, old.price));
                    if sub.history.len() > 20 {
                        sub.history.remove(0);
                    }
                }
                a.store.subs[i] = sub;
            } else {
                a.store.subs.push(sub);
            }
            a.store.save_subs();
            send_js(&state_json());
            send_js(&json!({"type": "saved", "text": if existing.is_some() { "Saved" } else { "Subscription added" }}));
            scan::subs_changed();
            after_change();
            maybe_fetch_rates(false);
        }
        "delete" => {
            let id = msg.get("id").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            a.store.subs.retain(|s| s.id != id);
            a.store.save_subs();
            send_js(&state_json());
            send_js(&json!({"type": "toast", "text": "Deleted"}));
            scan::subs_changed();
            after_change();
        }
        "import" => {
            let mut added = 0;
            for row in msg.get("items").and_then(|i| i.as_array()).map(|v| v.as_slice()).unwrap_or(&[]) {
                let id = a.store.next_id();
                if let Some(s) = sub_from_json(row, id, &main_cur, default_remind) {
                    a.store.subs.push(s);
                    added += 1;
                }
            }
            a.store.save_subs();
            send_js(&state_json());
            send_js(&json!({"type": "toast", "text": format!("Imported {} subscription{}", added, if added == 1 { "" } else { "s" })}));
            scan::subs_changed();
            after_change();
            maybe_fetch_rates(false);
        }
        "delete_payment" => {
            let id = msg.get("id").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            if let Some(d) = parse_iso(&str_of(&msg, "date")) {
                a.store.delete_payment(id, d);
            }
            send_js(&state_json());
        }
        "settings" => {
            let main = str_of(&msg, "main");
            let remind = msg.get("remind").and_then(|x| x.as_i64()).unwrap_or(3).clamp(0, 365) as i32;
            let budget = msg.get("budget").and_then(|x| x.as_f64()).filter(|b| b.is_finite() && *b >= 0.0).unwrap_or(0.0);
            a.store.edit_settings(|s| {
                if main.len() == 3 {
                    s.main_cur = main;
                }
                s.default_remind = remind;
                s.budget = budget;
            });
            maybe_fetch_rates(false);
            send_js(&state_json());
            after_change();
        }
        "autostart" => {
            system::set_autostart(msg.get("on").and_then(|x| x.as_bool()).unwrap_or(false));
            send_js(&state_json());
        }
        "rates" => {
            maybe_fetch_rates(true);
            send_js(&state_json());
        }
        "calendar" => {
            let y = msg.get("y").and_then(|x| x.as_i64()).unwrap_or(2026) as i32;
            let m = msg.get("m").and_then(|x| x.as_u64()).unwrap_or(1).clamp(1, 12) as u32;
            send_js(&calendar_json(y, m));
        }
        "restore" => {
            let name = str_of(&msg, "name");
            if a.store.restore(&name, a.today) {
                send_js(&json!({"type": "toast", "text": "Backup restored"}));
                after_change();
            } else {
                send_js(&json!({"type": "toast", "text": "Could not restore that backup"}));
            }
            send_js(&state_json());
        }
        "open_folder" => shell_open(&data_dir().to_string_lossy(), None),
        "export" => {
            let mut csv = String::from("Name,Price,Currency,Cycle,Next payment,Category,Payment method,Active,Website,Notes\r\n");
            let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\"").replace(['\r', '\n'], " "));
            for s in &a.store.subs {
                csv.push_str(&format!(
                    "{},{},{},{},{},{},{},{},{},{}\r\n",
                    q(&s.name), s.price, s.cur, q(&cycle_text(s)), iso(next_due(s, a.today)),
                    q(&s.category), q(&s.payment), if s.active { "yes" } else { "no" }, q(&s.url), q(&s.notes)
                ));
            }
            if std::fs::write(data_dir().join(data::EXPORT_FILE), csv).is_ok() {
                send_js(&json!({"type": "toast", "text": format!("Saved {}", data::EXPORT_FILE)}));
                shell_open(&data_dir().to_string_lossy(), None);
            }
        }
        "update_check" => maybe_check_updates(true),
        "update_install" => install_update(),
        "update_freq" => {
            let f = str_of(&msg, "freq");
            if ["weekly", "monthly", "off"].contains(&f.as_str()) {
                a.store.edit_settings(|s| s.update_freq = f);
            }
            send_js(&state_json());
        }
        "open_release" => shell_open(&format!("https://github.com/{}/releases/latest", update::REPO), None),
        "open_support" => shell_open(SUPPORT_URL, None),
        "uninstall" => shell_open(&system::exe().to_string_lossy(), Some("--uninstall")),
        c => {
            scan::handle_ipc(c, &msg);
        }
    }
}

unsafe extern "system" fn win_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            if let Some(wv) = &app().webview {
                let (cw, ch) = ((lp & 0xFFFF) as u32, ((lp >> 16) & 0xFFFF) as u32);
                let _ = wv.set_bounds(wry::Rect {
                    position: wry::dpi::PhysicalPosition::new(0, 0).into(),
                    size: wry::dpi::PhysicalSize::new(cw, ch).into(),
                });
            }
            0
        }
        WM_CLOSE => {
            close_window();
            0
        }
        WM_IPC => {
            let body = Box::from_raw(lp as *mut String);
            handle_ipc(&body);
            0
        }
        WM_RATES => {
            on_rates(lp);
            0
        }
        WM_UPDATE => {
            on_update(lp);
            0
        }
        WM_INSTALLED => {
            on_installed(lp);
            0
        }
        WM_SCAN => {
            scan::on_message(wp, lp);
            0
        }
        WM_OPENPAGE => {
            if let Some(m) = open_msg(wp) {
                send_js(&m);
            }
            0
        }
        WM_GETMINMAXINFO => {
            let mm = &mut *(lp as *mut MINMAXINFO);
            let sc = app().scale;
            mm.ptMinTrackSize = POINT { x: (560.0 * sc) as i32, y: (420.0 * sc) as i32 };
            0
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

// ---------- hidden tray host window ----------

fn tray_menu() {
    unsafe {
        let a = app();
        popup::close();
        let m = CreatePopupMenu();
        AppendMenuW(m, 0, M_OPEN, w("Open Subscribity").as_ptr());
        AppendMenuW(m, 0, M_ADD, w("Add subscription…").as_ptr());
        AppendMenuW(m, 0, M_SETTINGS, w("Settings").as_ptr());
        AppendMenuW(m, 0x800, 0, null());
        AppendMenuW(m, if system::autostart_enabled() { 8 } else { 0 }, M_AUTOSTART, w("Start with Windows").as_ptr());
        AppendMenuW(m, 0x800, 0, null());
        AppendMenuW(m, 0, M_EXIT, w("Exit").as_ptr());
        SetMenuDefaultItem(m, M_OPEN as u32, 0);
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(a.host);
        let cmd = TrackPopupMenu(m, 0x100 | 0x2, pt.x, pt.y, 0, a.host, null());
        PostMessageW(a.host, WM_NULL, 0, 0);
        DestroyMenu(m);
        match cmd as usize {
            M_OPEN => open_window(0),
            M_ADD => open_window(2),
            M_SETTINGS => open_window(1),
            M_AUTOSTART => system::set_autostart(!system::autostart_enabled()),
            M_EXIT => {
                let win = find_window();
                if !win.is_null() {
                    PostMessageW(win, WM_CLOSE, 0, 0);
                }
                DestroyWindow(a.host);
            }
            _ => {}
        }
    }
}

unsafe extern "system" fn host_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let a = app();
    match msg {
        WM_TRAY => {
            match (lp & 0xFFFF) as u32 {
                0x202 /* WM_LBUTTONUP */ => {
                    check_reminders(false);
                    popup::toggle(a.hinst, a.scale, (a.host, 1), popup_data, on_popup_action);
                    maybe_fetch_icons();
                }
                0x405 /* NIN_BALLOONUSERCLICK */ => open_window(a.notice_page),
                0x205 /* WM_RBUTTONUP */ => tray_menu(),
                _ => {}
            }
            0
        }
        WM_OPEN => {
            open_window(0);
            0
        }
        WM_ICONS => {
            popup::reload_icons();
            0
        }
        WM_RELOAD => {
            let notified = std::mem::take(&mut a.store.notified);
            let ack = std::mem::take(&mut a.store.ack);
            a.store = Store::load(data_dir());
            a.store.notified = notified;
            a.store.ack = ack;
            housekeeping();
            check_reminders(true);
            maybe_fetch_rates(false);
            scan_notice();
            trim_memory();
            0
        }
        WM_TIMER => {
            a.today = today();
            a.store.reload_settings();
            housekeeping();
            check_reminders(true);
            maybe_fetch_rates(false);
            maybe_check_updates(false);
            maybe_auto_scan();
            0
        }
        WM_RATES => {
            on_rates(lp);
            0
        }
        WM_UPDATE => {
            on_update(lp);
            0
        }
        WM_POWERBROADCAST => {
            if wp == 0x12 /* PBT_APMRESUMEAUTOMATIC */ {
                PostMessageW(h, WM_TIMER, TIMER_TICK, 0);
            }
            1
        }
        WM_DESTROY => {
            popup::close();
            let nid = tray_data();
            Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            0
        }
        _ if msg == a.taskbar_msg && msg != 0 => {
            tray_add(); // Explorer restarted
            a.badge = -1;
            check_reminders(false);
            0
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

// ---------- startup ----------

unsafe fn register(class: &str, proc_: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT, hinst: HINSTANCE, icon: HICON) {
    let name = w(class);
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(proc_);
    wc.hInstance = hinst;
    wc.hIcon = icon;
    wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
    wc.hbrBackground = GetStockObject(BLACK_BRUSH) as HBRUSH;
    wc.lpszClassName = name.as_ptr();
    RegisterClassW(&wc);
}

fn run_loop() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |a: &str| args.iter().any(|x| x == a);
    if has("--uninstall") {
        system::uninstall();
        return;
    }
    if has("--scan") {
        scan::headless(); // weekly auto-scan: no window, own mutex
        return;
    }
    if has("--icons") {
        icons_helper(); // icon fetch: no window, own mutex
        return;
    }
    if has("--scan-dump") {
        scan::dump(&args);
        return;
    }
    if has("--mail-test-connect") {
        scan::test_connect(&args);
        return;
    }
    unsafe {
        let window_mode = has("--window");
        let tray_host = FindWindowW(w("SubscribityHost").as_ptr(), null());
        let mutex = if window_mode { "Local\\SubscribityWindowProcess" } else { "Local\\SubscribityTrayApp" };
        CreateMutexW(null(), 1, w(mutex).as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if window_mode {
                let h = find_window();
                if !h.is_null() {
                    ShowWindow(h, SW_RESTORE);
                    SetForegroundWindow(h);
                }
            } else if !tray_host.is_null() {
                PostMessageW(tray_host, WM_OPEN, 0, 0);
            }
            return;
        }
        SetProcessDpiAwarenessContext(-4isize as _ /* per-monitor v2 */);
        let hinst = GetModuleHandleW(null()) as HINSTANCE;
        let store = Store::load(data_dir());
        let first_run = store.first_run;

        APP = Box::into_raw(Box::new(App {
            hinst,
            host: null_mut(),
            tray: tray_host,
            window_mode,
            win: null_mut(),
            webview: None,
            pending: Vec::new(),
            page_ready: false,
            store,
            icon: null_mut(),
            badge: -1,
            taskbar_msg: RegisterWindowMessageW(w("TaskbarCreated").as_ptr()),
            rates_busy: false,
            rates_try: (0, -1),
            update_busy: false,
            today: today(),
            scale: GetDpiForSystem() as f32 / 96.0,
            scan: scan::Runtime::default(),
            scan_new_seen: 0,
            scan_err_seen: false,
            scan_try: 0,
            notice_page: 0,
        }));
        let a = app();
        let exe_icon = LoadIconW(hinst, 1 as _);

        if window_mode {
            register("SubscribityWindow", win_proc, hinst, exe_icon);
            let code = if has("--add") {
                2
            } else if has("--found") {
                3
            } else if has("--settings") {
                1
            } else {
                args.iter().find_map(|x| x.strip_prefix("--edit=")).and_then(|id| id.parse::<usize>().ok()).map_or(0, |id| 1000 + id)
            };
            create_window(code);
            run_loop();
            return;
        }

        a.icon = make_icon(0);
        a.badge = 0;
        a.scan_new_seen = a.store.settings.scan_new;
        a.scan_err_seen = !a.store.settings.scan_error.is_empty();
        register("SubscribityHost", host_proc, hinst, exe_icon);
        a.host = CreateWindowExW(0, w("SubscribityHost").as_ptr(), w(APP_NAME).as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), hinst, null());
        a.tray = a.host;
        tray_add();
        update::cleanup_old(&system::exe());
        if first_run {
            a.store.edit_settings(|_| {});
            system::set_autostart(true);
        }
        system::ensure_integration();
        if has("--updated") {
            notify(&format!("{} updated", APP_NAME), &format!("You now have version {}.", VERSION));
        }
        housekeeping();
        check_reminders(true);
        maybe_fetch_rates(false);
        maybe_check_updates(false);
        maybe_auto_scan();
        SetTimer(a.host, TIMER_TICK, 10 * 60 * 1000, None);
        if first_run || has("--open") {
            open_window(0);
        }
        trim_memory();
        run_loop();
    }
}
