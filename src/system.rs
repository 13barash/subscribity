// Windows integration: start with Windows, Start menu shortcut, the "Installed apps" entry
// and uninstalling. Everything is per-user (HKCU / %APPDATA%), so no admin rights are needed.
use std::ffi::c_void;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::{OpenMutexW, ReleaseMutex, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::{data, w, APP_NAME, VERSION};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const UNINSTALL_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Subscribity";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}

fn reg_get(key: &str, name: &str) -> Option<String> {
    unsafe {
        let mut size = 0u32;
        if RegGetValueW(HKEY_CURRENT_USER, w(key).as_ptr(), w(name).as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut size) != 0 {
            return None;
        }
        let mut buf = vec![0u16; size as usize / 2 + 1];
        if RegGetValueW(HKEY_CURRENT_USER, w(key).as_ptr(), w(name).as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr() as *mut c_void, &mut size) != 0 {
            return None;
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

fn reg_set_str(key: &str, name: &str, value: &str) {
    let v = w(value);
    unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, w(key).as_ptr(), w(name).as_ptr(), REG_SZ, v.as_ptr() as *const c_void, (v.len() * 2) as u32) };
}

fn reg_set_dword(key: &str, name: &str, value: u32) {
    unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, w(key).as_ptr(), w(name).as_ptr(), REG_DWORD, &value as *const u32 as *const c_void, 4) };
}

pub fn autostart_enabled() -> bool {
    reg_get(RUN_KEY, APP_NAME).is_some()
}

/// `SUBSCRIBITY_NO_INTEGRATION=1`: a test copy never touches the Start menu shortcut, the
/// "Installed apps" entry or the Run key (which belong to the real install).
fn integration_off() -> bool {
    std::env::var_os("SUBSCRIBITY_NO_INTEGRATION").is_some_and(|v| !v.is_empty())
}

pub fn set_autostart(on: bool) {
    if integration_off() {
        return;
    }
    if on {
        reg_set_str(RUN_KEY, APP_NAME, &format!("\"{}\"", exe().display()));
    } else {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, w(RUN_KEY).as_ptr(), w(APP_NAME).as_ptr()) };
    }
}

fn shortcut_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs\\Subscribity.lnk"))
}

fn ps_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "''"))
}

/// Keeps the Start menu shortcut, the "Installed apps" entry and autostart pointing at
/// this exe. Cheap: the shortcut is only (re)created when missing or the app moved.
pub fn ensure_integration() {
    if integration_off() {
        return;
    }
    let exe = exe();
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let moved = reg_get(UNINSTALL_KEY, "InstallLocation").map_or(true, |l| Path::new(&l) != dir);

    if let Some(lnk) = shortcut_path() {
        if moved || !lnk.exists() {
            // WScript.Shell is the simplest way to write a .lnk without a COM binding.
            let script = format!(
                "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({});$s.TargetPath={};$s.WorkingDirectory={};$s.Description='Subscription tracker';$s.Save()",
                ps_quote(&lnk), ps_quote(&exe), ps_quote(&dir)
            );
            let _ = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
        }
    }

    let quoted = format!("\"{}\"", exe.display());
    reg_set_str(UNINSTALL_KEY, "DisplayName", APP_NAME);
    reg_set_str(UNINSTALL_KEY, "DisplayVersion", VERSION);
    reg_set_str(UNINSTALL_KEY, "Publisher", "Subscribity");
    reg_set_str(UNINSTALL_KEY, "DisplayIcon", &format!("{},0", quoted));
    reg_set_str(UNINSTALL_KEY, "UninstallString", &format!("{} --uninstall", quoted));
    reg_set_str(UNINSTALL_KEY, "InstallLocation", &dir.display().to_string());
    reg_set_str(UNINSTALL_KEY, "URLInfoAbout", &format!("https://github.com/{}", crate::update::REPO));
    reg_set_dword(UNINSTALL_KEY, "NoModify", 1);
    reg_set_dword(UNINSTALL_KEY, "NoRepair", 1);
    let kb = std::fs::metadata(&exe).map(|m| (m.len() / 1024) as u32).unwrap_or(0);
    reg_set_dword(UNINSTALL_KEY, "EstimatedSize", kb);

    // Repair a stale Run value (exe moved or reinstalled elsewhere). Absent = autostart off: leave it.
    let stale = reg_get(RUN_KEY, APP_NAME).map_or(false, |v| run_target_differs(&v, &exe));
    if (moved && autostart_enabled()) || stale {
        set_autostart(true);
    }
}

/// True when the Run value (optionally quoted, optional args) points to a different exe than `exe`.
fn run_target_differs(value: &str, exe: &Path) -> bool {
    let v = value.trim();
    let target = match v.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(""),
        None => v,
    };
    !target.eq_ignore_ascii_case(&exe.display().to_string())
}

#[cfg(test)]
mod run_tests {
    use super::run_target_differs;
    use std::path::Path;

    #[test]
    fn run_target_compare() {
        let exe = Path::new(r"C:\Users\n\SubscribityApp\Subscribity.exe");
        assert!(!run_target_differs(r#""C:\Users\n\SubscribityApp\Subscribity.exe""#, exe));
        assert!(!run_target_differs(r"c:\users\N\subscribityapp\SUBSCRIBITY.exe", exe));
        assert!(run_target_differs(r#""C:\Users\n\Subscribity\Subscribity.exe""#, exe));
        assert!(run_target_differs("", exe));
    }
}

fn ask(text: &str, flags: u32) -> i32 {
    unsafe { MessageBoxW(null_mut(), w(text).as_ptr(), w(APP_NAME).as_ptr(), flags) }
}

/// `Subscribity.exe --uninstall` (from Settings or Windows "Installed apps").
pub fn uninstall() {
    const YESNO_QUESTION: u32 = 0x4 | 0x20;
    const YESNOCANCEL_WARNING: u32 = 0x3 | 0x30 | 0x100; // default button: No
    const IDYES: i32 = 6;
    const IDNO: i32 = 7;
    if ask("Uninstall Subscribity?\n\nThis removes the app, its Start menu shortcut and autostart.", YESNO_QUESTION) != IDYES {
        return;
    }
    let exe = exe();
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let answer = ask(
        &format!(
            "Also delete your subscriptions, payment history and backups?\n\nYes: delete them permanently. This cannot be undone.\nNo: keep them in {}\n\nOther files in that folder are never touched.",
            dir.display()
        ),
        YESNOCANCEL_WARNING,
    );
    if answer != IDYES && answer != IDNO {
        return;
    }
    let delete_data = answer == IDYES;

    close_running();
    if !integration_off() {
        unsafe {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, w(RUN_KEY).as_ptr(), w(APP_NAME).as_ptr());
            RegDeleteTreeW(HKEY_CURRENT_USER, w(UNINSTALL_KEY).as_ptr());
        }
        if let Some(lnk) = shortcut_path() {
            let _ = fs::remove_file(lnk);
        }
        // WebView2 profile (cache only), shared by every copy of the app.
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let _ = fs::remove_dir_all(PathBuf::from(local).join("Subscribity"));
        }
    }

    // Icon cache (tray popup): rebuilt on demand, so it goes even when the data stays.
    let _ = fs::remove_dir_all(dir.join(crate::icons::DIR));
    // Only the app's own files: the folder may also hold the user's other files.
    if delete_data {
        for f in data::OWN_FILES {
            let _ = fs::remove_file(dir.join(f));
        }
        let _ = fs::remove_dir_all(dir.join(data::BACKUP_DIR));
        if std::env::var_os("LOCALAPPDATA").is_none() {
            let _ = fs::remove_dir_all(dir.join("webview")); // fallback profile (see create_window)
        }
    }
    for p in [exe.with_extension("exe.old"), exe.with_extension("exe.new")] {
        let _ = fs::remove_file(p); // updater leftovers
    }

    let done = if delete_data {
        "Subscribity and its data were removed.".to_string()
    } else {
        format!("Subscribity was removed. Your data is still in {}", dir.display())
    };
    ask(&done, 0x40);

    // A running exe can't delete itself: a hidden cmd deletes it (and then the folder, only
    // if it is empty) a moment after this process exits, which it does right after this.
    // Paths go in as environment variables inside quotes: an expanded value isn't scanned
    // again for %, and quotes keep & ^ | < > literal (/v:off keeps ! literal).
    let mut cleanup = String::from("ping -n 3 127.0.0.1 >nul & del /f /q \"%SUBSCRIBITY_EXE%\" 2>nul");
    if delete_data {
        cleanup.push_str(" & rmdir \"%SUBSCRIBITY_DIR%\" 2>nul");
    }
    let _ = std::process::Command::new("cmd.exe")
        .args(["/d", "/v:off", "/c"])
        .raw_arg(&cleanup) // cmd.exe does its own quote parsing
        .env("SUBSCRIBITY_EXE", &exe)
        .env("SUBSCRIBITY_DIR", &dir)
        .current_dir(std::env::temp_dir()) // not `dir`: a cmd sitting in it would block rmdir
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// Asks the running tray and window to close and waits until both processes, and an icon
/// download helper (it exits by itself), have exited (their mutexes become abandoned), so
/// none writes a file or holds the exe afterwards.
fn close_running() {
    unsafe {
        let mutexes: Vec<HANDLE> = ["Local\\SubscribityWindowProcess", "Local\\SubscribityTrayApp", crate::icons::MUTEX]
            .iter()
            .map(|n| OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, w(n).as_ptr()))
            .collect();
        for class in ["SubscribityWindow", "SubscribityHost"] {
            let h = FindWindowW(w(class).as_ptr(), null());
            if !h.is_null() {
                PostMessageW(h, WM_CLOSE, 0, 0);
            }
        }
        for m in mutexes {
            if !m.is_null() {
                let r = WaitForSingleObject(m, 5000);
                if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
                    ReleaseMutex(m);
                }
                CloseHandle(m);
            }
        }
    }
}
