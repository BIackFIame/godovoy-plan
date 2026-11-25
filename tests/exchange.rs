use std::{
    fs,
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
    sync::Arc,
};

use chrono::NaiveDate;
use godovoy_plan::{
    exchange::{export_file, import_file},
    model::{Category, Event, Id, Record, Task, YearChange},
    storage::{ApiSettings, LocalState, SharedStore, Store},
};

const YEAR: i32 = 2026;

struct Profile {
    store: SharedStore,
    directory: tempfile::TempDir,
}

impl Profile {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("profile")).unwrap();
        Self { store, directory }
    }

    fn start_workspace(&self, name: &str) {
        self.edit(|state| state.create_workspace(name.into(), "Анна".into()));
    }

    fn edit(&self, edit: impl FnOnce(&mut LocalState) -> godovoy_plan::Fallible<()>) {
        self.store.lock().unwrap().transaction(edit).unwrap();
    }

    fn snapshot(&self) -> Arc<LocalState> {
        self.store.lock().unwrap().snapshot()
    }

    fn calendar_file(&self) -> PathBuf {
        self.directory.path().join("profile/calendar.json")
    }
}

fn event(name: &str, tasks: Vec<Task>) -> Arc<Event> {
    Event {
        title: name.into(),
        description: String::new(),
        category: Category::Partners,
        date: NaiveDate::from_ymd_opt(YEAR, 12, 20).unwrap(),
        yearly: true,
        preparation_days: 30,
        owner: None,
        tasks,
    }
    .into()
}

fn task(id: Id, name: &str) -> Task {
    Task {
        id,
        title: name.into(),
        days_before: 10,
    }
}

fn configure_local(state: &mut LocalState, seed: u8, port: u16) {
    state
        .peers
        .insert(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    state.last_sync = Some(i64::from(port));
    state.api = Some(ApiSettings {
        token: [seed; 32],
        import_token: Some([seed + 1; 32]),
        port,
        enabled: true,
    });
}

#[test]
fn divergent_calendars_converge_via_files_without_syncing_local_settings() {
    let left = Profile::new();
    let right = Profile::new();
    let right_device = right.snapshot().device;
    let common = Id::new_v4();
    let canceled = Id::new_v4();
    let deleted = Id::new_v4();
    let first = Id::new_v4();
    let second = Id::new_v4();
    let right_only = Id::new_v4();
    left.start_workspace("Тестовый календарь");
    left.edit(|state| {
        let revision = state.revision()?;
        let workspace = state.workspace_mut()?;
        workspace
            .calendar
            .members
            .insert(right_device, Record::new(revision, "Борис".into()));
        for (id, event) in [
            (
                common,
                event(
                    "План мероприятий",
                    vec![task(first, "Первая"), task(second, "Вторая")],
                ),
            ),
            (canceled, event("Отмена в конкретном году", vec![])),
            (deleted, event("Удаление шаблона", vec![])),
        ] {
            workspace
                .calendar
                .plans
                .entry(id)
                .or_default()
                .templates
                .insert(YEAR, Record::new(revision, Some(event)));
        }
        let progress = workspace
            .calendar
            .plans
            .get_mut(&common)
            .unwrap()
            .years
            .entry(YEAR)
            .or_default();
        progress.note = Some(Record::new(revision, "Старая запись".into()));
        progress.checks.insert(first, Record::new(revision, true));
        configure_local(state, 11, 40001);
        Ok(())
    });
    let shared_workspace = left.snapshot().workspace.clone().unwrap();
    right.edit(|state| {
        state.workspace = Some(shared_workspace);
        configure_local(state, 22, 40002);
        Ok(())
    });

    left.edit(|state| {
        let revision = state.revision()?;
        let plans = &mut state.workspace_mut()?.calendar.plans;
        let progress = plans
            .get_mut(&common)
            .unwrap()
            .years
            .entry(YEAR)
            .or_default();
        progress.note = Some(Record::new(
            revision,
            "Поздняя запись из первого профиля".into(),
        ));
        progress.checks.insert(second, Record::new(revision, true));
        plans
            .get_mut(&canceled)
            .unwrap()
            .years
            .entry(YEAR)
            .or_default()
            .change = Some(Record::new(revision, YearChange::Cancel));
        plans
            .get_mut(&deleted)
            .unwrap()
            .templates
            .insert(YEAR, Record::new(revision, None));
        Ok(())
    });
    right.edit(|state| {
        let revision = state.revision()?;
        let plans = &mut state.workspace_mut()?.calendar.plans;
        plans
            .get_mut(&common)
            .unwrap()
            .years
            .entry(YEAR)
            .or_default()
            .checks
            .insert(first, Record::new(revision, false));
        plans.entry(right_only).or_default().templates.insert(
            YEAR,
            Record::new(revision, Some(event("Автономное событие", vec![]))),
        );
        Ok(())
    });

    let before_left = left.snapshot();
    let before_right = right.snapshot();
    let transfer = tempfile::tempdir().unwrap();
    let usb = transfer.path().join("test-usb");
    fs::create_dir(&usb).unwrap();
    let from_left = usb.join("left.yrplan");
    let from_right = usb.join("right.yrplan");
    for (source, path) in [(&left, &from_left), (&right, &from_right)] {
        let size = export_file(&source.store, path).unwrap();
        assert_eq!(size, fs::read(path).unwrap().len());
    }
    let left_packet = fs::read(&from_left).unwrap();
    let right_packet = fs::read(&from_right).unwrap();

    assert!(import_file(&left.store, &from_right).unwrap());
    assert!(import_file(&right.store, &from_left).unwrap());

    let left_after = left.snapshot();
    let right_after = right.snapshot();
    let merged = &left_after.workspace.as_ref().unwrap().calendar;
    assert_eq!(merged, &right_after.workspace.as_ref().unwrap().calendar);
    let common = merged.event(common, YEAR).unwrap();
    assert_eq!(common.note(), "Поздняя запись из первого профиля");
    assert!(!common.checked(first));
    assert!(common.checked(second));
    assert!(merged.event(canceled, YEAR).is_none());
    assert!(merged.event(canceled, YEAR + 1).is_some());
    assert!(merged.event(deleted, YEAR).is_none());
    assert!(merged.plans[&deleted].templates[&YEAR].value.is_none());
    assert!(merged.event(right_only, YEAR).is_some());

    for (profile, old, after, replay) in [
        (&left, before_left, left_after.clone(), &from_right),
        (&right, before_right, right_after, &from_left),
    ] {
        let mut expected = (*old).clone();
        expected.workspace_mut().unwrap().calendar = merged.clone();
        assert_eq!(&*after, &expected);
        let on_disk = fs::read(profile.calendar_file()).unwrap();
        assert_eq!(
            serde_json::from_slice::<LocalState>(&on_disk).unwrap(),
            *after
        );
        assert!(!import_file(&profile.store, replay).unwrap());
        assert!(Arc::ptr_eq(&after, &profile.snapshot()));
        assert_eq!(fs::read(profile.calendar_file()).unwrap(), on_disk);
    }
    assert_eq!(fs::read(from_left).unwrap(), left_packet);
    assert_eq!(fs::read(from_right).unwrap(), right_packet);
}

#[test]
fn bad_or_foreign_files_and_duplicate_exports_preserve_previous_data() {
    let source = Profile::new();
    source.start_workspace("Первая команда");
    let other = Profile::new();
    other.start_workspace("Другая команда");
    let files = tempfile::tempdir().unwrap();
    let original = files.path().join("original.yrplan");
    export_file(&source.store, &original).unwrap();
    let original_bytes = fs::read(&original).unwrap();
    let second = files.path().join("second.yrplan");
    export_file(&source.store, &second).unwrap();
    let second_bytes = fs::read(&second).unwrap();
    assert_eq!(original_bytes.len(), second_bytes.len());
    assert_ne!(&original_bytes[8..32], &second_bytes[8..32]);

    let wrong_key = Profile::new();
    let mut same_workspace = source.snapshot().workspace.clone().unwrap();
    std::sync::Arc::make_mut(&mut same_workspace).key[0] ^= 1;
    wrong_key.edit(|state| {
        state.workspace = Some(same_workspace);
        Ok(())
    });

    let bad_tag = files.path().join("tag-error.yrplan");
    let mut modified = original_bytes.clone();
    *modified.last_mut().unwrap() ^= 1;
    fs::write(&bad_tag, &modified).unwrap();

    let tampered = files.path().join("tampered.yrplan");
    let mut modified = original_bytes.clone();
    let position = modified.len() / 2;
    modified[position] ^= 1;
    fs::write(&tampered, &modified).unwrap();

    let truncated = files.path().join("truncated.yrplan");
    fs::write(&truncated, &original_bytes[..original_bytes.len() - 1]).unwrap();
    let short = files.path().join("short.yrplan");
    fs::write(&short, &original_bytes[..8]).unwrap();

    for (profile, path) in [
        (&source, &bad_tag),
        (&source, &tampered),
        (&source, &truncated),
        (&source, &short),
        (&other, &original),
        (&wrong_key, &original),
    ] {
        let snapshot = profile.snapshot();
        let calendar_bytes = fs::read(profile.calendar_file()).unwrap();
        let rejected_bytes = fs::read(path).unwrap();
        let error = import_file(&profile.store, path).unwrap_err();
        if *path == bad_tag {
            assert_eq!(
                error.to_string(),
                "Файл повреждён или принадлежит другому календарю."
            );
        }
        assert!(Arc::ptr_eq(&profile.snapshot(), &snapshot));
        assert_eq!(fs::read(profile.calendar_file()).unwrap(), calendar_bytes);
        assert_eq!(fs::read(path).unwrap(), rejected_bytes);
    }

    source.edit(|state| {
        let revision = state.revision()?;
        state.workspace_mut()?.calendar.members.insert(
            Id::new_v4(),
            Record::new(revision, "Новая запись после экспорта".into()),
        );
        Ok(())
    });
    let snapshot = source.snapshot();
    let updated_bytes = fs::read(source.calendar_file()).unwrap();
    assert!(export_file(&source.store, &original).is_err());
    assert_eq!(fs::read(&original).unwrap(), original_bytes);
    assert!(Arc::ptr_eq(&source.snapshot(), &snapshot));
    assert_eq!(fs::read(source.calendar_file()).unwrap(), updated_bytes);
}

#[cfg(unix)]
#[test]
fn file_exchange_rejects_a_link_and_preserves_its_target() {
    let profile = Profile::new();
    profile.start_workspace("Команда");
    let files = tempfile::tempdir().unwrap();
    let target = files.path().join("target.yrplan");
    let link = files.path().join("link.yrplan");
    export_file(&profile.store, &target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let original = fs::read(&target).unwrap();
    assert!(import_file(&profile.store, &link).is_err());
    assert!(export_file(&profile.store, &link).is_err());
    assert_eq!(fs::read(&target).unwrap(), original);
}
