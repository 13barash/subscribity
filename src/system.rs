// Windows integration: start with Windows, Start menu shortcut, the "Installed apps" entry
// and uninstalling. Everything is per-user (HKCU / %APPDATA%), so no admin rights are needed.
use std::ffi::c_void;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, WAIT_ABANDONED, WAIT_OBJECT_0};
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
pub(crate) fn integration_off() -> bool {
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

pub(crate) fn ask(text: &str, flags: u32) -> i32 {
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
    let env = crate::datadir::Env::current();
    let root = env.root();
    let data_dir = crate::datadir::resolve(&env).unwrap_or_else(|m| m.dir);
    let answer = ask(
        &format!(
            "Also delete your subscriptions, payment history and backups?\n\nYes: delete them permanently. This cannot be undone.\nNo: keep them in {}\n\nOther files in that folder are never touched.",
            data_dir.display()
        ),
        YESNOCANCEL_WARNING,
    );
    if answer != IDYES && answer != IDNO {
        return;
    }
    let delete_data = answer == IDYES;

    close_others(true, true);
    if !integration_off() {
        unsafe {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, w(RUN_KEY).as_ptr(), w(APP_NAME).as_ptr());
            RegDeleteTreeW(HKEY_CURRENT_USER, w(UNINSTALL_KEY).as_ptr());
        }
        if let Some(lnk) = shortcut_path() {
            let _ = fs::remove_file(lnk);
        }
        // WebView2 profile (cache only, see create_window), shared by every copy of the app.
        // Only that subfolder: anything else in %LOCALAPPDATA%\Subscribity isn't ours to delete.
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let local = PathBuf::from(local).join(APP_NAME);
            let _ = fs::remove_dir_all(local.join("WebView2"));
            let _ = fs::remove_dir(local); // only if empty
        }
    }
    if std::env::var_os("LOCALAPPDATA").is_none() {
        let _ = fs::remove_dir_all(dir.join("webview")); // fallback profile (see create_window)
    }

    // Icon cache (tray popup): rebuilt on demand, so it goes even when the data stays.
    let _ = fs::remove_dir_all(data_dir.join(crate::icons::DIR));
    // Only the app's own files: the folder may also hold the user's other files. Keeping the
    // data keeps the pointer too, so a reinstall finds the data again.
    if delete_data {
        for f in data::OWN_FILES {
            let _ = fs::remove_file(data_dir.join(f));
        }
        // The default folder may hold another, older data set (Settings > "use the data
        // there" leaves it): only the pointer goes.
        for f in [data::POINTER_FILE, "data-folder.txt.tmp"] {
            let _ = fs::remove_file(root.join(f));
        }
        let _ = fs::remove_dir_all(data_dir.join(data::BACKUP_DIR));
        let _ = fs::remove_dir_all(data_dir.join(crate::datadir::STAGE_DIR));
        for d in [&data_dir, &root] {
            if !crate::datadir::same_path(d, &dir) {
                let _ = fs::remove_dir(d); // only if empty; the exe folder goes below
            }
        }
    }
    for p in [exe.with_extension("exe.old"), exe.with_extension("exe.new")] {
        let _ = fs::remove_file(p); // updater leftovers
    }

    let done = if delete_data {
        "Subscribity and its data were removed.".to_string()
    } else {
        format!("Subscribity was removed. Your data is still in {}", data_dir.display())
    };
    ask(&done, 0x40);

    // A running exe can't delete itself: a hidden cmd deletes it (and then the folder, only
    // if it is empty) a moment after this process exits, which it does right after this.
    // Paths go in as environment variables inside quotes: an expanded value isn't scanned
    // again for %, and quotes keep & ^ | < > literal (/v:off keeps ! literal).
    let mut cleanup = String::from("ping -n 3 127.0.0.1 >nul & del /f /q \"%SUBSCRIBITY_EXE%\" 2>nul");
    if delete_data || !crate::datadir::same_path(&data_dir, &dir) {
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

/// Asks the running window and/or tray to close and waits until they, an icon download helper
/// and an email scan (both exit by themselves) have exited (their mutexes become abandoned),
/// so none writes a file or holds the exe afterwards. Waits at most 5 s for each.
pub fn close_others(window: bool, tray: bool) {
    let mut names = vec![crate::icons::MUTEX, "Local\\SubscribityScan"];
    let mut classes = Vec::new();
    if window {
        names.push("Local\\SubscribityWindowProcess");
        classes.push("SubscribityWindow");
    }
    if tray {
        names.push("Local\\SubscribityTrayApp");
        classes.push("SubscribityHost");
    }
    unsafe {
        let mutexes: Vec<HANDLE> = names.iter().map(|n| OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, w(n).as_ptr())).collect();
        for class in classes {
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

/// The Windows folder picker (IFileOpenDialog in folder mode; its address bar takes a typed
/// path and it has a New folder button), opened at `start`. None when cancelled.
pub fn pick_folder(owner: HWND, start: &Path) -> Option<PathBuf> {
    use windows_sys::core::GUID;
    use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::Shell::SHCreateItemFromParsingName;
    const CLSID_FILE_OPEN_DIALOG: GUID = GUID::from_u128(0xdc1c5a9c_e88a_4dde_a5a1_60f82a20aef7);
    const IID_FILE_OPEN_DIALOG: GUID = GUID::from_u128(0xd57c7288_d4ad_4768_be02_9d969532d960);
    const IID_SHELL_ITEM: GUID = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);
    const FOS_PICKFOLDERS: u32 = 0x20;
    const FOS_FORCEFILESYSTEM: u32 = 0x40;
    const FOS_PATHMUSTEXIST: u32 = 0x800;
    const SIGDN_FILESYSPATH: i32 = 0x8005_8000u32 as i32;
    type Obj = *mut *const usize; // COM object: pointer to its vtable
    unsafe fn slot<F: Copy>(o: Obj, i: usize) -> F {
        std::mem::transmute_copy(&*(*o).add(i))
    }
    unsafe fn release(o: Obj) {
        if !o.is_null() {
            slot::<unsafe extern "system" fn(Obj) -> u32>(o, 2)(o);
        }
    }
    unsafe {
        let init = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let mut dlg: Obj = null_mut();
        let mut out = None;
        if CoCreateInstance(&CLSID_FILE_OPEN_DIALOG, null_mut(), CLSCTX_INPROC_SERVER, &IID_FILE_OPEN_DIALOG, &mut dlg as *mut Obj as *mut *mut c_void) >= 0 {
            // IFileDialog vtable: 3 Show, 9 SetOptions, 10 GetOptions, 12 SetFolder, 17 SetTitle, 20 GetResult.
            let mut opts = 0u32;
            slot::<unsafe extern "system" fn(Obj, *mut u32) -> i32>(dlg, 10)(dlg, &mut opts);
            slot::<unsafe extern "system" fn(Obj, u32) -> i32>(dlg, 9)(dlg, opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST);
            slot::<unsafe extern "system" fn(Obj, *const u16) -> i32>(dlg, 17)(dlg, w("Choose a folder for your Subscribity data").as_ptr());
            let mut item: Obj = null_mut();
            if SHCreateItemFromParsingName(w(&start.to_string_lossy()).as_ptr(), null_mut(), &IID_SHELL_ITEM, &mut item as *mut Obj as *mut *mut c_void) >= 0 {
                slot::<unsafe extern "system" fn(Obj, Obj) -> i32>(dlg, 12)(dlg, item);
                release(item);
            }
            if slot::<unsafe extern "system" fn(Obj, HWND) -> i32>(dlg, 3)(dlg, owner) >= 0 {
                let mut res: Obj = null_mut();
                if slot::<unsafe extern "system" fn(Obj, *mut Obj) -> i32>(dlg, 20)(dlg, &mut res) >= 0 && !res.is_null() {
                    // IShellItem vtable: 5 GetDisplayName.
                    let mut name: *mut u16 = null_mut();
                    if slot::<unsafe extern "system" fn(Obj, i32, *mut *mut u16) -> i32>(res, 5)(res, SIGDN_FILESYSPATH, &mut name) >= 0 && !name.is_null() {
                        let len = (0..).take_while(|&i| *name.add(i) != 0).count();
                        out = Some(PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(name, len))));
                        CoTaskMemFree(name as *const c_void);
                    }
                    release(res);
                }
            }
            release(dlg);
        }
        if init >= 0 {
            CoUninitialize();
        }
        out
    }
}
