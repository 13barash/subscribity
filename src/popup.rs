// Compact native popup shown above the tray icon: monthly total, budget, upcoming renewals
// and quick actions. Drawn with GDI in the tray process, so it opens instantly and costs
// almost no memory; it is destroyed as soon as it loses focus.
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::w;

pub struct Item {
    pub id: u32,
    pub name: String,
    pub when: String,
    pub price: String,
    pub tone: u8, // 0 normal, 1 soon, 2 today, 3 trial
    pub icon: String, // icons.rs cache key: App Store artwork URL, else domain; "" = letter tile
    pub flag: bool, // reminder day (active, not flagged, due within the reminder window): show "Don't use it anymore"
    pub unused: bool, // already marked unused: show "Unused"
}

pub struct Data {
    pub total: String,
    pub caption: String,
    pub budget: Option<(f32, String)>, // used/budget ratio, label
    pub items: Vec<Item>,
    pub found: u32, // subscriptions found by the email scan, not reviewed yet
}

pub enum Action {
    Open,
    Add,
    Edit(u32),
    Review,
    Unused(u32), // "Don't use it anymore" on a reminder-day row (subscription id)
}

struct State {
    hwnd: HWND,
    data: Data,
    hover: i32,
    targets: Vec<(RECT, i32)>, // hit areas; i32: -1 open, -2 add, -3 review, -4 close, 0..99 item index, 100+i "Don't use it anymore" on item i
    fonts: [HFONT; 7], // 5, 6: Arial for ₪ (Segoe UI hints it into a box below ~18 px)
    scale: f32,
    closing: bool, // DestroyWindow already called (it re-enters via WM_ACTIVATE)
    logo: HICON, // the exe's own icon (resource 1), drawn in the header; null if missing
    bmps: Vec<HBITMAP>, // cached icon per item (null: letter tile), freed on destroy
}

/// Loads the cached icon of each row (a missing file gives null: letter tile).
fn load_icons(items: &[Item]) -> Vec<HBITMAP> {
    let dir = crate::data_dir();
    items.iter().map(|it| if it.icon.is_empty() { null_mut() } else { crate::icons::load(&crate::icons::path(&dir, &it.icon)) }).collect()
}

unsafe fn free_icons(st: &mut State) {
    for b in st.bmps.drain(..).filter(|b| !b.is_null()) {
        DeleteObject(b);
    }
}

/// New icons were downloaded (WM_ICONS): reload them and repaint the open popup.
pub fn reload_icons() {
    unsafe {
        if let Some(st) = STATE.as_mut().filter(|s| !s.closing) {
            free_icons(st);
            st.bmps = load_icons(&st.data.items);
            InvalidateRect(st.hwnd, null(), 0);
        }
    }
}

/// Destroys the popup once: destroying the active window deactivates it, and the
/// WM_ACTIVATE handler must not start a second, nested DestroyWindow.
unsafe fn destroy(st: &mut State) {
    if !st.closing {
        st.closing = true;
        DestroyWindow(st.hwnd);
    }
}

static mut STATE: Option<State> = None;
static mut CLOSED_AT: u32 = 0;
static mut ON_ACTION: Option<fn(Action)> = None;

const W_DIP: i32 = 340;
// Colors are 0x00BBGGRR (GDI COLORREF), same family as ui.html.
const BG: u32 = 0x0016110E; // #0E1116
const CARD: u32 = 0x00221B16; // #161B22
const LINE: u32 = 0x002B2624; // rgba(255,255,255,.09) over BG = #24262B
const HOVER: u32 = 0x0028211C; // #1C2128
const TEXT: u32 = 0x00F1ECE8; // #E8ECF1
const MUTED: u32 = 0x00B2A49A; // #9AA4B2
const ACCENT: u32 = 0x00C9A67F; // #7FA6C9
const ACCENT_HOVER: u32 = 0x00D3B593; // #93B5D3
const PRIMARY: u32 = ACCENT; // flat fill of the main button (BG-colored text), no gradient
const RED: u32 = 0x007A70D9; // #D9707A
const ORANGE: u32 = 0x005BA5D9; // #D9A55B
const PILL_HOVER: u32 = 0x0038312E; // #2E3138
// Same list as ui.html PALETTE (#58779A ... #4F6F8F).
const PALETTE: [u32; 10] = [0x009A7758, 0x0094774F, 0x007D7B4F, 0x00637B55, 0x004C786E, 0x004A7182, 0x004C6898, 0x006E63A0, 0x008A746A, 0x008F6F4F];

fn color_for(name: &str) -> u32 {
    // Same hash as the window UI, so avatars match.
    let mut h: u32 = 0;
    for c in name.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(c as u32);
    }
    PALETTE[(h % PALETTE.len() as u32) as usize]
}

pub fn is_open() -> bool {
    unsafe { STATE.is_some() }
}

pub fn close() {
    unsafe {
        if let Some(st) = STATE.as_mut() {
            destroy(st);
        }
    }
}

/// Opens the popup, or closes it if it is open. A click on the tray icon while the popup is
/// open first deactivates (closes) it, so a click right after a close is ignored.
pub fn toggle(hinst: HINSTANCE, scale: f32, anchor: (HWND, u32), data: impl FnOnce() -> Data, on_action: fn(Action)) {
    unsafe {
        if is_open() {
            close();
            return;
        }
        if GetTickCount().wrapping_sub(CLOSED_AT) < 300 {
            return;
        }
        ON_ACTION = Some(on_action);
        show(hinst, scale, anchor, data());
    }
}

unsafe fn font(px: f32, weight: i32) -> HFONT {
    face(px, weight, "Segoe UI")
}

unsafe fn face(px: f32, weight: i32, name: &str) -> HFONT {
    CreateFontW(-(px.round() as i32), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, CLEARTYPE_QUALITY as u32, 0, w(name).as_ptr())
}

fn layout_height(d: &Data) -> i32 {
    let mut y = 78;
    if d.budget.is_some() {
        y += 34;
    }
    if d.found > 0 {
        y += 40;
    }
    y += 28 + 46 * d.items.len().max(1) as i32;
    y + 1 + 12 + 34 + 14
}

unsafe fn show(hinst: HINSTANCE, scale: f32, anchor: (HWND, u32), data: Data) {
    static mut REGISTERED: bool = false;
    if !REGISTERED {
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(proc_);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        let name = w("SubscribityPopup");
        wc.lpszClassName = name.as_ptr();
        wc.style = 0x0002_0000; // CS_DROPSHADOW
        RegisterClassW(&wc);
        REGISTERED = true;
    }
    // Place next to the tray icon, inside the work area of its monitor.
    let mut icon: RECT = std::mem::zeroed();
    let mut id: NOTIFYICONIDENTIFIER = std::mem::zeroed();
    id.cbSize = std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32;
    id.hWnd = anchor.0;
    id.uID = anchor.1;
    if Shell_NotifyIconGetRect(&id, &mut icon) != 0 {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        icon = RECT { left: pt.x, top: pt.y, right: pt.x, bottom: pt.y };
    }
    let mon = MonitorFromRect(&icon, MONITOR_DEFAULTTONEAREST);
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(mon, &mut mi);
    let wa = mi.rcWork;
    // The process is per-monitor DPI aware: size for the monitor the popup opens on.
    let (mut dx, mut dy) = (0u32, 0u32);
    let scale = if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) == 0 && dx > 0 { dx as f32 / 96.0 } else { scale };
    let s = |v: i32| (v as f32 * scale).round() as i32;
    let (pw, ph) = (s(W_DIP), s(layout_height(&data)));
    let gap = s(12);
    let cx = (icon.left + icon.right) / 2;
    let mut x = cx - pw / 2;
    let mut y = if icon.top >= wa.bottom - gap { wa.bottom - ph - gap } else if icon.bottom <= wa.top + gap { wa.top + gap } else { icon.top - ph - gap };
    if icon.left >= wa.right - gap {
        x = wa.right - pw - gap; // taskbar on the right
        y = (icon.top + icon.bottom) / 2 - ph / 2;
    } else if icon.right <= wa.left + gap {
        x = wa.left + gap; // taskbar on the left
        y = (icon.top + icon.bottom) / 2 - ph / 2;
    }
    // min then max, not clamp(): clamp panics when the popup is larger than the work area.
    x = x.min(wa.right - pw - gap).max(wa.left + gap);
    y = y.min(wa.bottom - ph - gap).max(wa.top + gap);

    let hwnd = CreateWindowExW(0x80 | 0x8 /* toolwindow, topmost */, w("SubscribityPopup").as_ptr(), w("Subscribity").as_ptr(), WS_POPUP, x, y, pw, ph, null_mut(), null_mut(), hinst, null());
    if hwnd.is_null() {
        return;
    }
    crate::dwm_set(hwnd, 33, 2); // rounded corners (Windows 11)
    crate::dwm_set(hwnd, 34, LINE); // border color
    crate::dwm_set(hwnd, 20, 1); // dark
    STATE = Some(State {
        hwnd,
        bmps: load_icons(&data.items),
        logo: LoadImageW(GetModuleHandleW(null()), 1 as *const u16, IMAGE_ICON, (20.0 * scale).round() as i32, (20.0 * scale).round() as i32, 0) as HICON,
        data,
        hover: i32::MIN,
        closing: false,
        targets: Vec::new(),
        fonts: [font(12.5 * scale, 400), font(26.0 * scale, 700), font(14.0 * scale, 700), font(12.0 * scale, 400), font(13.0 * scale, 700), face(14.0 * scale, 700, "Arial"), face(12.5 * scale, 400, "Arial")],
        scale,
    });
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
}

unsafe fn fill(dc: HDC, r: RECT, color: u32, radius: i32) {
    let b = CreateSolidBrush(color);
    let ob = SelectObject(dc, b);
    let op = SelectObject(dc, GetStockObject(NULL_PEN));
    if radius > 0 {
        RoundRect(dc, r.left, r.top, r.right + 1, r.bottom + 1, radius, radius);
    } else {
        FillRect(dc, &r, b);
    }
    SelectObject(dc, op);
    SelectObject(dc, ob);
    DeleteObject(b);
}

unsafe fn text(dc: HDC, t: &str, mut r: RECT, f: HFONT, color: u32, flags: u32) {
    let of = SelectObject(dc, f);
    SetTextColor(dc, color);
    let mut v: Vec<u16> = t.encode_utf16().collect();
    DrawTextW(dc, v.as_mut_ptr(), v.len() as i32, &mut r, flags | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
    SelectObject(dc, of);
}

/// Like `text`, but draws each ₪ with `fsym`: Segoe UI's hinted ₪ turns into a box at small sizes.
/// Single line, DT_LEFT or DT_RIGHT, no ellipsis.
unsafe fn text_sym(dc: HDC, t: &str, r: RECT, f: HFONT, fsym: HFONT, color: u32, flags: u32) {
    if !t.contains('₪') {
        return text(dc, t, r, f, color, flags);
    }
    let mut runs: Vec<(Vec<u16>, HFONT, i32)> = Vec::new();
    for (i, part) in t.split('₪').enumerate() {
        if i > 0 {
            runs.push(("₪".encode_utf16().collect(), fsym, 0));
        }
        if !part.is_empty() {
            runs.push((part.encode_utf16().collect(), f, 0));
        }
    }
    let of = SelectObject(dc, f);
    let mut total = 0;
    for run in runs.iter_mut() {
        SelectObject(dc, run.1);
        let mut sz = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(dc, run.0.as_ptr(), run.0.len() as i32, &mut sz);
        run.2 = sz.cx;
        total += sz.cx;
    }
    SetTextColor(dc, color);
    let mut x = if flags & DT_RIGHT != 0 { (r.right - total).max(r.left) } else { r.left };
    for (v, fr, wd) in runs.iter_mut() {
        SelectObject(dc, *fr);
        let mut rr = RECT { left: x, top: r.top, right: (x + *wd).min(r.right), bottom: r.bottom };
        DrawTextW(dc, v.as_mut_ptr(), v.len() as i32, &mut rr, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        x += *wd;
    }
    SelectObject(dc, of);
}

unsafe fn paint(st: &mut State, dc: HDC, cw: i32, ch: i32) {
    let s = |v: i32| (v as f32 * st.scale).round() as i32;
    let rc = |x: i32, y: i32, w: i32, h: i32| RECT { left: s(x), top: s(y), right: s(x + w), bottom: s(y + h) };
    let [f_small, f_big, f_name, f_sub, f_btn, f_name_sym, f_small_sym] = st.fonts;
    fill(dc, RECT { left: 0, top: 0, right: cw, bottom: ch }, BG, 0);
    SetBkMode(dc, TRANSPARENT as _);
    st.targets.clear();
    let d = &st.data;

    // Hero block (total + budget) on a card. Radii below are true radii: RoundRect takes diameters.
    fill(dc, rc(8, 6, W_DIP - 16, if d.budget.is_some() { 104 } else { 70 }), CARD, s(32));
    if !st.logo.is_null() {
        DrawIconEx(dc, s(18), s(12), st.logo, s(20), s(20), 0, null_mut(), DI_NORMAL);
    }
    text(dc, "Per month", rc(44, 14, 174, 18), f_small, MUTED, DT_LEFT);
    text(dc, &d.caption, rc(100, 14, 182, 18), f_small, MUTED, DT_RIGHT);
    // Close button, top right (the popup also closes on Esc or a click outside).
    let x_btn = rc(290, 8, 32, 32);
    fill(dc, x_btn, if st.hover == -4 { HOVER } else { LINE }, s(24));
    text(dc, "×", x_btn, f_big, if st.hover == -4 { TEXT } else { MUTED }, DT_CENTER);
    st.targets.push((x_btn, -4));
    text(dc, &d.total, rc(18, 34, 304, 36), f_big, TEXT, DT_LEFT);
    let mut y = 78;
    if let Some((ratio, label)) = &d.budget {
        fill(dc, rc(18, y + 2, 304, 6), LINE, s(6));
        let fw = (304.0 * ratio.clamp(0.0, 1.0)).max(6.0) as i32;
        fill(dc, rc(18, y + 2, fw, 6), if *ratio > 1.0 { RED } else { ACCENT }, s(6));
        text_sym(dc, label, rc(18, y + 12, 304, 18), f_small, f_small_sym, if *ratio > 1.0 { RED } else { MUTED }, DT_LEFT);
        y += 34;
    }
    if d.found > 0 {
        let row = rc(8, y + 3, W_DIP - 16, 34);
        fill(dc, row, if st.hover == -3 { HOVER } else { CARD }, s(24));
        let n = if d.found == 1 { "1 new subscription found".to_string() } else { format!("{} new subscriptions found", d.found) };
        text(dc, &n, rc(18, y + 3, 230, 34), f_name, TEXT, DT_LEFT);
        text(dc, "Review", rc(246, y + 3, 76, 34), f_name, if st.hover == -3 { ACCENT_HOVER } else { ACCENT }, DT_RIGHT);
        st.targets.push((row, -3));
        y += 40;
    }
    fill(dc, rc(0, y, W_DIP, 1), LINE, 0);
    text(dc, "Upcoming", rc(18, y + 6, 200, 20), f_small, MUTED, DT_LEFT);
    y += 28;
    if d.items.is_empty() {
        text(dc, "Nothing renewing in the next 30 days", rc(18, y, 304, 46), f_sub, MUTED, DT_LEFT);
        y += 46;
    }
    for (i, it) in d.items.iter().enumerate() {
        let row = rc(8, y, W_DIP - 16, 46);
        let pill = rc(196, y + 23, 126, 20);
        let pill_on = it.flag && !it.unused;
        // The pill is pushed before the row: hit() takes the first match.
        if pill_on {
            st.targets.push((pill, 100 + i as i32));
        }
        if st.hover == i as i32 {
            fill(dc, row, HOVER, s(24));
        }
        st.targets.push((row, i as i32));
        let av = color_for(&it.name);
        let a = rc(18, y + 8, 30, 30);
        let bmp = st.bmps.get(i).copied().unwrap_or(null_mut());
        if !bmp.is_null() {
            crate::icons::draw(dc, a, bmp, av, s(20), s(3));
        } else {
            let b = CreateSolidBrush(av);
            let ob = SelectObject(dc, b);
            let op = SelectObject(dc, GetStockObject(NULL_PEN));
            RoundRect(dc, a.left, a.top, a.right + 1, a.bottom + 1, s(20), s(20));
            SelectObject(dc, op);
            SelectObject(dc, ob);
            DeleteObject(b);
            let letter: String = it.name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into());
            text(dc, &letter, a, f_name, 0x00FFFFFF, DT_CENTER);
        }
        text(dc, &it.name, rc(58, y + 5, 170, 20), f_name, TEXT, DT_LEFT);
        let tone = match it.tone { 1 => ORANGE, 2 => RED, 3 => ACCENT, _ => MUTED };
        text(dc, &it.when, rc(58, y + 24, if it.flag || it.unused { 134 } else { 170 }, 18), f_sub, tone, DT_LEFT);
        if pill_on {
            let hot = st.hover == 100 + i as i32;
            fill(dc, pill, if hot { PILL_HOVER } else { LINE }, s(24));
            text(dc, "Don't use it anymore", pill, f_sub, if hot { ACCENT_HOVER } else { ACCENT }, DT_CENTER);
        } else if it.unused {
            text(dc, "Unused", pill, f_sub, ORANGE, DT_RIGHT);
        }
        text_sym(dc, &it.price, rc(226, y + 5, 96, 20), f_name, f_name_sym, TEXT, DT_RIGHT);
        y += 46;
    }
    fill(dc, rc(0, y, W_DIP, 1), LINE, 0);
    y += 13;
    let open = rc(18, y, 196, 34);
    let add = rc(222, y, 100, 34);
    fill(dc, open, if st.hover == -1 { ACCENT } else { PRIMARY }, s(24));
    fill(dc, add, if st.hover == -2 { HOVER } else { LINE }, s(24));
    text(dc, "Open Subscribity", open, f_btn, BG, DT_CENTER);
    text(dc, "+ Add", add, f_btn, TEXT, DT_CENTER);
    st.targets.push((open, -1));
    st.targets.push((add, -2));
}

fn hit(st: &State, x: i32, y: i32) -> i32 {
    st.targets
        .iter()
        .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
        .map(|(_, t)| *t)
        .unwrap_or(i32::MIN)
}

unsafe extern "system" fn proc_(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(st) = STATE.as_mut().filter(|s| s.hwnd == h) else {
        return DefWindowProcW(h, msg, wp, lp);
    };
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(h, &mut ps);
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(h, &mut r);
            let mem = CreateCompatibleDC(dc);
            let bmp = CreateCompatibleBitmap(dc, r.right, r.bottom);
            let old = SelectObject(mem, bmp);
            paint(st, mem, r.right, r.bottom);
            BitBlt(dc, 0, 0, r.right, r.bottom, mem, 0, 0, SRCCOPY);
            SelectObject(mem, old);
            DeleteObject(bmp);
            DeleteDC(mem);
            EndPaint(h, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEMOVE => {
            let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
            let t = hit(st, x, y);
            if t != st.hover {
                st.hover = t;
                InvalidateRect(h, null(), 0);
            }
            let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
            0
        }
        0x02A3 /* WM_MOUSELEAVE */ => {
            st.hover = i32::MIN;
            InvalidateRect(h, null(), 0);
            0
        }
        WM_SETCURSOR => {
            SetCursor(LoadCursorW(null_mut(), if st.hover != i32::MIN { IDC_HAND } else { IDC_ARROW }));
            1
        }
        WM_LBUTTONUP => {
            let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
            let action = match hit(st, x, y) {
                -1 => Some(Action::Open),
                -2 => Some(Action::Add),
                -3 => Some(Action::Review),
                -4 => {
                    destroy(st);
                    None
                }
                i if i >= 100 => st.data.items.get(i as usize - 100).map(|it| Action::Unused(it.id)),
                i if i >= 0 => st.data.items.get(i as usize).map(|it| Action::Edit(it.id)),
                _ => None,
            };
            if let Some(a) = action {
                destroy(st);
                if let Some(f) = ON_ACTION {
                    f(a);
                }
            }
            0
        }
        WM_KEYDOWN if wp == 0x1B => {
            destroy(st);
            0
        }
        WM_ACTIVATE if (wp & 0xFFFF) == 0 => {
            destroy(st);
            0
        }
        WM_DESTROY => {
            for f in st.fonts {
                DeleteObject(f);
            }
            free_icons(st);
            if !st.logo.is_null() {
                DestroyIcon(st.logo);
            }
            STATE = None;
            CLOSED_AT = GetTickCount();
            crate::trim_memory();
            0
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}
