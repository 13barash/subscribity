use super::*;
use crate::data::{days_from_civil, MAIL_KEY_FILE, SCAN_FILE};
use std::fs;

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("subscribity-test-dd-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn env(base: &Path) -> Env {
    Env { var_dir: None, no_integration: false, appdata: Some(base.join("appdata")), exe_dir: base.join("exe") }
}

const V010_SUBS: &str = "id\tname\tprice\tcurrency\tevery\tunit\tfirst_payment\tcategory\tpayment_method\tremind_days\tactive\tnotes\n3\tOld ₪\t5\tILS\t1\t2\t2026-10-16\t\t\t3\t1\t\n";

/// A v0.1.0-style data folder plus newer files, backups (incl. "before restore") and icons.
fn fill(dir: &Path) {
    fs::create_dir_all(dir.join(BACKUP_DIR).join("2026-11-01")).unwrap();
    fs::create_dir_all(dir.join(BACKUP_DIR).join("2026-11-02 before restore 2")).unwrap();
    fs::create_dir_all(dir.join(crate::icons::DIR)).unwrap();
    fs::write(dir.join(SUBS_FILE), V010_SUBS).unwrap();
    fs::write(dir.join(SETTINGS_FILE), "main_currency=ILS\ndefault_remind_days=3\n").unwrap();
    fs::write(dir.join("payments.tsv"), "3\t2026-10-16\t5\tILS\tOld\n").unwrap();
    fs::write(dir.join(MAIL_KEY_FILE), [1u8, 2, 3, 0, 255]).unwrap();
    fs::write(dir.join("subscriptions.tmp"), "stale v0.1.0 temp").unwrap();
    fs::write(dir.join(BACKUP_DIR).join("2026-11-01").join(SUBS_FILE), "b1").unwrap();
    fs::write(dir.join(BACKUP_DIR).join("2026-11-01").join("subscriptions.tsv.tmp"), "x").unwrap();
    fs::write(dir.join(BACKUP_DIR).join("2026-11-02 before restore 2").join(SUBS_FILE), "b2").unwrap();
    fs::write(dir.join(crate::icons::DIR).join("a.bmp"), "icon").unwrap();
}

#[test]
fn resolve_matrix() {
    let base = temp_dir("resolve");
    let e = env(&base);
    let root = base.join("appdata").join("Subscribity");
    fs::create_dir_all(&e.exe_dir).unwrap();
    // Fresh install: the default folder. Test copies: exe folder, or SUBSCRIBITY_DATA_DIR.
    assert!(same_path(&resolve(&e).ok().unwrap(), &root));
    let t = Env { no_integration: true, ..env(&base) };
    assert!(same_path(&t.root(), &e.exe_dir) && same_path(&resolve(&t).ok().unwrap(), &e.exe_dir));
    let v = Env { var_dir: Some(base.join("v")), no_integration: true, ..env(&base) };
    assert!(same_path(&v.root(), &base.join("v")));
    assert!(same_path(&Env { appdata: None, ..env(&base) }.root(), &e.exe_dir));
    // Pre-v0.5.0 data next to the exe and no pointer: that folder (the tray migrates it).
    fs::write(e.exe_dir.join(SETTINGS_FILE), "").unwrap();
    assert!(same_path(&resolve(&e).ok().unwrap(), &e.exe_dir));
    // Pointer: empty, BOM + CRLF + unknown lines, garbage, relative → the default folder.
    fs::create_dir_all(&root).unwrap();
    let other = base.join("other ₪ dir");
    fs::create_dir_all(&other).unwrap();
    for (text, want) in [
        ("dir=\n".to_string(), &root),
        ("garbage\n".to_string(), &root),
        ("dir=relative\\path\n".to_string(), &root),
        (format!("\u{feff}# note\r\nfoo=bar\r\n dir = {} \r\n", other.display()), &other),
    ] {
        fs::write(root.join(POINTER_FILE), text).unwrap();
        assert!(same_path(&resolve(&e).ok().unwrap(), want));
    }
    // A pointed folder that is gone is an error, never the default.
    let gone = base.join("gone");
    assert!(write_pointer(&root, Some(&gone)));
    let m = resolve(&e).err().unwrap();
    assert!(same_path(&m.dir, &gone) && same_path(&m.default, &root));
    let _ = fs::remove_dir_all(base);
}

#[test]
fn pointer_roundtrip_with_unicode_and_spaces() {
    let base = temp_dir("pointer");
    let dir = base.join("My Data ₪ €");
    assert!(write_pointer(&base, Some(&dir)));
    assert_eq!(read_pointer(&base), Some(Some(dir)));
    assert!(write_pointer(&base, None));
    assert_eq!(read_pointer(&base), Some(None));
    assert!(!base.join("data-folder.txt.tmp").exists());
    assert_eq!(read_pointer(&base.join("nope")), None);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn legacy_migration_copies_byte_equal_and_cleans_only_own_files() {
    let base = temp_dir("legacy");
    let e = env(&base);
    let (exe, root) = (e.exe_dir.clone(), e.root());
    fill(&exe);
    fs::write(exe.join("Subscribity.exe"), "exe").unwrap();
    fs::write(exe.join("my notes.txt"), "mine").unwrap();
    let before = Store::load(exe.clone());
    let subs_bytes = fs::read(exe.join(SUBS_FILE)).unwrap();
    assert_eq!(migrate_legacy(&exe, &root, days_from_civil(2026, 11, 25)), Migrated::Done);
    assert_eq!(read_pointer(&root), Some(None));
    assert_eq!(fs::read(root.join(SUBS_FILE)).unwrap(), subs_bytes);
    assert_eq!(fs::read(root.join(MAIL_KEY_FILE)).unwrap(), vec![1u8, 2, 3, 0, 255]);
    let after = Store::load(root.clone());
    assert_eq!((after.subs.len(), after.subs[0].name.as_str(), after.subs[0].start), (before.subs.len(), "Old ₪", before.subs[0].start));
    assert_eq!(after.settings.main_cur, "ILS");
    assert_eq!(fs::read_to_string(root.join(BACKUP_DIR).join("2026-11-02 before restore 2").join(SUBS_FILE)).unwrap(), "b2");
    assert!(!root.join(BACKUP_DIR).join("2026-11-01").join("subscriptions.tsv.tmp").exists());
    assert!(!root.join("subscriptions.tmp").exists() && !root.join(STAGE_DIR).exists());
    assert_eq!(fs::read_to_string(root.join(crate::icons::DIR).join("a.bmp")).unwrap(), "icon");
    // Exe folder: own files gone, the exe and foreign files kept.
    for f in [SUBS_FILE, SETTINGS_FILE, MAIL_KEY_FILE, "subscriptions.tmp", BACKUP_DIR, crate::icons::DIR] {
        assert!(!exe.join(f).exists(), "{}", f);
    }
    assert!(exe.join("Subscribity.exe").exists() && exe.join("my notes.txt").exists());
    // Next start: the pointer decides, nothing more happens.
    assert!(same_path(&resolve(&e).ok().unwrap(), &root));
    let _ = fs::remove_dir_all(base);
}

#[test]
fn legacy_migration_leftover_stage_reruns_and_both_have_data_keeps_exe() {
    let base = temp_dir("legacy2");
    let e = env(&base);
    let (exe, root) = (e.exe_dir.clone(), e.root());
    fill(&exe);
    // An earlier run stopped half-way: the stage and a half-placed file are there.
    fs::create_dir_all(root.join(STAGE_DIR)).unwrap();
    fs::write(root.join(SUBS_FILE), "partial").unwrap();
    assert_eq!(migrate_legacy(&exe, &root, 1), Migrated::Done);
    assert_eq!(fs::read_to_string(root.join(SUBS_FILE)).unwrap(), V010_SUBS);
    assert!(!root.join(BACKUP_DIR).read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("before move")));
    // Both folders have data: the default one is used, the exe folder untouched.
    let base2 = temp_dir("legacy3");
    let e2 = env(&base2);
    fill(&e2.exe_dir);
    fs::create_dir_all(e2.root()).unwrap();
    fs::write(e2.root().join(SETTINGS_FILE), "main_currency=EUR\n").unwrap();
    assert_eq!(migrate_legacy(&e2.exe_dir, &e2.root(), 1), Migrated::Kept);
    assert!(e2.exe_dir.join(SUBS_FILE).exists() && e2.exe_dir.join(BACKUP_DIR).exists());
    assert_eq!(fs::read_to_string(e2.root().join(SETTINGS_FILE)).unwrap(), "main_currency=EUR\n");
    assert!(same_path(&resolve(&e2).ok().unwrap(), &e2.root()));
    let _ = fs::remove_dir_all(base);
    let _ = fs::remove_dir_all(base2);
}

#[test]
fn move_copy_replace_use_and_rejections() {
    let base = temp_dir("move");
    let root = base.join("root");
    let (from, to) = (base.join("from"), base.join("to"));
    fs::create_dir_all(&to).unwrap();
    fill(&from);
    fs::write(from.join("foreign.txt"), "keep").unwrap();
    // Rejected: same folder, inside the current one, missing.
    assert_eq!(move_to(&from, &from, Mode::Copy, &root, 1).unwrap_err(), SAME);
    assert_eq!(move_to(&from, &from.join(BACKUP_DIR), Mode::Copy, &root, 1).unwrap_err(), INSIDE);
    assert_eq!(move_to(&from, &base.join("missing"), Mode::Copy, &root, 1).unwrap_err(), CANT_WRITE);
    assert_eq!(move_to(&from, &to, Mode::Use, &root, 1).unwrap_err(), NO_DATA);
    assert!(from.join(SUBS_FILE).exists());
    assert_eq!(inspect(&from, &to).status, Status::Empty);
    // Copy into an empty folder.
    move_to(&from, &to, Mode::Copy, &root, 1).unwrap();
    assert_eq!(read_pointer(&root), Some(Some(to.clone())));
    assert_eq!(fs::read_to_string(to.join(SUBS_FILE)).unwrap(), V010_SUBS);
    assert!(!from.join(SUBS_FILE).exists() && !from.join(BACKUP_DIR).exists() && from.join("foreign.txt").exists());
    let t = inspect(&from, &to);
    assert_eq!((t.status, t.subs, t.modified.len()), (Status::HasData, 1, 10));
    // Replace: the target's data is saved first; backups with the same name both survive.
    let other = base.join("other");
    fill(&other);
    fs::write(other.join(SUBS_FILE), "replaced").unwrap();
    fs::write(other.join(SCAN_FILE), "other mailbox").unwrap();
    let today = days_from_civil(2026, 11, 25);
    move_to(&to, &other, Mode::Replace, &root, today).unwrap();
    assert_eq!(fs::read_to_string(other.join(SUBS_FILE)).unwrap(), V010_SUBS);
    assert_eq!(fs::read_to_string(other.join(BACKUP_DIR).join("2026-11-25 before move").join(SUBS_FILE)).unwrap(), "replaced");
    assert!(other.join(BACKUP_DIR).join("2026-11-01 2").exists() && !other.join(SCAN_FILE).exists());
    // The replaced set's scan results (not in normal backups) are in the "before move" copy too.
    assert_eq!(fs::read_to_string(other.join(BACKUP_DIR).join("2026-11-25 before move").join(SCAN_FILE)).unwrap(), "other mailbox");
    // Use: only the pointer changes; nothing is copied or removed.
    fs::create_dir_all(&from).unwrap();
    fs::write(from.join(SETTINGS_FILE), "main_currency=EUR\n").unwrap();
    move_to(&other, &from, Mode::Use, &root, today).unwrap();
    assert_eq!(read_pointer(&root), Some(Some(from.clone())));
    assert!(other.join(SUBS_FILE).exists() && !from.join(SUBS_FILE).exists());
    // Back to the default folder: the pointer says "default".
    move_to(&from, &root, Mode::Copy, &root, today).unwrap();
    assert_eq!(read_pointer(&root), Some(None));
    assert!(root.join(SETTINGS_FILE).exists() && root.join(POINTER_FILE).exists());
    let _ = fs::remove_dir_all(base);
}

#[test]
fn move_to_unwritable_target_keeps_source() {
    let base = temp_dir("readonly");
    let (from, to, root) = (base.join("from"), base.join("to"), base.join("root"));
    fill(&from);
    fs::create_dir_all(&to).unwrap();
    // A file where the stage folder would go makes the copy fail half-way.
    fs::write(to.join(STAGE_DIR), "blocker").unwrap();
    assert!(move_to(&from, &to, Mode::Copy, &root, 1).is_err());
    assert!(from.join(SUBS_FILE).exists() && from.join(BACKUP_DIR).exists() && read_pointer(&root).is_none());
    assert!(!to.join(SUBS_FILE).exists());
    let _ = fs::remove_dir_all(base);
}

#[test]
fn mail_key_survives_a_move() {
    use crate::scan::store::{load_key, save_key, Login};
    let base = temp_dir("mailkey");
    let (from, to, root) = (base.join("from"), base.join("to"), base.join("root"));
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(&to).unwrap();
    fs::write(from.join(SETTINGS_FILE), "").unwrap();
    let l = Login { user: "a@b.c".into(), host: "imap.b.c".into(), pass: "secret".into() };
    assert!(save_key(&from, &l));
    move_to(&from, &to, Mode::Copy, &root, 1).unwrap();
    let back = load_key(&to).unwrap();
    assert_eq!((back.user.as_str(), back.host.as_str()), ("a@b.c", "imap.b.c"));
    let _ = fs::remove_dir_all(base);
}

#[test]
fn has_data_ignores_another_apps_settings_ini() {
    let base = temp_dir("hasdata");
    assert!(!has_data(&base));
    fs::write(base.join(SETTINGS_FILE), "[General]\nfoo=1\n").unwrap();
    assert!(!has_data(&base)); // another app's settings.ini: not our data
    fs::write(base.join(SETTINGS_FILE), "").unwrap();
    assert!(has_data(&base)); // ours: written empty on first run
    fs::write(base.join(SETTINGS_FILE), "\u{feff}[x]\r\nmain_currency=EUR\r\n").unwrap();
    assert!(has_data(&base));
    fs::write(base.join(SETTINGS_FILE), "[General]\nfoo=1\n").unwrap();
    fs::write(base.join(SUBS_FILE), V010_SUBS).unwrap();
    assert!(has_data(&base)); // subscriptions.tsv always counts
    let _ = fs::remove_dir_all(base);
}
