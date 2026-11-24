//! Window side of the data-folder setting: `state.data_dir`, the `data_dir_*` IPC commands,
//! the move worker and its WM_MOVED result. Storage logic: `datadir.rs`.
use super::{get, inspect, move_to, same_path, Env, Mode, Status};
use crate::{app, send_js, state_json, system, w};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::ptr::null;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};

const SCAN_BUSY: &str = "Wait for the email scan to finish, then try again";
const WAIT: &str = "Wait a moment, then try again";
pub const MOVED: &str = "Data folder changed";

/// `state.data_dir`.
pub fn state() -> Value {
    let cur = get();
    let def = Env::current().root();
    json!({
        "path": cur.to_string_lossy(),
        "is_default": same_path(&cur, &def),
        "default_path": def.to_string_lossy(),
        "busy": app().moving,
    })
}

fn toast(text: &str) {
    send_js(&json!({"type": "toast", "text": text}));
}

fn picked(path: &Path) {
    let t = inspect(&get(), path);
    send_js(&json!({
        "type": "data_dir_picked", "path": path.to_string_lossy(), "status": t.status.as_str(),
        "subs": t.subs, "modified": t.modified, "text": t.text,
    }));
}

/// The `data_dir_*` commands; false when `cmd` isn't one of them.
pub fn handle_ipc(cmd: &str, msg: &Value) -> bool {
    match cmd {
        "data_dir_pick" => {
            if let Some(p) = system::pick_folder(app().win, &get()) {
                picked(&p);
            }
        }
        "data_dir_default" => {
            let root = Env::current().root();
            let _ = std::fs::create_dir_all(&root);
            picked(&root);
        }
        "data_dir_move" => {
            let path = PathBuf::from(msg.get("path").and_then(|p| p.as_str()).unwrap_or("").trim());
            let mode = match msg.get("mode").and_then(|m| m.as_str()).unwrap_or("") {
                "copy" => Mode::Copy,
                "use" => Mode::Use,
                "replace" => Mode::Replace,
                _ => return true,
            };
            start_move(path, mode);
        }
        _ => return false,
    }
    true
}

/// Checks, then stops the tray and moves on a worker thread; the window drops every
/// data-changing command and ignores Close until WM_MOVED arrives.
fn start_move(to: PathBuf, mode: Mode) {
    let a = app();
    if a.moving {
        return;
    }
    if crate::scan::busy() {
        return toast(SCAN_BUSY);
    }
    if a.update_busy || a.rates_busy {
        return toast(WAIT);
    }
    let from = get();
    let t = inspect(&from, &to);
    if matches!(t.status, Status::Same | Status::Bad) {
        return toast(t.text);
    }
    if mode == Mode::Use && t.status == Status::Empty {
        return toast(super::NO_DATA);
    }
    a.moving = true;
    send_js(&state_json());
    let host = a.host as isize;
    let today = crate::today();
    std::thread::spawn(move || {
        let r = run_move(&from, &to, mode, today).map(|_| to);
        unsafe { PostMessageW(host as HWND, crate::WM_MOVED, 0, Box::into_raw(Box::new(r)) as isize) };
    });
}

/// Err((text, a tray was stopped and must be started again)).
fn run_move(from: &Path, to: &Path, mode: Mode, today: i32) -> Result<(), (String, bool)> {
    // Held for the whole move: no headless scan writes scan.tsv meanwhile.
    let Some(_lock) = crate::scan::ScanLock::take() else { return Err((SCAN_BUSY.into(), false)) };
    let had_tray = unsafe { !FindWindowW(w("SubscribityHost").as_ptr(), null()).is_null() };
    system::close_others(false, true);
    move_to(from, to, mode, &Env::current().root(), today).map_err(|e| (e, had_tray))
}

/// WM_MOVED. Done: the window closes and a tray on the new folder opens Settings with
/// "Data folder changed" (`--moved`). Failed: the tray comes back and the page shows why.
pub fn on_moved(lp: isize) {
    let a = app();
    let r = unsafe { Box::from_raw(lp as *mut Result<PathBuf, (String, bool)>) };
    let exe = system::exe().to_string_lossy().into_owned();
    match *r {
        Ok(dir) => {
            a.store = crate::data::Store::load(dir); // the window size is saved in the new folder
            crate::close_window();
            crate::shell_open(&exe, Some("--moved"));
        }
        Err((e, restart)) => {
            a.moving = false;
            if restart {
                crate::shell_open(&exe, None);
                for _ in 0..50 {
                    let h = unsafe { FindWindowW(w("SubscribityHost").as_ptr(), null()) };
                    if !h.is_null() {
                        a.tray = h;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            toast(&e);
            send_js(&state_json());
        }
    }
}
