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
