// Subscribity: a tiny Windows tray app for tracking subscriptions.
//
// The tray part (icon, badge, reminders, toasts, exchange rates, autostart) is plain Win32
// and stays resident using ~1-3 MB of RAM. The window UI is HTML rendered by the WebView2
// runtime that ships with Windows; it is created when the window opens and fully destroyed
// when it closes, so it costs nothing while hidden.
#![windows_subsystem = "windows"]
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

mod data;
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
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::SystemInformation::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const APP_NAME: &str = "Subscribity";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const UI_HTML: &str = include_str!("ui.html");

const WM_TRAY: u32 = 0x8000 + 1;
const WM_RATES: u32 = 0x8000 + 2;
const WM_OPEN: u32 = 0x8000 + 3;
const WM_IPC: u32 = 0x8000 + 4;
const WM_RELOAD: u32 = 0x8000 + 5; // window process -> tray: data changed on disk
const WM_TESTNOTIFY: u32 = 0x8000 + 6; // window process -> tray: show a test toast
const WM_OPENPAGE: u32 = 0x8000 + 7; // tray -> window process: 1 = settings, 2 = add
const WM_UPDATE: u32 = 0x8000 + 8; // update check finished
const WM_INSTALLED: u32 = 0x8000 + 9; // update install finished
const TIMER_TICK: usize = 1;

const M_OPEN: usize = 1;
const M_ADD: usize = 2;
const M_SETTINGS: usize = 3;
const M_AUTOSTART: usize = 4;
const M_EXIT: usize = 5;

// ---------- app state (single UI thread) ----------

struct App {
    hinst: HINSTANCE,
    host: HWND, // window that receives background results (tray host, or the window itself)
    tray: HWND, // tray host window (in the window process: the running tray's)
    window_mode: bool,
    win: HWND,
    webview: Option<wry::WebView>,
    pending: Vec<String>, // messages queued until the page is ready
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
}

static mut APP: *mut App = null_mut();
fn app() -> &'static mut App {
    unsafe { &mut *APP }
}

fn w(s: &str) -> Vec<u16> {
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

fn trim_memory() {
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

fn data_dir() -> std::path::PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_default()
}

fn shell_open(target: &str) {
    unsafe {
        ShellExecuteW(null_mut(), w("open").as_ptr(), w(target).as_ptr(), null(), null(), SW_SHOWNORMAL);
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
    let f = CreateFontW(-height, 0, 0, 0, 700, 0, 0, 0, 0, 0, 0, ANTIALIASED_QUALITY as u32, 0, w("Segoe UI").as_ptr());
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
        unsafe { PostMessageW(app().tray, WM_TESTNOTIFY, 0, 0) };
        return;
    }
    let mut nid = tray_data();
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    copy_w(&mut nid.szInfoTitle, title);
    copy_w(&mut nid.szInfo, body);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
}

// ---------- reminders ----------

struct Due {
    key: String,
    text: String,
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
            (left <= s.remind.max(0)).then(|| (d, Due {
                key: format!("{}|{}", s.id, d),
                text: format!("{} {} – {}", s.name, money(s.price, &s.cur), due_text(left).to_lowercase()),
            }))
        })
        .collect();
    v.sort_by_key(|x| x.0);
    v.into_iter().map(|x| x.1).collect()
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
            let title = if fresh.len() == 1 { "Subscription renewing soon".to_string() } else { format!("{} subscriptions renewing soon", fresh.len()) };
            let mut body: Vec<String> = fresh.iter().take(4).map(|d| d.text.clone()).collect();
            if fresh.len() > 4 {
                body.push(format!("+{} more", fresh.len() - 4));
            }
            notify(&title, &body.join("\n"));
        }
    }
    if !find_window().is_null() {
        // The window is open, so the user has seen everything.
        a.store.ack.extend(keys.iter().cloned());
    }
    a.store.save_state();
    let unseen = due.iter().filter(|d| !a.store.ack.contains(&d.key)).count() as i32;
    let mut tip = if due.is_empty() {
        format!("{} – nothing renewing soon", APP_NAME)
    } else {
        let mut t = format!("{} – {} renewing soon", APP_NAME, due.len());
        for d in due.iter().take(3) {
            t.push('\n');
            t.push_str(&d.text);
        }
        t
    };
    if tip.encode_utf16().count() > 127 {
        tip = tip.chars().take(120).collect::<String>() + "…";
    }
    set_badge(unseen, &tip);
}

// ---------- exchange rates (background thread, WinHTTP) ----------

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
        let boxed = Box::into_raw(Box::new(result));
        unsafe { PostMessageW(host as HWND, WM_RATES, 0, boxed as isize) };
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
        format!("Updated {} · refreshed daily from open.er-api.com", fmt_date(st.rates_date))
    } else {
        "Not downloaded yet · needed only for subscriptions in other currencies".into()
    }
}

// ---------- autostart ----------

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

fn autostart_enabled() -> bool {
    unsafe {
        let mut size = 0u32;
        RegGetValueW(HKEY_CURRENT_USER, w(RUN_KEY).as_ptr(), w(APP_NAME).as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut size) == 0
    }
}

fn set_autostart(on: bool) {
    unsafe {
        if on {
            let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let v = w(&format!("\"{}\"", exe));
            RegSetKeyValueW(HKEY_CURRENT_USER, w(RUN_KEY).as_ptr(), w(APP_NAME).as_ptr(), REG_SZ, v.as_ptr() as *const c_void, (v.len() * 2) as u32);
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, w(RUN_KEY).as_ptr(), w(APP_NAME).as_ptr());
        }
    }
}

// ---------- window + WebView2 ----------

struct Win(HWND);
impl HasWindowHandle for Win {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let h = Win32WindowHandle::new(NonZeroIsize::new(self.0 as isize).ok_or(HandleError::Unavailable)?);
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(h)) })
    }
}

fn is_dark() -> bool {
    match app().store.settings.theme.as_str() {
        "dark" => true,
        "light" => false,
        _ => unsafe {
            // AppsUseLightTheme = 0 means dark mode.
            let mut v = 1u32;
            let mut size = 4u32;
            RegGetValueW(
                HKEY_CURRENT_USER,
                w("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize").as_ptr(),
                w("AppsUseLightTheme").as_ptr(),
                RRF_RT_REG_DWORD, null_mut(), &mut v as *mut u32 as *mut c_void, &mut size,
            );
            v == 0
        },
    }
}

fn set_title_bar_theme(h: HWND) {
    if h.is_null() {
        return;
    }
    // DWMWA_USE_IMMERSIVE_DARK_MODE = 20; loaded lazily so dwmapi is not a startup import.
    unsafe {
        let lib = LoadLibraryW(w("dwmapi.dll").as_ptr());
        if let Some(f) = GetProcAddress(lib, b"DwmSetWindowAttribute\0".as_ptr()) {
            let set: unsafe extern "system" fn(HWND, u32, *const c_void, u32) -> i32 = std::mem::transmute(f);
            let dark: i32 = is_dark() as i32;
            set(h, 20, &dark as *const i32 as *const c_void, 4);
        }
    }
}

fn find_window() -> HWND {
    unsafe { FindWindowW(w("SubscribityWindow").as_ptr(), null()) }
}

/// Tray side: show the window, which lives in its own short-lived process so that
/// closing it returns all WebView2 memory to Windows.
fn open_window(page: Option<&str>, add: bool) {
    let code = if add { 2 } else if page == Some("settings") { 1 } else { 0 };
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
    if let Ok(exe) = std::env::current_exe() {
        let args = match code {
            1 => "--window --settings",
            2 => "--window --add",
            _ => "--window",
        };
        unsafe {
            ShellExecuteW(null_mut(), w("open").as_ptr(), w(&exe.to_string_lossy()).as_ptr(), w(args).as_ptr(), null(), SW_SHOWNORMAL);
        }
    }
}

/// Window process: data changed, let the tray reload it and recheck reminders.
fn after_change() {
    let a = app();
    if a.window_mode {
        unsafe { PostMessageW(a.tray, WM_RELOAD, 0, 0) };
    } else {
        check_reminders(true);
    }
}

fn create_window(open_msg: Option<Value>) {
    let a = app();
    unsafe {
        let mut wa: RECT = std::mem::zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut _ as *mut c_void, 0);
        let ww = ((1040.0 * a.scale) as i32).min(wa.right - wa.left - 24);
        let wh = ((720.0 * a.scale) as i32).min(wa.bottom - wa.top - 24);
        let (x, y) = (wa.right - ww - 12, wa.bottom - wh - 12);
        a.win = CreateWindowExW(0, w("SubscribityWindow").as_ptr(), w(APP_NAME).as_ptr(), WS_OVERLAPPEDWINDOW, x, y, ww, wh, null_mut(), null_mut(), a.hinst, null());
        set_title_bar_theme(a.win);
    }
    a.host = a.win; // background results (rates) come back to this window
    a.page_ready = false;
    a.pending.clear();
    if let Some(m) = open_msg {
        a.pending.push(m.to_string());
    }

    let host = a.host as isize;
    let profile = std::env::var("LOCALAPPDATA")
        .map(|p| std::path::PathBuf::from(p).join("Subscribity").join("WebView2"))
        .unwrap_or_else(|_| data_dir().join("webview"));
    let mut ctx = wry::WebContext::new(Some(profile));
    let bg = if is_dark() { (18, 19, 26, 255) } else { (244, 245, 251, 255) };
    let built = wry::WebViewBuilder::with_web_context(&mut ctx)
        .with_html(UI_HTML)
        .with_background_color(bg)
        .with_devtools(cfg!(debug_assertions))
        .with_ipc_handler(move |req| {
            // Handle on the host window's message loop, outside the WebView2 callback.
            let body = Box::into_raw(Box::new(req.body().clone()));
            unsafe { PostMessageW(host as HWND, WM_IPC, 0, body as isize) };
        })
        .with_new_window_req_handler(|url| {
            if url.starts_with("https://") {
                shell_open(&url);
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
    a.webview = None;
    a.page_ready = false;
    a.pending.clear();
    if !a.win.is_null() {
        let h = a.win;
        a.win = null_mut();
        unsafe { DestroyWindow(h) };
    }
    // The window process ends here; Windows reclaims all WebView2 memory.
    unsafe { PostQuitMessage(0) };
}

fn on_rates(lp: LPARAM) {
    let a = app();
    a.rates_busy = false;
    let res = unsafe { Box::from_raw(lp as *mut Option<(String, HashMap<String, f64>)>) };
    match *res {
        Some((base, rates)) => {
            a.store.rates = rates;
            a.store.settings.rates_base = base;
            a.store.settings.rates_date = today();
            a.store.save_rates();
            a.store.save_settings();
            send_js(&json!({"type": "toast", "text": "Exchange rates updated"}));
            if a.window_mode {
                after_change();
            }
        }
        None => send_js(&json!({"type": "toast", "text": "Could not download exchange rates"})),
    }
    send_js(&state_json());
    trim_memory();
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
                "monthly": monthly(s),
                "monthlyMain": st.to_main(monthly(s), &s.cur),
                "priceMain": st.to_main(s.price, &s.cur),
            })
        })
        .collect();
    json!({
        "type": "state",
        "version": VERSION,
        "today": iso(t),
        "subs": subs,
        "currencies": CURRENCIES,
        "settings": {
            "main": st.settings.main_cur,
            "remind": st.settings.default_remind,
            "theme": st.settings.theme,
            "autostart": autostart_enabled(),
            "ratesStatus": rates_status(),
            "updateFreq": st.settings.update_freq,
            "latest": if update::is_newer(&st.settings.latest_version, VERSION) { st.settings.latest_version.as_str() } else { "" },
            "updateStatus": update_status(),
            "updateBusy": a.update_busy,
        }
    })
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
        format!("You have the latest version ({}) · checked {}", VERSION, fmt_date(st.update_last))
    } else {
        format!("Version {}", VERSION)
    }
}

/// Checks GitHub Releases in the background (weekly or monthly, or now when forced).
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
    match *res {
        Ok(v) => {
            a.store.settings.latest_version = v.clone();
            a.store.settings.update_last = today();
            let newer = update::is_newer(&v, VERSION);
            if newer && !a.window_mode && a.store.settings.update_notified != v {
                a.store.settings.update_notified = v.clone();
                notify(&format!("{} {} is available", APP_NAME, v), "Open Settings in the app to install it with one click.");
            }
            a.store.save_settings();
            send_js(&json!({"type": "toast", "text": if newer { format!("Version {} is available", v) } else { "You have the latest version".to_string() }}));
            if a.window_mode {
                after_change();
            }
        }
        Err(e) => {
            // Don't retry every few minutes while offline; try again at the next period.
            a.store.settings.update_last = today();
            a.store.save_settings();
            send_js(&json!({"type": "toast", "text": format!("Could not check for updates: {}", e)}));
        }
    }
    send_js(&state_json());
}

/// Window process: download, verify and install the latest release, then restart.
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
            .and_then(|(v, exe)| {
                let path = std::env::current_exe().map_err(|e| e.to_string())?;
                update::swap_in(&path, &exe).map(|_| v)
            });
        unsafe { PostMessageW(host as HWND, WM_INSTALLED, 0, Box::into_raw(Box::new(r)) as isize) };
    });
}

fn on_installed(lp: LPARAM) {
    let a = app();
    a.update_busy = false;
    let res = unsafe { Box::from_raw(lp as *mut Result<String, String>) };
    match *res {
        Ok(_) => unsafe {
            // Stop the old tray, start the new version, and close this window.
            if !a.tray.is_null() {
                PostMessageW(a.tray, WM_CLOSE, 0, 0);
                for _ in 0..50 {
                    if FindWindowW(w("SubscribityHost").as_ptr(), null()).is_null() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            if let Ok(exe) = std::env::current_exe() {
                ShellExecuteW(null_mut(), w("open").as_ptr(), w(&exe.to_string_lossy()).as_ptr(), w("--updated --open").as_ptr(), null(), SW_SHOWNORMAL);
            }
            close_window();
        },
        Err(e) => {
            send_js(&json!({"type": "toast", "text": format!("Update failed: {}", e)}));
            send_js(&state_json());
        }
    }
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
            events.push(json!({"day": day, "id": s.id, "name": s.name, "price": s.price, "cur": s.cur, "priceMain": a.store.to_main(s.price, &s.cur)}));
            d = next_due(s, d + 1);
            guard += 1;
        }
    }
    json!({"type": "calendar", "y": y, "m": m, "events": events})
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string()
}

fn handle_ipc(body: &str) {
    let Ok(msg) = serde_json::from_str::<Value>(body) else { return };
    let a = app();
    match msg.get("cmd").and_then(|c| c.as_str()).unwrap_or("") {
        "update_check" => maybe_check_updates(true),
        "update_install" => install_update(),
        "update_freq" => {
            let f = str_of(&msg, "freq");
            if ["weekly", "monthly", "off"].contains(&f.as_str()) {
                a.store.settings.update_freq = f;
                a.store.save_settings();
                after_change();
            }
            send_js(&state_json());
        }
        "open_release" => shell_open(&format!("https://github.com/{}/releases/latest", update::REPO)),
        "ready" => {
            a.page_ready = true;
            eval(&state_json().to_string());
            for p in std::mem::take(&mut a.pending) {
                eval(&p);
            }
            after_change(); // lets the tray clear its badge
        }
        "save" => {
            let s = &msg["sub"];
            let name: String = str_of(s, "name").chars().take(80).collect();
            let price = s.get("price").and_then(|p| p.as_f64()).filter(|p| p.is_finite() && *p >= 0.0);
            let start = parse_iso(&str_of(s, "start"));
            let (Some(price), Some(start), false) = (price, start, name.is_empty()) else {
                send_js(&json!({"type": "error", "text": "Please check the name, price and date."}));
                return;
            };
            let id = s.get("id").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            let cur = str_of(s, "cur");
            let sub = Sub {
                id: if id == 0 { a.store.next_id() } else { id },
                name,
                price,
                cur: if cur.is_empty() { a.store.settings.main_cur.clone() } else { cur },
                every: s.get("every").and_then(|x| x.as_u64()).unwrap_or(1).clamp(1, 999) as u32,
                unit: s.get("unit").and_then(|x| x.as_u64()).unwrap_or(2).min(3) as u8,
                start,
                category: str_of(s, "category").chars().take(40).collect(),
                payment: str_of(s, "payment").chars().take(40).collect(),
                remind: s.get("remind").and_then(|x| x.as_i64()).unwrap_or(3).clamp(0, 365) as i32,
                active: s.get("active").and_then(|x| x.as_bool()).unwrap_or(true),
                notes: s.get("notes").and_then(|x| x.as_str()).unwrap_or("").chars().take(500).collect(),
                url: str_of(s, "url").chars().take(200).collect(),
            };
            let existed = a.store.subs.iter().position(|x| x.id == sub.id);
            match existed {
                Some(i) => a.store.subs[i] = sub,
                None => a.store.subs.push(sub),
            }
            a.store.save_subs();
            send_js(&state_json());
            send_js(&json!({"type": "saved", "text": if existed.is_some() { "Saved" } else { "Subscription added" }}));
            after_change();
            maybe_fetch_rates(false);
        }
        "delete" => {
            let id = msg.get("id").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            a.store.subs.retain(|s| s.id != id);
            a.store.save_subs();
            send_js(&state_json());
            send_js(&json!({"type": "toast", "text": "Deleted"}));
            after_change();
        }
        "settings" => {
            let main = str_of(&msg, "main");
            if main.len() == 3 {
                a.store.settings.main_cur = main;
            }
            a.store.settings.default_remind = msg.get("remind").and_then(|x| x.as_i64()).unwrap_or(3).clamp(0, 365) as i32;
            let theme = str_of(&msg, "theme");
            if ["system", "light", "dark"].contains(&theme.as_str()) {
                a.store.settings.theme = theme;
            }
            a.store.save_settings();
            set_title_bar_theme(a.win);
            maybe_fetch_rates(false);
            send_js(&state_json());
            after_change();
        }
        "autostart" => {
            set_autostart(msg.get("on").and_then(|x| x.as_bool()).unwrap_or(false));
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
        "test_notify" => notify("Subscribity notifications work", "You'll get a reminder like this before a subscription renews."),
        "open_folder" => shell_open(&data_dir().to_string_lossy()),
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
            let path = data_dir().join("subscriptions-export.csv");
            if std::fs::write(&path, csv).is_ok() {
                send_js(&json!({"type": "toast", "text": "Exported subscriptions-export.csv"}));
                shell_open(&data_dir().to_string_lossy());
            }
        }
        _ => {}
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
        WM_OPENPAGE => {
            match wp {
                1 => send_js(&json!({"type": "open", "page": "settings"})),
                2 => send_js(&json!({"type": "open", "page": "subs", "add": true})),
                _ => {}
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

// ---------- hidden host window: tray, timer, rates, IPC ----------

fn tray_menu() {
    unsafe {
        let a = app();
        let m = CreatePopupMenu();
        AppendMenuW(m, 0, M_OPEN, w("Open Subscribity").as_ptr());
        AppendMenuW(m, 0, M_ADD, w("Add subscription…").as_ptr());
        AppendMenuW(m, 0, M_SETTINGS, w("Settings").as_ptr());
        AppendMenuW(m, 0x800, 0, null());
        AppendMenuW(m, if autostart_enabled() { 8 } else { 0 }, M_AUTOSTART, w("Start with Windows").as_ptr());
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
            M_OPEN => open_window(None, false),
            M_ADD => open_window(Some("subs"), true),
            M_SETTINGS => open_window(Some("settings"), false),
            M_AUTOSTART => set_autostart(!autostart_enabled()),
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
                0x202 /* WM_LBUTTONUP */ | 0x405 /* NIN_BALLOONUSERCLICK */ => open_window(None, false),
                0x205 /* WM_RBUTTONUP */ => tray_menu(),
                _ => {}
            }
            0
        }
        WM_OPEN => {
            open_window(None, false);
            0
        }
        WM_RELOAD => {
            // The window process saved changes: reload from disk.
            a.store = Store::load(data_dir());
            check_reminders(true);
            maybe_fetch_rates(false);
            trim_memory();
            0
        }
        WM_TESTNOTIFY => {
            notify("Subscribity notifications work", "You'll get a reminder like this before a subscription renews.");
            0
        }
        WM_TIMER => {
            check_reminders(true);
            maybe_fetch_rates(false);
            maybe_check_updates(false);
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
            let nid = tray_data();
            Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            0
        }
        _ if msg == a.taskbar_msg && msg != 0 => {
            // Explorer restarted: put the icon back.
            tray_add();
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

fn main() {
    unsafe {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let window_mode = args.iter().any(|a| a == "--window");
        let tray_host = FindWindowW(w("SubscribityHost").as_ptr(), null());
        if window_mode {
            // Only one window process.
            CreateMutexW(null(), 1, w("Local\\SubscribityWindowProcess").as_ptr());
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let h = find_window();
                if !h.is_null() {
                    ShowWindow(h, SW_RESTORE);
                    SetForegroundWindow(h);
                }
                return;
            }
        } else {
            // One tray instance only; a second launch opens the window.
            CreateMutexW(null(), 1, w("Local\\SubscribityTrayApp").as_ptr());
            if GetLastError() == ERROR_ALREADY_EXISTS {
                if !tray_host.is_null() {
                    PostMessageW(tray_host, WM_OPEN, 0, 0);
                }
                return;
            }
        }
        SetProcessDpiAwarenessContext(-4isize as _ /* per-monitor v2 */);
        let hinst = GetModuleHandleW(null()) as HINSTANCE;
        let store = Store::load(data_dir());
        let first_run = store.first_run;

        APP = Box::into_raw(Box::new(App {
            hinst,
            tray: tray_host,
            window_mode,
            host: null_mut(),
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
        }));
        let a = app();

        if window_mode {
            register("SubscribityWindow", win_proc, hinst, make_icon(0));
            let open_msg = if args.iter().any(|x| x == "--add") {
                Some(json!({"type": "open", "page": "subs", "add": true}))
            } else if args.iter().any(|x| x == "--settings") {
                Some(json!({"type": "open", "page": "settings"}))
            } else {
                None
            };
            create_window(open_msg);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            return;
        }

        a.icon = make_icon(0);
        a.badge = 0;
        register("SubscribityHost", host_proc, hinst, null_mut());
        a.host =CreateWindowExW(0, w("SubscribityHost").as_ptr(), w(APP_NAME).as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), hinst, null());
        a.tray = a.host;
        tray_add();
        if let Ok(exe) = std::env::current_exe() {
            update::cleanup_old(&exe);
        }
        if args.iter().any(|x| x == "--updated") {
            notify(&format!("{} updated", APP_NAME), &format!("You now have version {}.", VERSION));
        }

        if first_run {
            a.store.save_settings();
            set_autostart(true);
        }
        check_reminders(true);
        maybe_fetch_rates(false);
        SetTimer(a.host, TIMER_TICK, 10 * 60 * 1000, None); // every 10 minutes
        maybe_check_updates(false);
        if first_run || args.iter().any(|x| x == "--open") {
            open_window(None, false);
        }
        trim_memory();

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
