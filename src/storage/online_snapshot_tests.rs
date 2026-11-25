use super::*;
use crate::model::Calendar;

fn setup() -> (TempDir, OnlineVault) {
    let temp = staging(&std::env::temp_dir()).unwrap();
    let vault = OnlineVault::init(
        &temp.path().join("live"),
        "Календарь".into(),
        Calendar::default(),
        BTreeMap::new(),
    )
    .unwrap();
    (temp, vault)
}

#[test]
fn snapshot_includes_both_markers_and_renews_no_credentials() {
    let (temp, vault) = setup();
    let backup = temp.path().join("copy");
    let restored = temp.path().join("restored");
    let original_key = {
        let lock = vault.lock().unwrap();
        let (_, _, key) = lock.grant("Ada", None, false).unwrap();
        lock.set_protection(
            0,
            super::super::ProtectionSettings {
                russia: true,
                ..Default::default()
            },
        )
        .unwrap();
        lock.begin_standby(2).unwrap();
        assert!(lock.protection_report().unwrap()["backup"]["lastVerifiedAt"].is_null());
        key
    };
    vault.create_verified_backup(&backup).unwrap();
    assert!(OnlineVault::open(&backup).is_err());
    assert!(backup.join(format!("part-{STANDBY}")).is_file());
    assert!(backup.join(format!("part-{PROTECTION}")).is_file());
    assert!(OnlineVault::verify_backup(&backup).is_ok());
    let original = vault.lock().unwrap().protection_report().unwrap();
    assert!(original["backup"]["lastVerifiedAt"].is_string());
    let rendered = original.to_string();
    assert!(!rendered.contains("Ada") && !rendered.contains(&original_key));
    let stats = OnlineVault::restore_backup(&backup, &restored).unwrap();
    assert_eq!(stats["revoked"], 1);
    let copy = OnlineVault::open(&restored).unwrap();
    let restored_state = copy.lock().unwrap();
    assert!(restored_state.standby_window().unwrap().is_some());
    assert!(restored_state.protection_report().unwrap()["backup"]["lastVerifiedAt"].is_string());
    assert_eq!(restored_state.protection().unwrap().revision, 1);
    assert!(
        restored_state
            .access()
            .unwrap()
            .authorize(&original_key)
            .is_none()
    );
    assert!(
        vault
            .lock()
            .unwrap()
            .access()
            .unwrap()
            .authorize(&original_key)
            .is_some()
    );
    assert!(OnlineVault::restore_backup(&backup, &restored).is_err());
    assert!(vault.create_verified_backup(&backup).is_err());
    assert!(
        vault
            .create_verified_backup(&vault.0.join("inside"))
            .is_err()
    );
}

#[test]
fn altered_or_missing_backup_data_never_publishes_a_restore() {
    let (temp, vault) = setup();
    let backup = temp.path().join("backup");
    let dest = temp.path().join("result");
    vault.create_verified_backup(&backup).unwrap();
    let payload = backup.join(format!("part-{DATA}"));
    let mut data = fs::read(&payload).unwrap();
    data[0] = b'!';
    fs::write(&payload, data).unwrap();
    assert!(OnlineVault::verify_backup(&backup).is_err());
    assert!(OnlineVault::restore_backup(&backup, &dest).is_err());
    assert!(!dest.exists());
    fs::remove_file(payload).unwrap();
    assert!(OnlineVault::verify_backup(&backup).is_err());
}

#[test]
fn journal_coverage_distinguishes_missing_files_and_checks_records_during_backup() {
    let (temp, vault) = setup();
    for month in ["2021-02", "2021-05"] {
        let mut out = create_file(&vault.0.join(format!("security-{month}.jsonl"))).unwrap();
        writeln!(
            out,
            r#"{{"at":"{month}-01T00:00:00Z","action":"grant_requested"}}"#
        )
        .unwrap();
    }
    let report = vault.lock().unwrap().protection_report().unwrap();
    assert_eq!(
        report["audit"]["gaps"],
        json!([{"from":"2021-03","to":"2021-04"}])
    );
    assert_eq!(report["audit"]["months"], 2);
    fs::write(vault.0.join("security-2021-05.jsonl"), b"unfinished").unwrap();
    let backup = temp.path().join("copy");
    assert!(vault.create_verified_backup(&backup).is_err());
    assert!(!backup.exists());
    assert!(
        vault.lock().unwrap().protection_report().unwrap()["backup"]["lastVerifiedAt"].is_null()
    );
}

#[cfg(unix)]
#[test]
fn linked_backup_inputs_are_rejected() {
    use std::os::unix::fs::symlink;
    let (temp, vault) = setup();
    let backup = temp.path().join("copy");
    vault.create_verified_backup(&backup).unwrap();
    let part = backup.join(format!("part-{ACCESS}"));
    fs::remove_file(&part).unwrap();
    symlink(vault.0.join(ACCESS), &part).unwrap();
    assert!(OnlineVault::verify_backup(&backup).is_err());
    fs::remove_file(&part).unwrap();
    fs::hard_link(vault.0.join(ACCESS), &part).unwrap();
    assert!(OnlineVault::verify_backup(&backup).is_err());
}
