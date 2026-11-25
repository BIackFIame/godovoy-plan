use godovoy_plan::storage::Store;
use std::fs;

#[cfg(unix)]
#[test]
fn old_private_profile_and_recovery_copy_are_hardened_without_rewriting_content() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("profile");
    let store = Store::open(&path).unwrap();
    store
        .lock()
        .unwrap()
        .transaction(|state| state.create_workspace("Группа".into(), "Ирина".into()))
        .unwrap();
    store.lock().unwrap().create_backup().unwrap();
    drop(store);
    let calendar = path.join("calendar.json");
    let original = path.join("calendar-before-restore-previous.json");
    fs::copy(&calendar, &original).unwrap();
    let backup = fs::read_dir(path.join("backups"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("calendar-")
        })
        .unwrap();
    let bytes = fs::read(&calendar).unwrap();
    for p in [&calendar, &backup, &original] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o644)).unwrap();
    }
    drop(Store::open_existing(&path).unwrap());
    assert_eq!(fs::read(calendar.clone()).unwrap(), bytes);
    for p in [calendar, backup, original] {
        assert_eq!(fs::metadata(p).unwrap().permissions().mode() & 0o077, 0);
    }
}

#[cfg(unix)]
#[test]
fn externally_linked_readable_profile_is_rejected_without_altering_target() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("profile");
    drop(Store::open(&path).unwrap());
    let calendar = path.join("calendar.json");
    let outside = temp.path().join("archive.json");
    fs::hard_link(&calendar, &outside).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Store::open_existing(&path).is_err());
    assert_eq!(
        fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[cfg(target_os = "macos")]
#[test]
fn extended_acl_on_owned_profile_is_rejected_even_with_private_posix_mode() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("profile");
    drop(Store::open(&path).unwrap());
    let calendar = path.join("calendar.json");
    let original = fs::read(&calendar).unwrap();
    assert!(
        std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(&calendar)
            .status()
            .unwrap()
            .success()
    );
    assert!(Store::open_existing(&path).is_err());
    assert_eq!(fs::read(&calendar).unwrap(), original);
    assert!(
        std::process::Command::new("/bin/chmod")
            .arg("-N")
            .arg(&calendar)
            .status()
            .unwrap()
            .success()
    );
    assert!(Store::open_existing(&path).is_ok());
}

#[cfg(windows)]
#[test]
fn windows_profile_and_backup_reject_an_extra_file_grant() {
    use std::process::Command;
    use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("profile");
    let store = Store::open(&dir).unwrap();
    store
        .lock()
        .unwrap()
        .transaction(|s| s.create_workspace("Офис".into(), "Ирина".into()))
        .unwrap();
    store.lock().unwrap().create_backup().unwrap();
    drop(store);
    assert!(Store::open_existing(&dir).is_ok());
    let backup = fs::read_dir(dir.join("backups"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("calendar-")
        })
        .unwrap();
    let before = fs::read(&backup).unwrap();
    let registry = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .unwrap();
    let windows: String = registry.get_value("SystemRoot").unwrap();
    let icacls = std::path::Path::new(&windows).join("System32/icacls.exe");
    assert!(
        Command::new(&icacls)
            .arg(&backup)
            .args(["/grant", "*S-1-1-0:(R)"])
            .status()
            .unwrap()
            .success()
    );
    assert!(Store::open_existing(&dir).is_err());
    assert_eq!(fs::read(backup).unwrap(), before);
}

#[cfg(windows)]
#[test]
fn windows_shared_custom_directory_does_not_receive_new_profile_data() {
    use std::process::Command;
    use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("shared");
    fs::create_dir(&dir).unwrap();
    let registry = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .unwrap();
    let windows: String = registry.get_value("SystemRoot").unwrap();
    let icacls = std::path::Path::new(&windows).join("System32/icacls.exe");
    assert!(
        Command::new(icacls)
            .arg(&dir)
            .args(["/grant", "*S-1-1-0:(OI)(CI)(RX)"])
            .status()
            .unwrap()
            .success()
    );
    assert!(Store::open(&dir).is_err());
    assert!(!dir.join("calendar.json").exists());
}
