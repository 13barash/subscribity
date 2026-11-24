//! The data folder: subscriptions.tsv, settings.ini, payments, backups\, icons\, mail.key...
//!
//! Default `%APPDATA%\Subscribity` (Roaming: uninstalls before v0.5.0 wipe
//! `%LOCALAPPDATA%\Subscribity`). Settings can move it anywhere. The default folder holds the
//! pointer `data-folder.txt`, one line `dir=<absolute path>` (empty: the default folder itself).
//! Before v0.5.0 the data lived next to the exe; the tray copies it over once (`migrate_legacy`).
//!
//! Every process follows the same rule (`resolve`); only the tray migrates. A pointed folder
//! that is missing or read-only (a USB drive not plugged in) is never silently replaced by
//! another one: the tray and the window ask, helper processes just exit.
use crate::data::{self, days_from_civil, iso, write_atomic, Store, BACKUP_DIR, DATA_FILES, OWN_FILES, POINTER_FILE, PROBE_FILE, SETTINGS_FILE, SUBS_FILE};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Staging folder inside the target while a move copies and checks the files.
pub const STAGE_DIR: &str = "moving.tmp";
const POINTER_TMP: &str = "data-folder.txt.tmp";

pub const SAME: &str = "That is already your data folder";
pub const CANT_WRITE: &str = "Subscribity can't write to that folder";
pub const INSIDE: &str = "Choose a folder outside your current data folder";
pub const NO_DATA: &str = "There is no Subscribity data in that folder";
pub const FAILED: &str = "Could not move your data. Nothing was changed.";

/// What decides the data folder; `Env::current()` in the app, built by hand in tests.
pub struct Env {
    pub var_dir: Option<PathBuf>, // SUBSCRIBITY_DATA_DIR
    pub no_integration: bool,     // SUBSCRIBITY_NO_INTEGRATION (test copies)
    pub appdata: Option<PathBuf>,
    pub exe_dir: PathBuf,
}

impl Env {
    pub fn current() -> Env {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        Env {
            var_dir: var("SUBSCRIBITY_DATA_DIR"),
            no_integration: crate::system::integration_off(),
            appdata: var("APPDATA"),
            exe_dir: crate::system::exe().parent().map(Path::to_path_buf).unwrap_or_default(),
        }
    }

    /// The default data folder, which also holds the pointer. Test copies without
    /// SUBSCRIBITY_DATA_DIR keep their data next to the exe, as before v0.5.0.
    pub fn root(&self) -> PathBuf {
        if let Some(d) = &self.var_dir {
            return d.clone();
        }
        if self.no_integration {
            return self.exe_dir.clone();
        }
        self.appdata.as_ref().map_or_else(|| self.exe_dir.clone(), |a| a.join(crate::APP_NAME))
    }
}

/// The pointed folder isn't there or can't be written.
pub struct Missing {
    pub dir: PathBuf,
    pub default: PathBuf,
}

/// None: no pointer file. Some(None): the default folder. Some(Some(dir)): elsewhere.
/// Tolerates a BOM, CRLF, spaces and unknown lines; a relative path counts as the default.
pub fn read_pointer(root: &Path) -> Option<Option<PathBuf>> {
    let bytes = fs::read(root.join(POINTER_FILE)).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let dir = text
        .trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("dir"))
        .map_or("", |(_, v)| v.trim());
    let p = PathBuf::from(dir);
    Some((!dir.is_empty() && p.is_absolute()).then_some(p))
}

/// Writes the pointer atomically (None: the default folder). Creates the default folder.
pub fn write_pointer(root: &Path, dir: Option<&Path>) -> bool {
    let _ = fs::create_dir_all(root);
    let line = dir.map_or(String::new(), |d| d.to_string_lossy().into_owned());
    write_atomic(&root.join(POINTER_FILE), format!("dir={}\n", line))
}

fn pointer_for<'a>(to: &'a Path, root: &Path) -> Option<&'a Path> {
    (!same_path(to, root)).then_some(to)
}

/// Lowercase, `\\?\` and trailing separators removed, canonical when it exists.
fn norm(p: &Path) -> String {
    let c = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = c.to_string_lossy().replace('/', "\\").to_lowercase();
    let s = match s.strip_prefix(r"\\?\unc\") {
        Some(rest) => format!(r"\\{}", rest),
        None => s.strip_prefix(r"\\?\").map_or(s.clone(), str::to_string),
    };
    s.trim_end_matches('\\').to_string()
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    norm(a) == norm(b)
}

fn inside(child: &Path, parent: &Path) -> bool {
    norm(child).starts_with(&format!("{}\\", norm(parent)))
}

pub fn has_data(dir: &Path) -> bool {
    dir.join(SETTINGS_FILE).is_file() || dir.join(SUBS_FILE).is_file()
}

/// Exists and takes a file (written and deleted right away).
pub fn writable(dir: &Path) -> bool {
    let p = dir.join(PROBE_FILE);
    let ok = dir.is_dir() && fs::write(&p, b"").is_ok();
    let _ = fs::remove_file(&p);
    ok
}

/// Pointer present: its folder (must exist and be writable). No pointer: the exe folder if it
/// still holds pre-v0.5.0 data (the tray migrates it), else the default folder.
pub fn resolve(env: &Env) -> Result<PathBuf, Missing> {
    let root = env.root();
    match read_pointer(&root) {
        Some(Some(dir)) if !same_path(&dir, &root) => {
            if writable(&dir) {
                Ok(dir)
            } else {
                Err(Missing { dir, default: root })
            }
        }
        Some(_) => Ok(root),
        None if !same_path(&env.exe_dir, &root) && has_data(&env.exe_dir) => Ok(env.exe_dir.clone()),
        None => Ok(root),
    }
}

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// This process's data folder (set by `startup`/`init_quiet`; dev commands resolve lazily and
/// keep a missing folder rather than switching to another one).
pub fn get() -> PathBuf {
    DIR.get_or_init(|| resolve(&Env::current()).unwrap_or_else(|m| m.dir)).clone()
}

/// Helper processes (`--scan`, `--icons`): false = data folder unavailable, exit quietly.
pub fn init_quiet() -> bool {
    match resolve(&Env::current()) {
        Ok(d) => {
            let _ = DIR.set(d);
            true
        }
        Err(_) => false,
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Migrated {
    No,
    Done,
    Kept, // both folders had data: the default one is used, the exe folder left alone
    Failed,
}

/// Tray and window start. None: the user chose Cancel on a missing folder.
pub fn startup(tray: bool, today: i32) -> Option<(PathBuf, Migrated)> {
    let env = Env::current();
    let root = env.root();
    let mut tries = 0;
    let dir = loop {
        match resolve(&env) {
            Ok(d) => break d,
            Err(_) if tray && tries < 3 => {
                tries += 1; // at sign-in a USB or network drive may show up a moment later
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            Err(m) => match ask_missing(&m) {
                10 /* IDTRYAGAIN */ => {}
                11 /* IDCONTINUE */ => {
                    write_pointer(&m.default, None);
                    break m.default;
                }
                _ => return None,
            },
        }
    };
    let mut how = Migrated::No;
    let dir = if tray && read_pointer(&root).is_none() && !same_path(&dir, &root) {
        how = migrate_legacy(&dir, &root, today);
        if how == Migrated::Failed { dir } else { root.clone() }
    } else {
        dir
    };
    if same_path(&dir, &root) {
        let _ = fs::create_dir_all(&root);
    }
    let _ = DIR.set(dir.clone());
    Some((dir, how))
}

fn ask_missing(m: &Missing) -> i32 {
    let text = format!(
        "Subscribity can't open its data folder:\n{}\n\nTry Again: check again, for example after plugging in the drive.\nContinue: use the default folder {} instead. Your data in the folder above is not changed.\nCancel: close Subscribity.",
        m.dir.display(),
        m.default.display()
    );
    crate::system::ask(&text, 0x6 /* MB_CANCELTRYCONTINUE */ | 0x30 /* MB_ICONWARNING */)
}

/// Pre-v0.5.0 data next to the exe → the default folder. Copies and checks everything, writes
/// the pointer, then deletes the app's own files from the exe folder (failures are left).
/// Data already in the default folder wins and nothing is copied. Safe to run again.
pub fn migrate_legacy(exe_dir: &Path, root: &Path, today: i32) -> Migrated {
    let staged = root.join(STAGE_DIR).exists(); // an earlier run stopped half-way: redo it
    if has_data(root) && !staged {
        return if write_pointer(root, None) { Migrated::Kept } else { Migrated::Failed };
    }
    let _ = fs::create_dir_all(root);
    match copy_over(exe_dir, root, root, today, !staged) {
        Ok(()) => {
            clean(exe_dir);
            Migrated::Done
        }
        Err(_) => Migrated::Failed,
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Copy,    // carry the current data over (an empty target)
    Use,     // just switch to the data already there
    Replace, // carry it over; the target's data is first saved in its backups\
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    Empty,
    HasData,
    Same,
    Bad,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Empty => "empty",
            Status::HasData => "has_data",
            Status::Same => "same",
            Status::Bad => "bad",
        }
    }
}

pub struct Target {
    pub status: Status,
    pub subs: usize,
    pub modified: String, // YYYY-MM-DD of the target's newest data file, "" when empty
    pub text: &'static str, // why it is Same/Bad
}

/// What a folder offered as the new data folder holds.
pub fn inspect(cur: &Path, target: &Path) -> Target {
    let t = |status, text| Target { status, subs: 0, modified: String::new(), text };
    if !target.is_absolute() || !target.is_dir() {
        return t(Status::Bad, CANT_WRITE);
    }
    if same_path(cur, target) {
        return t(Status::Same, SAME);
    }
    if inside(target, cur) {
        return t(Status::Bad, INSIDE);
    }
    if !writable(target) {
        return t(Status::Bad, CANT_WRITE);
    }
    if !has_data(target) {
        return t(Status::Empty, "");
    }
    let newest = [SUBS_FILE, SETTINGS_FILE].iter().filter_map(|f| fs::metadata(target.join(f)).and_then(|m| m.modified()).ok()).max();
    Target { status: Status::HasData, subs: Store::load(target.to_path_buf()).subs.len(), modified: newest.map_or(String::new(), local_date), text: "" }
}

fn local_date(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64) + utc_offset();
    iso(days_from_civil(1970, 1, 1) + secs.div_euclid(86400) as i32)
}

/// Seconds the local clock is ahead of UTC.
fn utc_offset() -> i64 {
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
    let secs = |t: &windows_sys::Win32::Foundation::SYSTEMTIME| {
        days_from_civil(t.wYear as i32, t.wMonth as u32, t.wDay as u32) as i64 * 86400 + t.wHour as i64 * 3600 + t.wMinute as i64 * 60
    };
    unsafe {
        let (mut l, mut u) = (std::mem::zeroed(), std::mem::zeroed());
        GetLocalTime(&mut l);
        GetSystemTime(&mut u);
        secs(&l) - secs(&u)
    }
}

/// Settings move from `from` (the current data folder) to `to`. Copy/Replace: everything is
/// copied into `to\moving.tmp\` and compared byte for byte, the target's own data is saved to
/// its backups\, the files are put in place (settings.ini last), then the pointer is written:
/// that is the commit. Any error before it leaves `from` and the pointer untouched. After it,
/// the app's own files are removed from `from`.
pub fn move_to(from: &Path, to: &Path, mode: Mode, root: &Path, today: i32) -> Result<(), String> {
    let t = inspect(from, to);
    match t.status {
        Status::Same | Status::Bad => return Err(t.text.into()),
        Status::Empty if mode == Mode::Use => return Err(NO_DATA.into()),
        _ => {}
    }
    let _ = fs::create_dir_all(root);
    if !writable(root) {
        return Err(FAILED.into()); // the pointer couldn't be written
    }
    if mode == Mode::Use {
        return if write_pointer(root, pointer_for(to, root)) { Ok(()) } else { Err(FAILED.into()) };
    }
    copy_over(from, to, root, today, true).map_err(|_| FAILED.to_string())?;
    clean(from);
    Ok(())
}

/// The copy, check, put-in-place and pointer steps of a move (see `move_to`).
fn copy_over(from: &Path, to: &Path, root: &Path, today: i32, backup_target: bool) -> io::Result<()> {
    let stage = to.join(STAGE_DIR);
    let r = (|| {
        let _ = fs::remove_dir_all(&stage);
        fs::create_dir_all(&stage)?;
        for f in DATA_FILES {
            if from.join(f).is_file() {
                copy_checked(&from.join(f), &stage.join(f))?;
            }
        }
        if from.join(BACKUP_DIR).is_dir() {
            copy_tree(&from.join(BACKUP_DIR), &stage.join(BACKUP_DIR), true)?;
        }
        if from.join(crate::icons::DIR).is_dir() {
            copy_tree(&from.join(crate::icons::DIR), &stage.join(crate::icons::DIR), false)?; // cache: best effort
        }
        if backup_target && has_data(to) {
            let b = data::unique_path(&to.join(BACKUP_DIR), &format!("{} before move", iso(today)));
            // Every data file of the replaced set (mail.key, scan.tsv and state.tsv too, which a
            // normal backup leaves out), so "saved as a backup first" holds for all of it.
            fs::create_dir_all(&b)?;
            for f in DATA_FILES {
                if to.join(f).is_file() {
                    copy_checked(&to.join(f), &b.join(f))?;
                }
            }
        }
        place(&stage, to)?;
        if !write_pointer(root, pointer_for(to, root)) {
            return Err(io::Error::other("could not write the pointer"));
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&stage);
    if r.is_ok() {
        // The target now is exactly the moved data: its own files the source didn't have
        // (a mailbox login, reminder state of the replaced data) go. Exports stay.
        for f in DATA_FILES.iter().filter(|f| **f != data::EXPORT_FILE) {
            if !from.join(f).exists() {
                let _ = fs::remove_file(to.join(f));
            }
        }
    }
    r
}

/// Staged files → `to`: backups (a name already there gets " 2"), icons (replaced), then the
/// data files, settings.ini last.
fn place(stage: &Path, to: &Path) -> io::Result<()> {
    let sb = stage.join(BACKUP_DIR);
    if sb.is_dir() {
        let tb = to.join(BACKUP_DIR);
        fs::create_dir_all(&tb)?;
        for e in fs::read_dir(&sb)? {
            let e = e?;
            fs::rename(e.path(), data::unique_path(&tb, &e.file_name().to_string_lossy()))?;
        }
    }
    let si = stage.join(crate::icons::DIR);
    if si.is_dir() {
        let ti = to.join(crate::icons::DIR);
        let _ = fs::create_dir_all(&ti);
        for e in fs::read_dir(&si)?.flatten() {
            if e.path().is_file() {
                let _ = fs::rename(e.path(), ti.join(e.file_name()));
            }
        }
    }
    for f in DATA_FILES {
        if stage.join(f).is_file() {
            fs::rename(stage.join(f), to.join(f))?;
        }
    }
    Ok(())
}

/// Copy, flush, then compare length and bytes.
fn copy_checked(from: &Path, to: &Path) -> io::Result<()> {
    fs::copy(from, to)?;
    fs::OpenOptions::new().write(true).open(to)?.sync_all()?;
    let (a, b) = (fs::read(from)?, fs::read(to)?);
    if a.len() != b.len() || a != b {
        return Err(io::Error::other("copy differs"));
    }
    Ok(())
}

/// Recursive copy; temp files a crash left behind are skipped. Not `strict`: a file that
/// fails is dropped and the rest goes on.
fn copy_tree(from: &Path, to: &Path, strict: bool) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            copy_tree(&src, &dst, strict)?;
        } else if !e.file_name().to_string_lossy().to_lowercase().ends_with(".tmp") {
            if let Err(err) = copy_checked(&src, &dst) {
                let _ = fs::remove_file(&dst);
                if strict {
                    return Err(err);
                }
            }
        }
    }
    Ok(())
}

/// The app's own files, backups\ and icons\ out of a folder it no longer uses. The pointer
/// stays (the old folder may be the default one); nothing else is touched.
fn clean(dir: &Path) {
    for f in OWN_FILES.iter().filter(|f| **f != POINTER_FILE && **f != POINTER_TMP) {
        let _ = fs::remove_file(dir.join(f));
    }
    let _ = fs::remove_dir_all(dir.join(BACKUP_DIR));
    let _ = fs::remove_dir_all(dir.join(crate::icons::DIR));
}

#[path = "datadir_ipc.rs"]
mod ipc;
pub use ipc::{handle_ipc, on_moved, state, MOVED};

#[cfg(test)]
#[path = "datadir_tests.rs"]
mod tests;
