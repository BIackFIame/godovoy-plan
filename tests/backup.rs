use std::{
    fs,
    net::{Ipv4Addr, SocketAddrV4},
    path::Path,
    sync::Arc,
};

use chrono::NaiveDate;
use godovoy_plan::{
    model::{Category, Event, Id, Record},
    network::{Config, Network},
    storage::{
        ApiSettings, LocalState, PROFILE_LIMIT, ProfileOpenIssue, ProfileSizeLimit, SharedStore,
        Store,
    },
};

const OLD_PROFILE_LIMIT: usize = 8 * 1024 * 1024;

fn open_team(path: &Path, name: &str) -> SharedStore {
    let store = Store::open(path).unwrap();
    store
        .lock()
        .unwrap()
        .transaction(|state| state.create_workspace("Команда".into(), name.into()))
        .unwrap();
    store
}

fn set_calendar_and_credentials(store: &SharedStore, name: &str, key: u8) {
    store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let device = state.device;
            state
                .workspace_mut()?
                .calendar
                .members
                .insert(device, Record::new(revision, name.into()));
            state.api = Some(ApiSettings {
                token: [key; 32],
                import_token: Some([key + 1; 32]),
                port: 45900,
                enabled: true,
            });
            state.enrollment_grants.insert(Id::new_v4(), Id::new_v4());
            Ok(())
        })
        .unwrap();
}

fn copy_path(profile: &Path) -> std::path::PathBuf {
    fs::read_dir(Store::backup_folder(profile))
        .unwrap()
        .map(|item| item.unwrap())
        .find(|item| item.file_name().to_string_lossy().starts_with("calendar-"))
        .unwrap()
        .path()
}

fn fill_to_old_profile_limit(store: &SharedStore) {
    const YEAR: i32 = 2026;
    const MAX_TEXT: usize = 4_000;
    let date = NaiveDate::from_ymd_opt(YEAR, 12, 2).unwrap();
    store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let calendar = &mut state.workspace_mut()?.calendar;
            for index in 1..=1_050 {
                let plan = calendar.plans.entry(Id::from_u128(index)).or_default();
                let event = Event {
                    title: "Событие".into(),
                    description: String::new(),
                    category: Category::Campaign,
                    date,
                    yearly: false,
                    preparation_days: 0,
                    owner: None,
                    tasks: Vec::new(),
                };
                plan.templates
                    .insert(YEAR, Record::new(revision, Some(event.into())));
                plan.years.entry(YEAR).or_default().note =
                    Some(Record::new(revision, String::new()));
            }

            // ASCII padding adds exactly one byte per character to the compact JSON.
            let mut remaining = OLD_PROFILE_LIMIT - serde_json::to_vec(state)?.len();
            for plan in state.workspace_mut()?.calendar.plans.values_mut() {
                let event = plan
                    .templates
                    .get_mut(&YEAR)
                    .unwrap()
                    .value
                    .as_mut()
                    .unwrap();
                let description = remaining.min(MAX_TEXT);
                Arc::make_mut(event).description = "x".repeat(description);
                remaining -= description;

                let note = plan.years.get_mut(&YEAR).unwrap().note.as_mut().unwrap();
                let length = remaining.min(MAX_TEXT);
                note.value = "y".repeat(length);
                remaining -= length;
            }
            assert_eq!(remaining, 0);
            Ok(())
        })
        .unwrap();
}

#[test]
fn backup_and_restore_preserve_old_bytes_and_current_credentials() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = open_team(&profile, "Анна");
    set_calendar_and_credentials(&store, "Анна", 30);
    let backup = store.lock().unwrap().create_backup().unwrap();
    assert_eq!(
        Store::backups(&profile).unwrap(),
        std::slice::from_ref(&backup)
    );
    let bytes = fs::read(copy_path(&profile)).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(saved["secure_api_v1"].is_null());
    assert!(saved["enrollment_grants"].as_object().unwrap().is_empty());

    set_calendar_and_credentials(&store, "Вера", 44);
    let active = store.lock().unwrap().snapshot();
    let calendar = profile.join("calendar.json");
    let previous = fs::read(&calendar).unwrap();
    let locked = Store::restore_backup(&profile, &backup).unwrap_err();
    assert_eq!(locked.downcast_ref(), Some(&ProfileOpenIssue::Locked));
    assert_eq!(fs::read(&calendar).unwrap(), previous);
    drop(store);

    let reopened = Store::restore_and_open_backup(&profile, &backup).unwrap();
    assert_eq!(
        fs::read(reopened.result.original.unwrap()).unwrap(),
        previous
    );
    assert_eq!(fs::read(copy_path(&profile)).unwrap(), bytes);
    let opened = reopened.store.unwrap();
    let locked = Store::open_existing(&profile).err().unwrap();
    assert_eq!(locked.downcast_ref(), Some(&ProfileOpenIssue::Locked));
    let restored = opened.lock().unwrap().snapshot();
    let workspace = restored.workspace.as_ref().unwrap();
    assert_eq!(workspace.calendar.members[&active.device].value, "Анна");
    assert_eq!(restored.device, active.device);
    assert_eq!(restored.join_proof, active.join_proof);
    assert_eq!(restored.api, active.api);
    assert_eq!(restored.enrollment_grants, active.enrollment_grants);
    assert_eq!(
        restored.revision().unwrap().sequence,
        active.revision().unwrap().sequence
    );
}

#[test]
fn wrong_or_damaged_copy_cannot_replace_the_profile() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let other = root.path().join("other");
    let store = open_team(&profile, "Анна");
    let other_store = open_team(&other, "Борис");
    let backup = store.lock().unwrap().create_backup().unwrap();
    other_store.lock().unwrap().create_backup().unwrap();
    drop((store, other_store));

    let calendar = profile.join("calendar.json");
    let original = fs::read(&calendar).unwrap();
    let copy = copy_path(&profile);
    fs::copy(copy_path(&other), &copy).unwrap();
    let error = Store::restore_backup(&profile, &backup).unwrap_err();
    assert!(error.to_string().contains("другому профилю"));
    assert_eq!(fs::read(&calendar).unwrap(), original);

    fs::write(&copy, b"{ broken copy").unwrap();
    let error = Store::restore_backup(&profile, &backup).unwrap_err();
    assert_eq!(
        error.downcast_ref(),
        Some(&ProfileOpenIssue::InvalidCalendar)
    );
    assert_eq!(fs::read(&calendar).unwrap(), original);
    assert!(fs::read_dir(&profile).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("calendar-before-restore-")
    }));
}

#[test]
fn restoring_then_returning_uses_sanitized_points_and_preserves_current_settings() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = open_team(&profile, "Анна");
    let manual = store.lock().unwrap().create_backup().unwrap();
    set_calendar_and_credentials(&store, "Вера", 44);
    let before = store.lock().unwrap().snapshot();
    let restored = store.lock().unwrap().restore_open_backup(&manual).unwrap();
    let point = restored.restore_point.unwrap();
    assert!(point.is_restore_point());
    assert_eq!(Store::backups(&profile).unwrap(), vec![manual.clone()]);
    assert!(Store::restore_choices(&profile).unwrap().contains(&point));

    let folder = Store::backup_folder(&profile);
    let point_path = fs::read_dir(&folder)
        .unwrap()
        .map(|row| row.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("restore-point-")
        })
        .unwrap();
    let point_bytes = fs::read(&point_path).unwrap();
    let saved: LocalState = serde_json::from_slice(&point_bytes).unwrap();
    assert!(saved.api.is_none());
    assert!(saved.enrollment_grants.is_empty());
    assert_eq!(saved.workspace, before.workspace);

    set_calendar_and_credentials(&store, "Изменено после восстановления", 77);
    let current = store.lock().unwrap().snapshot();
    drop(store);
    let reverse = Store::restore_backup(&profile, &point).unwrap();
    assert!(reverse.restore_point.unwrap().is_restore_point());
    assert_eq!(fs::read(point_path).unwrap(), point_bytes);
    let reopened = Store::open_existing(&profile).unwrap();
    let after = reopened.lock().unwrap().snapshot();
    assert_eq!(after.workspace, before.workspace);
    assert_eq!(after.api, current.api);
    assert_eq!(after.enrollment_grants, current.enrollment_grants);
    assert_eq!(
        after.revision().unwrap().sequence,
        current.revision().unwrap().sequence
    );
    let disk: serde_json::Value =
        serde_json::from_slice(&fs::read(profile.join("calendar.json")).unwrap()).unwrap();
    assert_eq!(disk["sync_paused"], true);
    assert_eq!(Store::restore_choices(&profile).unwrap().len(), 3);
}

#[cfg(unix)]
#[test]
fn read_only_backup_folder_allows_restore_with_a_preserved_original() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = open_team(&profile, "Анна");
    let copy = store.lock().unwrap().create_backup().unwrap();
    set_calendar_and_credentials(&store, "Вера", 44);
    let bytes = fs::read(profile.join("calendar.json")).unwrap();
    let folder = Store::backup_folder(&profile);
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o500)).unwrap();
    let result = store.lock().unwrap().restore_open_backup(&copy);
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
    let result = result.unwrap();
    assert_eq!(fs::read(result.original.unwrap()).unwrap(), bytes);
    assert_eq!(Store::backups(&profile).unwrap(), vec![copy]);
    if result.restore_point.is_none() {
        assert!(result.restore_point_warning);
    } else {
        // An elevated test process may create files even in a read-only directory.
        assert!(!result.restore_point_warning);
    }
}

#[test]
fn an_existing_team_without_a_copy_does_not_become_a_new_profile() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    drop(open_team(&profile, "Анна"));
    let marker = profile.join("team-initialized");
    fs::remove_file(&marker).unwrap(); // The previous version had no marker.
    drop(Store::open(&profile).unwrap());
    assert!(marker.is_file());

    let calendar = profile.join("calendar.json");
    let original = root.path().join("saved-calendar.json");
    fs::rename(&calendar, &original).unwrap();
    let error = Store::open(&profile).err().expect("known team is missing");
    assert_eq!(
        error.downcast_ref(),
        Some(&ProfileOpenIssue::CalendarMissing)
    );
    assert!(!calendar.exists());
    fs::rename(&original, &calendar).unwrap();
    assert!(Store::open(&profile).is_ok());
}

#[test]
fn reopening_removes_only_app_staging_files_after_validating_the_calendar() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = open_team(&profile, "Анна");
    let backup = store.lock().unwrap().create_backup().unwrap();
    drop(store);

    let staged = profile.join(".godovoy-stage-0123456789ABCDEF");
    let staged_copy = Store::backup_folder(&profile).join(".godovoy-stage-0123456789ABCDEF");
    let unrelated = profile.join(".tmpABCDEF");
    let bad_suffix = profile.join(".godovoy-stage-0123456789ABC!EF");
    for file in [&staged, &staged_copy, &unrelated, &bad_suffix] {
        fs::write(file, b"partial data").unwrap();
    }
    let calendar = profile.join("calendar.json");
    let previous = fs::read(&calendar).unwrap();
    fs::write(&calendar, b"{ damaged").unwrap();
    assert!(Store::open(&profile).is_err());
    assert!(staged.exists() && staged_copy.exists());

    fs::write(&calendar, &previous).unwrap();
    let _opened = Store::open(&profile).unwrap();
    assert!(!staged.exists() && !staged_copy.exists());
    assert!(unrelated.is_file() && bad_suffix.is_file());
    assert_eq!(fs::read(&calendar).unwrap(), previous);
    assert_eq!(Store::backups(&profile).unwrap(), vec![backup]);
}

#[test]
fn open_restore_keeps_the_lock_and_publishes_only_a_saved_copy() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let shared = open_team(&profile, "Анна");
    let backup = shared.lock().unwrap().create_backup().unwrap();
    set_calendar_and_credentials(&shared, "Вера", 44);
    let calendar = profile.join("calendar.json");
    let previous = fs::read(&calendar).unwrap();
    let copy = copy_path(&profile);
    let valid_copy = fs::read(&copy).unwrap();

    let mut store = shared.lock().unwrap();
    let reader = store.reader();
    let before = reader.snapshot();
    fs::write(&copy, b"{ damaged").unwrap();
    assert!(store.restore_open_backup(&backup).is_err());
    assert!(Arc::ptr_eq(&before.state, &reader.snapshot().state));
    assert_eq!(fs::read(&calendar).unwrap(), previous);

    fs::write(&copy, valid_copy).unwrap();
    let result = store.restore_open_backup(&backup).unwrap();
    assert_eq!(fs::read(result.original.unwrap()).unwrap(), previous);
    assert!(result.working_snapshot.is_none());
    let after = reader.snapshot();
    assert_eq!(after.usage.bytes, fs::read(&calendar).unwrap().len());
    let workspace = after.state.workspace.as_ref().unwrap();
    assert_eq!(
        workspace.calendar.members[&before.state.device].value,
        "Анна"
    );
    assert_eq!(after.state.api, before.state.api);
    assert!(Arc::ptr_eq(&after.state, &store.snapshot()));
    let locked = Store::open_existing(&profile)
        .err()
        .expect("profile stays locked");
    assert_eq!(locked.downcast_ref(), Some(&ProfileOpenIssue::Locked));
}

#[test]
fn open_restore_preserves_working_state_if_the_current_file_is_missing_or_damaged() {
    for damage in [None, Some(&b"{ damaged"[..])] {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let shared = open_team(&profile, "Анна");
        let backup = shared.lock().unwrap().create_backup().unwrap();
        set_calendar_and_credentials(&shared, "Вера", 44);

        let calendar = profile.join("calendar.json");
        if let Some(bytes) = damage {
            fs::write(&calendar, bytes).unwrap();
        } else {
            fs::remove_file(&calendar).unwrap();
        }

        let mut store = shared.lock().unwrap();
        let active = store.snapshot();
        let result = store.restore_open_backup(&backup).unwrap();
        assert_eq!(
            result
                .original
                .map(|path| fs::read(path).unwrap())
                .as_deref(),
            damage
        );
        let saved: LocalState =
            serde_json::from_slice(&fs::read(result.working_snapshot.unwrap()).unwrap()).unwrap();
        assert_eq!(saved, *active);
        let restored = store.reader().snapshot().state;
        assert_eq!(restored.api, active.api);
        assert_eq!(restored.enrollment_grants, active.enrollment_grants);
        assert_eq!(
            restored.revision().unwrap().sequence,
            active.revision().unwrap().sequence
        );
        assert_eq!(
            restored.workspace.as_ref().unwrap().calendar.members[&restored.device].value,
            "Анна"
        );
        let bytes = fs::read(calendar).unwrap();
        let disk: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(disk["sync_paused"], true);
        let persisted: LocalState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(persisted, *restored);
    }
}

#[test]
fn restore_recovers_broken_or_missing_calendar_without_reviving_tokens() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = open_team(&profile, "Анна");
    set_calendar_and_credentials(&store, "Анна", 30);
    let backup = store.lock().unwrap().create_backup().unwrap();
    drop(store);

    let calendar = profile.join("calendar.json");
    // A copy from an older version may still have this profile's credentials.
    fs::write(copy_path(&profile), fs::read(&calendar).unwrap()).unwrap();
    fs::write(&calendar, b"{ damaged").unwrap();
    let result = Store::restore_backup(&profile, &backup).unwrap();
    assert_eq!(fs::read(result.original.unwrap()).unwrap(), b"{ damaged");
    assert!(result.restore_point.is_none());
    assert!(!result.restore_point_warning);
    let recovered = Store::open(&profile).unwrap();
    let state = recovered.lock().unwrap().snapshot();
    assert!(state.workspace.is_some());
    assert!(state.api.is_none());
    assert!(state.enrollment_grants.is_empty());
    drop(recovered);

    fs::remove_file(&calendar).unwrap();
    let error = Store::open(&profile)
        .err()
        .expect("known profile is missing");
    assert_eq!(
        error.downcast_ref(),
        Some(&ProfileOpenIssue::CalendarMissing)
    );
    assert!(!calendar.exists());
    let result = Store::restore_backup(&profile, &backup).unwrap();
    assert!(result.original.is_none());
    assert!(Store::open_existing(&profile).is_ok());
}

#[test]
fn restored_profile_stays_offline_when_opened_again() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = open_team(&profile, "Анна");
    let backup = store.lock().unwrap().create_backup().unwrap();
    drop(store);

    Store::restore_backup(&profile, &backup).unwrap();
    let reopened = Store::open_existing(&profile).unwrap();
    let config = Config {
        listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        discovery_port: None,
    };
    let network = Network::start(reopened, config).unwrap();
    assert!(network.paused());
    assert!(network.make_invite(&network.pairing_request()).is_err());

    let calendar = profile.join("calendar.json");
    let moved = root.path().join("saved-calendar.json");
    fs::rename(&calendar, &moved).unwrap();
    fs::create_dir(&calendar).unwrap();
    assert!(network.set_profile_paused(false).is_err());
    assert!(network.paused());
    fs::remove_dir(&calendar).unwrap();
    fs::rename(moved, calendar).unwrap();

    network.set_profile_paused(false).unwrap();
    assert!(network.make_invite(&network.pairing_request()).is_ok());
    drop(network);
    let reopened = Store::open_existing(&profile).unwrap();
    let network = Network::start(reopened, config).unwrap();
    assert!(!network.paused());
}

#[test]
fn a_disk_error_pauses_exchange_until_the_end_of_the_session() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = open_team(&profile, "Анна");
    let config = Config {
        listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        discovery_port: None,
    };
    let network = Network::start(store, config).unwrap();
    let calendar = profile.join("calendar.json");
    let original = profile.join("calendar.original");
    fs::rename(&calendar, &original).unwrap();
    fs::create_dir(&calendar).unwrap();
    assert!(network.set_profile_paused(true).is_err());
    assert!(network.paused());
    assert!(network.make_invite(&network.pairing_request()).is_err());
    fs::remove_dir(&calendar).unwrap();
    fs::rename(original, calendar).unwrap();
    network.set_profile_paused(true).unwrap();
    drop(network);
    let reopened = Store::open_existing(&profile).unwrap();
    assert!(Network::start(reopened, config).unwrap().paused());
}

#[test]
fn old_limit_profile_can_pause_and_restore_after_calendar_loss() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = open_team(&profile, "Анна");
    fill_to_old_profile_limit(&store);
    let calendar = profile.join("calendar.json");
    let original = fs::read(&calendar).unwrap();
    assert_eq!(original.len(), OLD_PROFILE_LIMIT);
    let backup = store.lock().unwrap().create_backup().unwrap();
    let copy = copy_path(&profile);
    assert_eq!(fs::read(&copy).unwrap(), original);

    let config = Config {
        listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        discovery_port: None,
    };
    let network = Network::start(store, config).unwrap();
    assert!(!network.paused());
    network.set_profile_paused(true).unwrap();
    assert!(network.paused());
    assert!(fs::metadata(&calendar).unwrap().len() > OLD_PROFILE_LIMIT as u64);
    drop(network);

    let reopened = Store::open_existing(&profile).unwrap();
    let network = Network::start(reopened, config).unwrap();
    assert!(network.paused());
    drop(network);

    fs::remove_file(&calendar).unwrap();
    let reopened = Store::restore_and_open_backup(&profile, &backup).unwrap();
    assert!(reopened.result.original.is_none());
    assert!(reopened.result.working_snapshot.is_none());
    assert_eq!(fs::read(copy).unwrap(), original);

    let reopened = reopened.store.unwrap();
    let restored = reopened.lock().unwrap().snapshot();
    let expected: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let mut persisted = serde_json::to_value(&*restored).unwrap();
    assert_eq!(
        persisted.as_object_mut().unwrap().remove("sync_paused"),
        Some(true.into())
    );
    assert!(
        persisted == expected,
        "cold restore changed the unpaused backup content"
    );
    let network = Network::start(reopened, config).unwrap();
    assert!(network.paused());
}

#[test]
fn restoring_old_limit_copy_keeps_live_credentials_and_revision() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = open_team(&profile, "Анна");
    fill_to_old_profile_limit(&store);
    let (backup, archived) = {
        let store = store.lock().unwrap();
        (store.create_backup().unwrap(), store.snapshot())
    };
    let copy = copy_path(&profile);
    assert_eq!(fs::metadata(&copy).unwrap().len(), OLD_PROFILE_LIMIT as u64);

    store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let device = state.device;
            let calendar = &mut state.workspace_mut()?.calendar;
            // Free room for the live metadata in the independently valid current file.
            for plan in calendar.plans.values_mut().take(8) {
                let event = plan
                    .templates
                    .values_mut()
                    .next()
                    .unwrap()
                    .value
                    .as_mut()
                    .unwrap();
                Arc::make_mut(event).description.clear();
            }
            calendar
                .members
                .insert(device, Record::new(revision, "Вера".into()));
            state.enrollment_grants = (1..=256)
                .map(|index| (Id::from_u128(index), Id::from_u128(index + 256)))
                .collect();
            state.api = Some(ApiSettings {
                token: [42; 32],
                import_token: Some([43; 32]),
                port: 45900,
                enabled: true,
            });
            state
                .peers
                .insert(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45736));
            state.last_sync = Some(1_795_000_000);
            Ok(())
        })
        .unwrap();

    let calendar = profile.join("calendar.json");
    let before = fs::read(&calendar).unwrap();
    assert!(before.len() <= OLD_PROFILE_LIMIT);
    let (active, reader) = {
        let store = store.lock().unwrap();
        (store.snapshot(), store.reader())
    };
    assert_eq!(active.enrollment_grants.len(), 256);
    let result = store.lock().unwrap().restore_open_backup(&backup).unwrap();
    assert_eq!(fs::read(result.original.unwrap()).unwrap(), before);
    assert!(result.working_snapshot.is_none());
    assert_eq!(fs::metadata(&copy).unwrap().len(), OLD_PROFILE_LIMIT as u64);

    let published = reader.snapshot();
    assert_eq!(
        published.usage.bytes,
        fs::metadata(&calendar).unwrap().len() as usize
    );
    assert!(published.usage.bytes > OLD_PROFILE_LIMIT);
    let restored = published.state;
    assert!(
        restored.workspace == archived.workspace,
        "restore must publish the exact archived workspace"
    );
    assert_eq!(restored.api, active.api);
    assert_eq!(restored.enrollment_grants, active.enrollment_grants);
    assert_eq!(restored.peers, active.peers);
    assert_eq!(restored.last_sync, active.last_sync);
    assert_eq!(
        restored.revision().unwrap().sequence,
        active.revision().unwrap().sequence
    );
    drop(store);

    let reopened = Store::open_existing(&profile).unwrap();
    assert!(*reopened.lock().unwrap().snapshot() == *restored);
    let network = Network::start(
        reopened,
        Config {
            listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            discovery_port: None,
        },
    )
    .unwrap();
    assert!(network.paused());
}

#[test]
fn old_limit_profile_rejects_growth_past_the_legacy_join_envelope() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = open_team(&profile, "Анна");
    fill_to_old_profile_limit(&store);
    let calendar = profile.join("calendar.json");
    let original = fs::read(&calendar).unwrap();
    assert_eq!(original.len(), OLD_PROFILE_LIMIT);

    let mut candidate = Vec::new();
    {
        let mut store = store.lock().unwrap();
        let reader = store.reader();
        let original_snapshot = reader.snapshot();
        let rejected = store
            .transaction(|state| {
                let workspace = state.workspace_mut()?;
                let padding = OLD_PROFILE_LIMIT - serde_json::to_vec(&*workspace)?.len();
                assert!((1..=4_000).contains(&padding));
                let note = workspace
                    .calendar
                    .plans
                    .last_entry()
                    .unwrap()
                    .into_mut()
                    .years
                    .get_mut(&2026)
                    .unwrap()
                    .note
                    .as_mut()
                    .unwrap();
                assert!(note.value.is_empty());
                note.value = "z".repeat(padding);
                // The bare workspace fits. The invitation envelope around it does not.
                assert_eq!(serde_json::to_vec(&*workspace)?.len(), OLD_PROFILE_LIMIT);
                candidate = serde_json::to_vec(state)?;
                assert!((OLD_PROFILE_LIMIT + 1..=PROFILE_LIMIT).contains(&candidate.len()));
                Ok(())
            })
            .unwrap_err();
        assert_eq!(rejected.downcast_ref(), Some(&ProfileSizeLimit::Calendar));
        assert!(Arc::ptr_eq(
            &reader.snapshot().state,
            &original_snapshot.state
        ));
        assert_eq!(reader.snapshot().usage.bytes, OLD_PROFILE_LIMIT);
        assert_eq!(fs::read(&calendar).unwrap(), original);
    }
    drop(store);

    fs::write(&calendar, &candidate).unwrap();
    let rejected = Store::open_existing(&profile)
        .err()
        .expect("legacy invitation would overflow");
    assert_eq!(
        rejected.downcast_ref(),
        Some(&ProfileOpenIssue::CalendarTooLarge)
    );
    assert_eq!(fs::read(calendar).unwrap(), candidate);
}

#[cfg(unix)]
#[test]
fn backup_rejects_a_link_to_an_unrelated_directory() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let target = root.path().join("unrelated");
    fs::create_dir(&target).unwrap();
    let store = open_team(&profile, "Анна");
    symlink(&target, Store::backup_folder(&profile)).unwrap();

    assert!(store.lock().unwrap().create_backup().is_err());
    assert!(Store::backups(&profile).is_err());
    assert_eq!(fs::read_dir(target).unwrap().count(), 0);
}
