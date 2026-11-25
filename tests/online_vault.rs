//! Provisioning and migration through the same CLI shipped to administrators.
use godovoy_plan::{
    model::{Category, Event, Id, Record},
    storage::{Store, online::OnlineVault},
};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

fn cli(action: &str, dir: &Path, more: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_godovoy-online"))
        .arg(action)
        .arg(dir)
        .args(more)
        .output()
        .unwrap()
}

#[test]
fn write_fingerprint_matches_committed_bytes_across_chunks() {
    use blake2::Digest;
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("write");
    let vault = OnlineVault::init(
        &dir,
        "Календарь".into(),
        Default::default(),
        Default::default(),
    )
    .unwrap();
    let lock = vault.lock().unwrap();
    let mut data = lock.data().unwrap();
    let id = Id::new_v4();
    for _ in 0..1000 {
        godovoy_plan::storage::online::record(&mut data.audit, "check", id, id, None);
    }
    let prefix = b"per-credential and view-specific prefix";
    let hash = blake2::Blake2b::<blake2::digest::consts::U32>::new().chain_update(prefix);
    let written = lock.save_data_hashed(&data, hash.clone()).unwrap();
    let raw = lock.data_bytes().unwrap();
    assert!(raw.len() > 32 * 1024);
    assert_eq!(raw, serde_json::to_vec(&data).unwrap());
    assert_eq!(
        written.as_slice(),
        hash.chain_update(&raw).finalize().as_slice()
    );

    data.audit[0].action = "x".repeat(9 * 1024 * 1024);
    assert!(
        lock.save_data_hashed(&data, blake2::Blake2b::new())
            .is_err()
    );
    assert_eq!(raw, lock.data_bytes().unwrap());
}

#[test]
fn acl_reads_accept_exact_limit_and_reject_trailing_or_oversized_documents() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("bounded");
    let vault = OnlineVault::init(
        &dir,
        "Календарь".into(),
        Default::default(),
        Default::default(),
    )
    .unwrap();
    let lock = vault.lock().unwrap();
    let path = dir.join("online-access-v1.json");
    let mut padded = fs::read(&path).unwrap();
    padded.resize(512 * 1024, b' ');
    fs::write(&path, &padded).unwrap();
    assert!(lock.access().is_ok());
    padded.push(b' ');
    fs::write(&path, &padded).unwrap();
    assert!(lock.access().is_err());
    padded.truncate(512 * 1024);
    *padded.last_mut().unwrap() = b'{';
    fs::write(path, padded).unwrap();
    assert!(lock.access().is_err());
}

#[test]
fn legacy_office_grant_and_new_remote_grant_for_one_employee_are_independent() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("shared");
    assert!(cli("init", &dir, &["Календарь"]).status.success());
    let office = cli("grant", &dir, &["Анна"]);
    assert!(office.status.success());
    let issued = String::from_utf8(office.stdout).unwrap();
    let office = issued.lines().last().unwrap();
    let id = office.split_once('.').unwrap().0;
    let path = dir.join("online-access-v1.json");
    let mut legacy: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let grant = legacy["grants"][id].as_object_mut().unwrap();
    assert_eq!(grant.remove("allow_remote"), Some(false.into()));
    assert_eq!(grant.remove("purpose"), Some("employee".into()));
    let employee = grant["employee"].as_str().unwrap().to_owned();
    fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let remote = cli(
        "grant",
        &dir,
        &[
            "Анна",
            "--allow-remote",
            "--employee-id",
            &employee,
            "--read-only",
        ],
    );
    assert!(remote.status.success());
    let issued = String::from_utf8(remote.stdout).unwrap();
    let remote = issued.lines().last().unwrap();
    let vault = OnlineVault::open(&dir).unwrap();
    let acl = vault.lock().unwrap().access().unwrap();
    assert!(!acl.authorize(office).unwrap().1.allow_remote);
    let (remote_id, grant) = acl.authorize(remote).unwrap();
    assert!(grant.read_only && grant.allow_remote);
    let status = cli("status", &dir, &[]);
    assert!(status.status.success());
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(
        status
            .lines()
            .any(|l| l.contains(id) && l.contains("только офис"))
    );
    assert!(
        status
            .lines()
            .any(|l| l.contains(&remote_id.to_string()) && l.contains("офис и внешний вход"))
    );
    assert!(!status.contains(remote) && !status.contains(office));
    assert!(
        !cli("grant", &dir, &["Анна", "--allow-remote", "--allow-remote"])
            .status
            .success()
    );
    assert!(
        !cli("serve", &dir, &["--port", "0", "--access", "unknown"])
            .status
            .success()
    );
}
#[test]
fn migration_copies_calendar_only_and_requires_new_individual_grants() {
    let tmp = TempDir::new().unwrap();
    let source = tmp.path().join("old");
    let destination = tmp.path().join("central");
    let store = Store::open(&source).unwrap();
    let event = Id::new_v4();
    store
        .lock()
        .unwrap()
        .transaction(|s| {
            s.create_workspace("Команда".into(), "Сотрудник".into())?;
            let revision = s.revision()?;
            let e = Event {
                date: "2027-03-10".parse()?,
                title: "Согласовать график".into(),
                description: "Тест".into(),
                category: Category::Team,
                yearly: true,
                owner: Some(s.device),
                preparation_days: 7,
                tasks: vec![],
            };
            s.workspace_mut()?
                .calendar
                .plans
                .entry(event)
                .or_default()
                .templates
                .insert(2027, Record::new(revision, Some(e.into())));
            Ok(())
        })
        .unwrap();
    let employee = store.lock().unwrap().state().device;
    assert!(
        !cli(
            "init",
            &destination,
            &["--from-profile", source.to_str().unwrap()]
        )
        .status
        .success()
    );
    drop(store);
    let before = fs::read(source.join("calendar.json")).unwrap();
    assert!(
        cli(
            "init",
            &destination,
            &["--from-profile", source.to_str().unwrap()]
        )
        .status
        .success()
    );
    assert_eq!(fs::read(source.join("calendar.json")).unwrap(), before);
    assert!(!cli("init", &destination, &["Замена"]).status.success());
    assert!(
        !cli(
            "init",
            &source.join("nested"),
            &["--from-profile", source.to_str().unwrap()]
        )
        .status
        .success()
    );
    assert!(!cli("status", &source, &[]).status.success());
    assert!(!destination.join("calendar.json").exists());
    let vault = OnlineVault::open(&destination).unwrap();
    {
        let lock = vault.lock().unwrap();
        let acl = lock.access().unwrap();
        assert!(acl.grants.is_empty());
        assert_eq!(acl.employees.get(&employee).unwrap(), "Сотрудник");
        let data = lock.data().unwrap();
        assert_eq!(
            data.calendar.event(event, 2027).unwrap().event.owner,
            Some(employee)
        );
    }
    let eid = employee.to_string();
    let out = cli(
        "grant",
        &destination,
        &["Сотрудник", "--employee-id", &eid, "--read-only"],
    );
    assert!(out.status.success());
    let out = String::from_utf8(out.stdout).unwrap();
    let token = out.lines().last().unwrap();
    let (grant, _) = token.split_once('.').unwrap();
    let acl = vault.lock().unwrap().access().unwrap();
    assert_eq!(acl.authorize(token).unwrap().1.employee, employee);
    assert!(acl.authorize(token).unwrap().1.read_only);
    assert!(cli("revoke", &destination, &[grant]).status.success());
    assert!(
        vault
            .lock()
            .unwrap()
            .access()
            .unwrap()
            .authorize(token)
            .is_none()
    );
}

#[test]
fn audit_history_is_private_persistent_and_required_for_access_changes() {
    use std::io::Write;
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("central");
    assert!(cli("init", &dir, &["Test"]).status.success());
    let first = cli("grant", &dir, &["Private Fixture Name"]);
    assert!(first.status.success());
    let output = String::from_utf8(first.stdout).unwrap();
    let token = output.lines().last().unwrap();
    let (id, _) = token.split_once('.').unwrap();
    let journal = fs::read_dir(&dir)
        .unwrap()
        .map(|p| p.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .unwrap();
    let first_record = fs::read_to_string(&journal).unwrap();
    assert!(first_record.contains("grant_requested") && first_record.contains(id));
    assert!(!first_record.contains(token) && !first_record.contains("Private Fixture Name"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&journal).unwrap().mode() & 0o077, 0);
    }
    let mut log = fs::OpenOptions::new().append(true).open(&journal).unwrap();
    for _ in 0..1024 {
        log.write_all(first_record.as_bytes()).unwrap();
    }
    drop(log);
    assert!(cli("grant", &dir, &["Second"]).status.success());
    assert_eq!(fs::read_to_string(&journal).unwrap().lines().count(), 1026);
    fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(b"{")
        .unwrap();
    assert!(!cli("revoke", &dir, &[id]).status.success());
    assert!(!cli("revoke-all", &dir, &[]).status.success());
    assert!(
        OnlineVault::open(&dir)
            .unwrap()
            .lock()
            .unwrap()
            .access()
            .unwrap()
            .authorize(token)
            .is_some()
    );
}

#[cfg(unix)]
#[test]
fn audit_refuses_linked_or_public_logs() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("central");
    assert!(cli("init", &dir, &["Test"]).status.success());
    assert!(cli("grant", &dir, &["First"]).status.success());
    let journal = fs::read_dir(&dir)
        .unwrap()
        .map(|p| p.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .unwrap();
    let other = tmp.path().join("other");
    fs::rename(&journal, &other).unwrap();
    let original = fs::read(&other).unwrap();
    symlink(&other, &journal).unwrap();
    assert!(!cli("grant", &dir, &["Second"]).status.success());
    fs::remove_file(&journal).unwrap();
    fs::hard_link(&other, &journal).unwrap();
    assert!(!cli("grant", &dir, &["Second"]).status.success());
    fs::remove_file(&journal).unwrap();
    fs::rename(&other, &journal).unwrap();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!cli("grant", &dir, &["Second"]).status.success());
    assert_eq!(original, fs::read(&journal).unwrap());
}

#[test]
fn revoke_all_after_restore_is_atomic_repeatable_and_preserves_assignments() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("restored");
    assert!(cli("init", &dir, &["Архив"]).status.success());
    let writer = cli("grant", &dir, &["Анна"]);
    let reader = cli("grant", &dir, &["Иван", "--read-only"]);
    assert!(writer.status.success() && reader.status.success());
    let writer = String::from_utf8(writer.stdout).unwrap();
    let reader = String::from_utf8(reader.stdout).unwrap();
    let writer = writer.lines().last().unwrap();
    let reader = reader.lines().last().unwrap();
    let vault = OnlineVault::open(&dir).unwrap();
    let before = vault.lock().unwrap().access().unwrap();
    assert!(before.authorize(writer).is_some() && before.authorize(reader).is_some());
    let calendar = fs::read(dir.join("online-calendar-v1.json")).unwrap();
    let first = cli("revoke-all", &dir, &[]);
    assert!(first.status.success());
    assert_eq!(
        String::from_utf8(first.stdout).unwrap().trim(),
        "Отозвано ключей: 2."
    );
    let second = cli("revoke-all", &dir, &[]);
    assert!(second.status.success());
    assert_eq!(
        String::from_utf8(second.stdout).unwrap().trim(),
        "Отозвано ключей: 0."
    );
    let after = vault.lock().unwrap().access().unwrap();
    assert!(after.authorize(writer).is_none() && after.authorize(reader).is_none());
    assert_eq!(before.employees, after.employees);
    assert_eq!(
        calendar,
        fs::read(dir.join("online-calendar-v1.json")).unwrap()
    );
    let journal = dir.join(format!(
        "security-{}.jsonl",
        chrono::Utc::now().format("%Y-%m")
    ));
    assert_eq!(
        fs::read_to_string(journal)
            .unwrap()
            .lines()
            .filter(|line| line.contains("revoke_all_requested"))
            .count(),
        2
    );
}

#[test]
fn standby_cli_preserves_window_rejects_corruption_and_allows_emergency_revocation() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("central");
    assert!(cli("init", &dir, &["Офис"]).status.success());
    let issued = cli("grant", &dir, &["Анна"]);
    let token = String::from_utf8(issued.stdout).unwrap();
    let id = token.lines().last().unwrap().split_once('.').unwrap().0;
    for invalid in ["0", "1441", "-1", "1.5"] {
        assert!(
            !cli("standby-begin", &dir, &["--minutes", invalid])
                .status
                .success()
        );
    }
    let first = cli("standby-begin", &dir, &["--minutes", "1"]);
    assert!(first.status.success());
    assert_eq!(
        first.stdout,
        cli("standby-begin", &dir, &["--minutes", "1440"]).stdout
    );
    let status = cli("status", &dir, &[]);
    assert!(
        String::from_utf8(status.stdout)
            .unwrap()
            .contains(String::from_utf8(first.stdout).unwrap().trim())
    );
    assert!(!cli("grant", &dir, &["Борис"]).status.success());
    let marker = dir.join("online-standby-v1.json");
    let good = fs::read(&marker).unwrap();
    for bad in ["{", "null", r#"{"deadline":0}"#] {
        fs::write(&marker, bad).unwrap();
        assert!(
            !cli("standby-begin", &dir, &["--minutes", "30"])
                .status
                .success()
        );
        assert!(!cli("standby-end", &dir, &[]).status.success());
        assert!(!cli("grant", &dir, &["Борис"]).status.success());
        assert!(!cli("status", &dir, &[]).status.success());
    }
    fs::remove_file(&marker).unwrap();
    assert!(
        !cli("standby-begin", &dir, &["--minutes", "30"])
            .status
            .success()
    );
    assert!(!cli("standby-end", &dir, &[]).status.success());
    assert!(!cli("status", &dir, &[]).status.success());
    assert!(cli("revoke", &dir, &[id]).status.success());
    assert!(cli("revoke-all", &dir, &[]).status.success());
    fs::write(&marker, good).unwrap();
    assert!(cli("standby-end", &dir, &[]).status.success());
    assert!(cli("standby-end", &dir, &[]).status.success());
    assert!(cli("grant", &dir, &["Борис"]).status.success());
    let log = fs::read_dir(&dir)
        .unwrap()
        .map(|p| p.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .unwrap();
    let log = fs::read_to_string(log).unwrap();
    assert_eq!(log.matches("standby_begin_requested").count(), 1);
    assert_eq!(log.matches("standby_end_requested").count(), 1);
}

#[test]
fn protection_is_versioned_auditable_and_fail_closed_on_partial_or_damaged_storage() {
    use godovoy_plan::storage::online::{ProtectionConflict, ProtectionSettings};
    use serde_json::{Value, json};
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("central");
    assert!(cli("init", &dir, &["Test"]).status.success());
    let vault = OnlineVault::open(&dir).unwrap();
    let lock = vault.lock().unwrap();
    let acl = dir.join("online-access-v1.json");
    let legacy: Value = serde_json::from_slice(&fs::read(&acl).unwrap()).unwrap();
    assert!(!legacy.as_object().unwrap().contains_key("protection"));
    assert_eq!(lock.protection().unwrap().revision, 0);
    let policy = ProtectionSettings {
        russia: true,
        ..Default::default()
    };
    let first = lock.set_protection(0, policy).unwrap();
    assert_eq!(first.revision, 1);
    assert!(!first.settings.remote_allowed() && !first.settings.free_text_allowed());
    assert!(
        lock.set_protection(0, policy)
            .unwrap_err()
            .is::<ProtectionConflict>()
    );
    assert_eq!(lock.set_protection(1, policy).unwrap(), first);
    let policy = ProtectionSettings {
        russia: false,
        belarus: true,
        allow_free_text: true,
        allow_remote: true,
    };
    let second = lock.set_protection(1, policy).unwrap();
    assert_eq!(second.revision, 2);
    assert!(second.settings.remote_allowed() && second.settings.free_text_allowed());
    lock.begin_standby(5).unwrap();
    assert!(
        lock.set_protection(2, ProtectionSettings::default())
            .is_err()
    );
    assert_eq!(lock.protection().unwrap(), second);
    lock.end_standby().unwrap();
    let journal = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .unwrap();
    let logs: Vec<Value> = fs::read_to_string(&journal)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    for state in [first, second] {
        let done = logs
            .iter()
            .find(|e| e["action"] == "protection_applied" && e["protection"] == json!(state))
            .unwrap();
        assert!(logs.iter().any(|e| e["action"] == "protection_requested"
            && e["object"] == done["object"]
            && e["protection"] == done["protection"]));
    }
    let valid = fs::read(&acl).unwrap();
    let pristine: Value = serde_json::from_slice(&valid).unwrap();
    for path in [
        "/protection/version",
        "/protection/state/revision",
        "/protection/state/settings/allowRemote",
        "/protection",
    ] {
        let mut broken = pristine.clone();
        *broken.pointer_mut(path).unwrap() = Value::Null;
        fs::write(&acl, serde_json::to_vec(&broken).unwrap()).unwrap();
        assert!(lock.access().is_err());
    }
    let mut broken = pristine.clone();
    broken.as_object_mut().unwrap().remove("protection");
    fs::write(&acl, serde_json::to_vec(&broken).unwrap()).unwrap();
    assert!(lock.access().is_err());
    fs::write(&acl, &valid).unwrap();
    let pin = dir.join("online-protection-required-v1.json");
    fs::remove_file(&pin).unwrap();
    assert!(lock.access().is_err());
    fs::write(&pin, "1").unwrap();
    let mut pending = pristine;
    pending["protection"]["pendingAudit"] = json!(true);
    fs::write(&acl, serde_json::to_vec(&pending).unwrap()).unwrap();
    let saved_log = fs::read(&journal).unwrap();
    fs::write(&journal, b"partial").unwrap();
    assert!(lock.protection().is_err());
    fs::write(&journal, saved_log).unwrap();
    assert_eq!(lock.protection().unwrap(), second);
    assert_eq!(fs::read(&acl).unwrap(), valid);
    let disabled = lock
        .set_protection(2, ProtectionSettings::default())
        .unwrap();
    assert_eq!(disabled.revision, 3);
    assert!(disabled.settings.free_text_allowed() && disabled.settings.remote_allowed());
}

#[cfg(unix)]
#[test]
fn diagnostics_keep_an_unusual_profile_name_on_one_line() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("O'Connor & коллеги\nмай\t2026\u{202e}");
    fs::write(&path, b"").unwrap();
    let output = cli("status", &path, &[]);
    assert!(!output.status.success());
    let message = String::from_utf8(output.stderr).unwrap();
    assert_eq!(message.lines().count(), 1);
    assert!(message.contains(r"O'Connor & коллеги\nмай\t2026\u{202e}"));
}

#[test]
fn mcp_credentials_bind_to_one_employee_key_and_cannot_be_used_as_ordinary_keys() {
    use blake2::{Blake2s256, Digest};
    use godovoy_plan::storage::online::{GrantPermissions, GrantPurpose};

    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("mcp");
    assert!(cli("init", &dir, &["Команда"]).status.success());
    let vault = OnlineVault::open(&dir).unwrap();
    let (office_key, employee_id, employee_token) =
        vault.lock().unwrap().grant("Анна", None, false).unwrap();
    let employee = employee_id.to_string();
    let before = fs::read(dir.join("online-access-v1.json")).unwrap();
    for flags in [
        vec!["Анна", "--mcp"],
        vec!["Анна", "--employee-id", &employee, "--mcp-write"],
        vec![
            "Анна",
            "--employee-id",
            &employee,
            "--mcp",
            "--allow-remote",
        ],
        vec![
            "Анна",
            "--employee-id",
            &employee,
            "--mcp",
            "--read-only",
            "--mcp-write",
        ],
        vec!["Анна", "--employee-id", &employee, "--mcp", "--mcp"],
        vec!["Борис", "--employee-id", &employee, "--mcp"],
    ] {
        assert!(!cli("grant", &dir, &flags).status.success());
    }
    assert_eq!(fs::read(dir.join("online-access-v1.json")).unwrap(), before);
    let issued = cli(
        "grant",
        &dir,
        &["Анна", "--employee-id", &employee, "--mcp"],
    );
    assert!(issued.status.success());
    let receipt = String::from_utf8(issued.stdout).unwrap();
    let mcp = receipt.lines().last().unwrap();
    let acl = vault.lock().unwrap().access().unwrap();
    assert!(acl.authorize(mcp).is_none());
    assert!(
        acl.authorize_for(&employee_token, GrantPurpose::Mcp)
            .is_none()
    );
    let (mcp_id, grant) = acl.authorize_for(mcp, GrantPurpose::Mcp).unwrap();
    assert_eq!(grant.employee, employee_id);
    assert_eq!(grant.parent, Some(office_key));
    let rights = acl.mcp_permissions(grant).unwrap();
    assert!(rights.read_only && !rights.allow_remote);

    // Simulate the digest comparison used by versions predating the MCP key type.
    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("online-access-v1.json")).unwrap()).unwrap();
    let (_, secret) = mcp.split_once('.').unwrap();
    let old_digest: [u8; 32] = Blake2s256::digest(secret.as_bytes()).into();
    assert_ne!(
        raw["grants"][mcp_id.to_string()]["digest"],
        serde_json::json!(old_digest)
    );

    let (remote_reader, _, _) = vault
        .lock()
        .unwrap()
        .grant_with_permissions(
            "Анна",
            Some(employee_id),
            GrantPermissions {
                read_only: true,
                allow_remote: true,
            },
        )
        .unwrap();
    // Independent rights on different parent keys cannot be combined.
    assert!(
        !cli(
            "grant",
            &dir,
            &[
                "Анна",
                "--employee-id",
                &employee,
                "--mcp",
                "--allow-remote",
                "--mcp-write"
            ]
        )
        .status
        .success()
    );
    let writer = cli(
        "grant",
        &dir,
        &["Анна", "--employee-id", &employee, "--mcp", "--mcp-write"],
    );
    assert!(writer.status.success());
    let writer_receipt = String::from_utf8(writer.stdout).unwrap();
    let writer = writer_receipt.lines().last().unwrap();
    let report = vault.lock().unwrap().protection_report().unwrap();
    assert_eq!(report["credentials"]["mcp"], 2);
    assert_eq!(report["credentials"]["assignedOnly"], 2);
    let status = String::from_utf8(cli("status", &dir, &[]).stdout).unwrap();
    assert!(status.contains("Действующих ключей MCP: 2."));
    assert!(status.contains(&format!("Основание MCP: {office_key}")));
    assert!(!status.contains(mcp) && !status.contains(writer));

    let lock = vault.lock().unwrap();
    let acl = lock.access().unwrap();
    let writer_grant = acl.authorize_for(writer, GrantPurpose::Mcp).unwrap().1;
    assert!(!acl.mcp_permissions(writer_grant).unwrap().read_only);
    let (_, _, independent) = lock
        .grant_mcp(
            "Анна",
            employee_id,
            GrantPermissions {
                read_only: true,
                allow_remote: true,
            },
        )
        .unwrap();
    assert_eq!(
        lock.access()
            .unwrap()
            .authorize_for(&independent, GrantPurpose::Mcp)
            .unwrap()
            .1
            .parent,
        Some(remote_reader)
    );
    lock.revoke(office_key).unwrap();
    lock.grant("Анна", Some(employee_id), false).unwrap();
    let acl = lock.access().unwrap();
    assert!(acl.authorize_for(mcp, GrantPurpose::Mcp).is_none());
    assert!(acl.authorize_for(writer, GrantPurpose::Mcp).is_none());
    assert!(acl.authorize_for(&independent, GrantPurpose::Mcp).is_some());
}

#[cfg(unix)]
#[test]
fn failed_profile_probes_preserve_unrelated_files_and_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let folder = tmp.path().join("unrelated");
    fs::create_dir(&folder).unwrap();
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
    let sentinel = folder.join("reminders-v1.json");
    fs::write(&sentinel, "other application").unwrap();
    fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o644)).unwrap();
    let destination = tmp.path().join("destination");
    for output in [
        cli("grant", &folder, &["Test"]),
        cli(
            "init",
            &destination,
            &["--from-profile", folder.to_str().unwrap()],
        ),
        cli("backup-verify", &folder, &[]),
        cli("backup-restore", &folder, &[destination.to_str().unwrap()]),
    ] {
        assert!(!output.status.success());
    }
    assert_eq!(fs::read_dir(&folder).unwrap().count(), 1);
    assert_eq!(
        fs::metadata(&sentinel).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "other application");
    assert!(!destination.exists());

    assert!(cli("init", &destination, &["Team"]).status.success());
    let permissions_file = destination.join("online-access-v1.json");
    fs::set_permissions(&permissions_file, fs::Permissions::from_mode(0o644)).unwrap();
    for (action, args) in [
        ("unknown-command", &[][..]),
        ("grant", &["Test", "--typo"]),
        ("revoke", &["invalid-id"]),
        ("serve", &["--port", "invalid-port"]),
    ] {
        assert!(!cli(action, &destination, args).status.success());
        assert_eq!(
            fs::metadata(&permissions_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }
}

#[cfg(unix)]
#[test]
fn valid_backup_checks_the_parent_without_migrating_sibling_files() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let vault = tmp.path().join("central");
    assert!(cli("init", &vault, &["Team"]).status.success());
    let backup = tmp.path().join("backup");
    let sibling = tmp.path().join("reminders-v1.json");
    fs::write(&sibling, "unrelated sibling").unwrap();
    fs::set_permissions(&sibling, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        cli("backup", &vault, &[backup.to_str().unwrap()])
            .status
            .success()
    );
    assert!(cli("backup-verify", &backup, &[]).status.success());
    assert_eq!(
        fs::metadata(sibling).unwrap().permissions().mode() & 0o777,
        0o644
    );
}
