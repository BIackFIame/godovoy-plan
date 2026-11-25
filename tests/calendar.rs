use chrono::NaiveDate;
use godovoy_plan::model::{Calendar, Category, Event, Id, Record, Revision, Task, YearChange};

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn template(owner: Id, date: NaiveDate) -> Event {
    Event {
        title: "Поздравления".into(),
        description: String::new(),
        category: Category::Partners,
        date,
        yearly: true,
        preparation_days: 45,
        owner: Some(owner),
        tasks: vec![
            Task {
                id: Id::new_v4(),
                title: "Список партнёров".into(),
                days_before: 35,
            },
            Task {
                id: Id::new_v4(),
                title: "Согласовать макет".into(),
                days_before: 19,
            },
        ],
    }
}

#[test]
fn preparation_and_deadlines_recur_with_new_checklist() {
    let mut calendar = Calendar::default();
    let id = Id::new_v4();
    let owner = Id::new_v4();
    let event = template(owner, date(2026, 12, 20));
    let first = event.tasks[0].id;
    let plan = calendar.plans.entry(id).or_default();
    let rev = Revision {
        operation: owner,
        sequence: 1,
    };
    plan.templates
        .insert(2026, Record::new(rev, Some(event.into())));
    plan.years
        .entry(2026)
        .or_default()
        .checks
        .insert(first, Record::new(rev, true));

    let next = calendar.event(id, 2027).unwrap();
    assert_eq!(next.preparation_date(), date(2027, 11, 5));
    assert_eq!(next.due(&next.event.tasks[0]), date(2027, 11, 15));
    assert!(!next.needs_attention(date(2027, 11, 4), owner));
    assert!(next.needs_attention(date(2027, 11, 5), owner));
    assert!(!next.needs_attention(date(2027, 11, 5), Id::new_v4()));
    assert!(!next.checked(first));
    assert!(calendar.event(id, 2026).unwrap().checked(first));
    assert!(calendar.event(id, 2025).is_none());
}

#[test]
fn changes_to_different_tasks_merge_and_converge() {
    let mut left = Calendar::default();
    let id = Id::new_v4();
    let alice = Id::new_v4();
    let bob = Id::new_v4();
    let event = template(alice, date(2026, 12, 20));
    let first = event.tasks[0].id;
    let second = event.tasks[1].id;
    left.plans.entry(id).or_default().templates.insert(
        2026,
        Record::new(
            Revision {
                operation: alice,
                sequence: 1,
            },
            Some(event.into()),
        ),
    );
    let mut right = left.clone();
    for (calendar, task, device) in [(&mut left, first, alice), (&mut right, second, bob)] {
        calendar
            .plans
            .get_mut(&id)
            .unwrap()
            .years
            .entry(2026)
            .or_default()
            .checks
            .insert(
                task,
                Record::new(
                    Revision {
                        operation: device,
                        sequence: 2,
                    },
                    true,
                ),
            );
    }
    let previous_left = left.clone();
    left.merge(right.clone());
    right.merge(previous_left);
    assert_eq!(left, right);
    assert_eq!(left.event(id, 2026).unwrap().completed(), 2);
}

#[test]
fn one_year_override_and_future_template_keep_earlier_history() {
    let mut calendar = Calendar::default();
    let id = Id::new_v4();
    let owner = Id::new_v4();
    let event = template(owner, date(2024, 2, 29));
    let plan = calendar.plans.entry(id).or_default();
    plan.templates.insert(
        2024,
        Record::new(
            Revision {
                operation: owner,
                sequence: 1,
            },
            Some(event.clone().into()),
        ),
    );
    let mut override_event = event.clone();
    override_event.date = date(2025, 3, 4);
    plan.years.entry(2025).or_default().change = Some(Record::new(
        Revision {
            operation: owner,
            sequence: 2,
        },
        YearChange::Edit(override_event.into()),
    ));
    let mut new_template = event;
    new_template.date = date(2027, 4, 1);
    plan.templates.insert(
        2027,
        Record::new(
            Revision {
                operation: owner,
                sequence: 3,
            },
            Some(new_template.into()),
        ),
    );
    assert_eq!(calendar.event(id, 2024).unwrap().date, date(2024, 2, 29));
    assert_eq!(calendar.event(id, 2025).unwrap().date, date(2025, 3, 4));
    assert_eq!(calendar.event(id, 2026).unwrap().date, date(2026, 2, 28));
    assert_eq!(calendar.event(id, 2028).unwrap().date, date(2028, 4, 1));
}

#[test]
fn agenda_and_reminders_follow_owners_and_cross_year_deadlines() {
    use godovoy_plan::planning::{self, Horizon, Milestone, OwnerFilter};

    let owner = Id::new_v4();
    let colleague = Id::new_v4();
    let first_id = Id::new_v4();
    let second_id = Id::new_v4();
    let mut calendar = Calendar::default();
    let mut january = template(owner, date(2027, 1, 12));
    january.preparation_days = 30;
    january.tasks.truncate(1);
    january.tasks[0].days_before = 15;
    let task_id = january.tasks[0].id;
    let mut unassigned = january.clone();
    unassigned.owner = None;
    for (id, event) in [(first_id, january), (second_id, unassigned)] {
        calendar.plans.entry(id).or_default().templates.insert(
            2027,
            Record::new(
                Revision {
                    sequence: 1,
                    operation: id,
                },
                Some(event.into()),
            ),
        );
    }

    let today = date(2026, 12, 21);
    let rows = planning::agenda(
        &calendar,
        today,
        Horizon::Days30,
        OwnerFilter::Member(owner),
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].date, rows[0].kind),
        (date(2026, 12, 28), Milestone::Task(task_id))
    );
    assert_eq!(rows[1].kind, Milestone::Event);
    assert!(rows.iter().all(|item| item.event == first_id));
    let unassigned = planning::agenda(&calendar, today, Horizon::Days60, OwnerFilter::Unassigned);
    assert!(unassigned.iter().all(|item| item.event == second_id));
    assert!(planning::personal_events(&calendar, today, colleague).is_empty());

    let day_before_preparation = date(2026, 12, 12);
    assert!(planning::personal_events(&calendar, day_before_preparation, owner).is_empty());
    let preparation_day = date(2026, 12, 13);
    let starting = planning::personal_events(&calendar, preparation_day, owner);
    assert_eq!(starting.len(), 1);
    assert_eq!(starting[0].year, 2027);
    assert_eq!(
        planning::due_reminders(&starting, preparation_day)[0].kind,
        Milestone::Preparation
    );
    assert_eq!(
        planning::personal_events(&calendar, date(2027, 1, 1), owner)[0].year,
        2027
    );

    let early = date(2026, 12, 26);
    let events = planning::personal_events(&calendar, early, owner);
    assert_eq!(planning::due_reminders(&events, early).len(), 1);
    let tomorrow = date(2026, 12, 27);
    let reminders = planning::due_reminders(&events, tomorrow);
    assert_eq!(reminders.len(), 2);
    assert_eq!(reminders[1].kind, Milestone::Task(task_id));
    assert_eq!(reminders[1].date, date(2026, 12, 28));
}

#[test]
fn team_overdue_filters_open_tasks_by_due_date_and_owner() {
    use chrono::Days;
    use godovoy_plan::planning::{self, Horizon, Milestone, OVERDUE_DAYS, OwnerFilter};

    let today = date(2026, 12, 29);
    let owner = Id::new_v4();
    let boundary_id = Id::new_v4();
    let next_year_id = Id::new_v4();
    let unassigned_id = Id::new_v4();
    let cancelled_id = Id::new_v4();
    let mut calendar = Calendar::default();

    let mut boundary = template(owner, today);
    boundary.yearly = false;
    boundary.preparation_days = 91;
    boundary.tasks = [91, 90, 0]
        .map(|days_before| Task {
            id: Id::new_v4(),
            title: format!("Срок за {days_before} дней"),
            days_before,
        })
        .into();
    let oldest_task = boundary.tasks[1].id;
    let today_task = boundary.tasks[2].id;

    let mut january = template(owner, date(2027, 1, 12));
    january
        .tasks
        .iter_mut()
        .for_each(|task| task.days_before = 15);
    let open_task = january.tasks[0].id;
    let checked_task = january.tasks[1].id;
    let cancelled = january.clone();
    let mut unassigned = january.clone();
    unassigned.owner = None;
    unassigned.tasks.truncate(1);
    let revision = Revision {
        sequence: 1,
        operation: owner,
    };
    for (id, year, event) in [
        (boundary_id, 2026, boundary),
        (next_year_id, 2027, january),
        (unassigned_id, 2027, unassigned),
        (cancelled_id, 2027, cancelled),
    ] {
        calendar
            .plans
            .entry(id)
            .or_default()
            .templates
            .insert(year, Record::new(revision, Some(event.into())));
    }
    calendar
        .plans
        .get_mut(&next_year_id)
        .unwrap()
        .years
        .entry(2027)
        .or_default()
        .checks
        .insert(checked_task, Record::new(revision, true));
    calendar
        .plans
        .get_mut(&cancelled_id)
        .unwrap()
        .years
        .entry(2027)
        .or_default()
        .change = Some(Record::new(revision, YearChange::Cancel));

    let rows = planning::overdue(&calendar, today, OwnerFilter::Everyone);
    assert_eq!(rows.len(), 3);
    let owned = planning::overdue(&calendar, today, OwnerFilter::Member(owner));
    assert_eq!(owned.len(), 2);
    assert_eq!(owned[0].event, boundary_id);
    assert_eq!(
        owned[0].date,
        today.checked_sub_days(Days::new(OVERDUE_DAYS)).unwrap()
    );
    assert_eq!(owned[0].kind, Milestone::Task(oldest_task));
    assert_eq!(owned[1].event, next_year_id);
    assert_eq!(owned[1].year, 2027);
    assert_eq!(owned[1].date, today.pred_opt().unwrap());
    assert_eq!(owned[1].kind, Milestone::Task(open_task));
    let unassigned = planning::overdue(&calendar, today, OwnerFilter::Unassigned);
    assert_eq!(unassigned.len(), 1);
    assert_eq!(unassigned[0].event, unassigned_id);
    assert!(planning::overdue(&calendar, today, OwnerFilter::Member(Id::new_v4())).is_empty());

    let upcoming = planning::agenda(
        &calendar,
        today,
        Horizon::Days30,
        OwnerFilter::Member(owner),
    );
    assert!(
        upcoming
            .iter()
            .any(|row| row.date == today && row.kind == Milestone::Task(today_task))
    );
    let after_new_year = planning::overdue(&calendar, date(2027, 1, 1), OwnerFilter::Member(owner));
    assert!(after_new_year.contains(&owned[1]));
    assert!(after_new_year.iter().any(|row| row.date == today
        && row.year == 2026
        && row.kind == Milestone::Task(today_task)));
}

#[test]
fn restoring_a_copy_does_not_reuse_operation_ids() {
    use godovoy_plan::storage::Store;

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("profile")).unwrap();
    let guard = store.lock().unwrap();
    let state = guard.state();
    let restored = state.clone();
    let before = state.revision().unwrap();
    let after = restored.revision().unwrap();
    assert_eq!(before.sequence, after.sequence);
    assert_ne!(before.operation, after.operation);
    for revision in [before, after] {
        let encoded = serde_json::to_value(revision).unwrap();
        assert_eq!(encoded["device"], revision.operation.to_string());
    }
}

#[test]
fn reminder_receipts_survive_restart_without_rewriting_calendar() {
    use godovoy_plan::{planning, storage::Store};

    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let store = Store::open(&profile).unwrap();
    let today = date(2026, 12, 3);
    let event_id = Id::new_v4();
    let mut guard = store.lock().unwrap();
    guard
        .transaction(|state| {
            state.create_workspace("Команда".into(), "Анна".into())?;
            let event = template(state.device, date(2026, 12, 20));
            let revision = state.revision()?;
            state
                .workspace_mut()?
                .calendar
                .plans
                .entry(event_id)
                .or_default()
                .templates
                .insert(2026, Record::new(revision, Some(event.into())));
            Ok(())
        })
        .unwrap();
    let snapshot = guard.snapshot();
    let calendar = &snapshot.workspace.as_ref().unwrap().calendar;
    let events = planning::personal_events(calendar, today, snapshot.device);
    let reminders = planning::due_reminders(&events, today);
    assert!(!reminders.is_empty());
    let calendar_path = profile.join("calendar.json");
    let original = std::fs::read(&calendar_path).unwrap();
    guard.acknowledge_reminders(today, &reminders[..1]).unwrap();
    assert_eq!(std::fs::read(&calendar_path).unwrap(), original);
    drop(guard);
    drop(store);

    let reopened = Store::open(&profile).unwrap();
    let actual = reopened.lock().unwrap().reminder_receipts();
    assert_eq!(actual.iter().copied().collect::<Vec<_>>(), reminders[..1]);
}

#[test]
fn damaged_reminder_file_does_not_block_calendar_and_recovery_keeps_original() {
    use godovoy_plan::storage::{ReminderFileIssue, Store};

    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path().join("profile");
    let store = Store::open(&profile).unwrap();
    store
        .lock()
        .unwrap()
        .transaction(|state| state.create_workspace("Команда".into(), "Анна".into()))
        .unwrap();
    drop(store);

    let calendar = std::fs::read(profile.join("calendar.json")).unwrap();
    let sidecar = profile.join("reminders-v1.json");
    let damaged = b"{ incomplete";
    std::fs::write(&sidecar, damaged).unwrap();

    let store = Store::open(&profile).unwrap();
    let mut guard = store.lock().unwrap();
    assert!(guard.state().workspace.is_some());
    assert_eq!(guard.reminder_issue(), Some(ReminderFileIssue::InvalidData));
    assert!(guard.reminder_receipts().is_empty());
    assert!(
        guard
            .acknowledge_reminders(date(2026, 11, 23), &[])
            .is_err()
    );
    assert_eq!(std::fs::read(&sidecar).unwrap(), damaged);

    guard.recover_reminders().unwrap();
    assert_eq!(guard.reminder_issue(), None);
    let original = std::fs::read_dir(&profile)
        .unwrap()
        .filter_map(Result::ok)
        .find(|item| {
            item.file_name()
                .to_string_lossy()
                .starts_with("reminders-damaged-")
        })
        .unwrap();
    assert_eq!(std::fs::read(original.path()).unwrap(), damaged);
    assert_eq!(
        std::fs::read(profile.join("calendar.json")).unwrap(),
        calendar
    );
    drop(guard);
    drop(store);
    assert_eq!(
        Store::open(&profile)
            .unwrap()
            .lock()
            .unwrap()
            .reminder_issue(),
        None
    );
}

#[test]
fn damaged_calendar_is_preserved_and_retry_requires_a_copy() {
    use godovoy_plan::storage::{ProfileOpenIssue, Store};

    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = Store::open(&profile).unwrap();
    let device = store.lock().unwrap().state().device;
    drop(store);
    let path = profile.join("calendar.json");
    let backup = std::fs::read(&path).unwrap();
    let damaged = b"{ incomplete";
    std::fs::write(&path, damaged).unwrap();

    let error = Store::open(&profile).err().expect("damaged calendar");
    assert_eq!(
        error.downcast_ref::<ProfileOpenIssue>(),
        Some(&ProfileOpenIssue::InvalidCalendar)
    );
    assert_eq!(std::fs::read(&path).unwrap(), damaged);

    std::fs::rename(&path, profile.join("calendar-damaged.json")).unwrap();
    let error = Store::open_existing(&profile).err().expect("missing copy");
    assert_eq!(
        error.downcast_ref::<ProfileOpenIssue>(),
        Some(&ProfileOpenIssue::CalendarMissing)
    );
    assert!(!path.exists());

    std::fs::write(&path, backup).unwrap();
    let recovered = Store::open_existing(&profile).unwrap();
    assert_eq!(recovered.lock().unwrap().state().device, device);
    assert_eq!(
        std::fs::read(profile.join("calendar-damaged.json")).unwrap(),
        damaged
    );
}

#[test]
fn an_oversize_calendar_stays_untouched() {
    use godovoy_plan::storage::{PROFILE_LIMIT, ProfileOpenIssue, Store};

    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path().join("profile");
    drop(Store::open(&profile).unwrap());
    let path = profile.join("calendar.json");
    let bytes = PROFILE_LIMIT as u64 + 1;
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(bytes)
        .unwrap();
    let error = Store::open(&profile).err().expect("oversize calendar");
    assert_eq!(
        error.downcast_ref::<ProfileOpenIssue>(),
        Some(&ProfileOpenIssue::CalendarTooLarge)
    );
    assert_eq!(std::fs::metadata(path).unwrap().len(), bytes);
}

#[test]
fn published_snapshot_changes_only_after_commit_and_rejects_oversize_without_mutation() {
    use godovoy_plan::storage::{ProfileSizeLimit, Store};

    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path().join("profile");
    let store = Store::open(&profile).unwrap();
    let reader = store.lock().unwrap().reader();
    let (staged, prepared) = std::sync::mpsc::channel();
    let (finish, continue_write) = std::sync::mpsc::channel();
    let (observed, access) = std::sync::mpsc::channel();
    let shared = store.clone();
    let writer = std::thread::spawn(move || {
        shared
            .lock()
            .unwrap()
            .transaction(|state| {
                state.create_workspace("Команда".into(), "Анна".into())?;
                staged.send(())?;
                continue_write.recv()?;
                Ok(())
            })
            .unwrap();
    });
    prepared
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    let other = reader.clone();
    let check = std::thread::spawn(move || observed.send(other.snapshot()).unwrap());
    let during = access.recv_timeout(std::time::Duration::from_secs(2));
    finish.send(()).unwrap();
    check.join().unwrap();
    writer.join().unwrap();
    assert!(during.unwrap().state.workspace.is_none());
    let committed = reader.snapshot();
    assert!(committed.state.workspace.is_some());
    let data_path = profile.join("calendar.json");
    let original = std::fs::read(&data_path).unwrap();
    assert_eq!(committed.usage.bytes, original.len());

    let excess = store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let mut event = template(state.device, date(2026, 12, 20));
            event.description = "я".repeat(4000);
            event.tasks.clear();
            let calendar = &mut state.workspace_mut()?.calendar;
            for key in 1_u128..=1200 {
                calendar
                    .plans
                    .entry(Id::from_u128(key))
                    .or_default()
                    .templates
                    .insert(2026, Record::new(revision, Some(event.clone().into())));
            }
            Ok(())
        })
        .unwrap_err();
    assert_eq!(excess.downcast_ref(), Some(&ProfileSizeLimit::File));
    let after = reader.snapshot();
    assert!(std::sync::Arc::ptr_eq(&committed.state, &after.state));
    assert_eq!(committed.usage, after.usage);
    assert_eq!(std::fs::read(&data_path).unwrap(), original);
}

#[test]
fn diary_update_shares_the_event_without_mutating_previous_snapshots() {
    use godovoy_plan::storage::Store;
    use std::sync::Arc;

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("profile")).unwrap();
    let mut store = store.lock().unwrap();
    let id = Id::new_v4();
    store
        .transaction(|state| {
            state.create_workspace("Команда".into(), "Анна".into())?;
            let revision = state.revision()?;
            let event = template(state.device, date(2026, 12, 20));
            state
                .workspace_mut()?
                .calendar
                .plans
                .entry(id)
                .or_default()
                .templates
                .insert(2026, Record::new(revision, Some(event.into())));
            Ok(())
        })
        .unwrap();
    let reader = store.reader();
    let before = reader.snapshot();
    store
        .transaction(|state| {
            let revision = state.revision()?;
            state
                .workspace_mut()?
                .calendar
                .plans
                .get_mut(&id)
                .unwrap()
                .years
                .entry(2026)
                .or_default()
                .note = Some(Record::new(revision, "Напоминание".into()));
            Ok(())
        })
        .unwrap();
    let after = reader.snapshot();
    let old = &before.state.workspace.as_ref().unwrap().calendar.plans[&id];
    let new = &after.state.workspace.as_ref().unwrap().calendar.plans[&id];
    assert!(Arc::ptr_eq(
        old.templates[&2026].value.as_ref().unwrap(),
        new.templates[&2026].value.as_ref().unwrap()
    ));
    assert_eq!(old.event(id, 2026).unwrap().note(), "");
    assert_eq!(new.event(id, 2026).unwrap().note(), "Напоминание");
}
