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

#[test]
fn concurrent_changes_stay_outside_the_captured_journal_prefix_and_acl() {
    let (temp, vault) = setup();
    let snapshot = staging(temp.path()).unwrap();
    let capture = CapturedBackup::capture(&vault, snapshot.path()).unwrap();
    let (journal, prefix) = capture.journals.first().unwrap();
    let journal = journal.clone();
    let expected = fs::read(vault.0.join(&journal)).unwrap();
    assert_eq!(prefix.limit(), expected.len() as u64);

    // This uses the normal grant path while the pending snapshot still has open files.
    let (_, _, key) = vault.lock().unwrap().grant("Ada", None, true).unwrap();
    assert!(
        vault
            .lock()
            .unwrap()
            .access()
            .unwrap()
            .authorize(&key)
            .is_some()
    );
    capture.write_to(snapshot.path()).unwrap();
    assert_eq!(
        fs::read(snapshot.path().join(format!("part-{journal}"))).unwrap(),
        expected
    );

    let restored = staging(temp.path()).unwrap();
    reconstruct(snapshot.path(), restored.path()).unwrap();
    let old = OnlineVault::open(restored.path()).unwrap();
    assert!(
        old.lock()
            .unwrap()
            .access()
            .unwrap()
            .authorize(&key)
            .is_none()
    );
}

#[test]
fn histories_longer_than_the_handle_budget_include_every_month() {
    let (temp, vault) = setup();
    let snapshot = staging(temp.path()).unwrap();
    for offset in 0..=MAX_PINNED_JOURNALS {
        let month = format!("{:04}-{:02}", 2000 + offset / 12, 1 + offset % 12);
        let mut journal = create_file(&vault.0.join(format!("security-{month}.jsonl"))).unwrap();
        writeln!(
            journal,
            r#"{{"at":"{month}-01T00:00:00Z","action":"grant_requested"}}"#
        )
        .unwrap();
    }
    let capture = CapturedBackup::capture(&vault, snapshot.path()).unwrap();
    assert_eq!(capture.journals.len(), MAX_PINNED_JOURNALS);
    // Historical months plus the live month containing backup_requested.
    assert_eq!(capture.manifest.files.len(), 2);
    capture.write_to(snapshot.path()).unwrap();
    let out = staging(temp.path()).unwrap();
    let manifest = reconstruct(snapshot.path(), out.path()).unwrap();
    assert_eq!(
        manifest
            .files
            .keys()
            .filter(|name| evidence::audit_month(name).is_some())
            .count(),
        MAX_PINNED_JOURNALS + 2
    );
}
