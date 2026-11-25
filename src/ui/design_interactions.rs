use super::*;

#[test]
fn revealing_december_keeps_preceding_events_in_the_annual_list() {
    let directory = tempfile::tempdir().unwrap();
    let (mut app, _, _) = copy_test_event(&directory.path().join("profile"));
    let usage = app.reader.snapshot().usage;
    let rows: Vec<_> = (1..=12)
        .map(|month| CalendarRow {
            id: Id::from_u128(u128::from(month)),
            date: NaiveDate::from_ymd_opt(2026, month, 1).unwrap(),
            title: format!("Событие {month}").into(),
            category: Category::Partners,
        })
        .collect();
    let ctx = egui::Context::default();
    crate::theme::setup(&ctx);
    for (target, neighbor) in [(11, 10), (0, 1)] {
        app.selected = Some(rows[target].id);
        app.reveal_event = true;
        let mut render = || {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    vec2(340.0, 220.0),
                )),
                ..Default::default()
            };
            ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    app.event_list(ui, &rows, rows[0].date, usage);
                });
            })
        };
        render();
        let painted = render();
        for index in [target, neighbor] {
            assert!(
                painted.shapes.iter().any(|shape| {
                    matches!(&shape.shape, egui::Shape::Text(text)
                    if text.galley.text() == rows[index].title.as_ref()
                        && shape.clip_rect.contains_rect(text.visual_bounding_rect()))
                }),
                "expected visible event {}",
                rows[index].title
            );
        }
    }
}

#[test]
fn restore_result_keeps_source_files_and_warning_separate_from_backup_status() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("profile")).unwrap();
    let mut app = App::new(store, Err("Без сети".into()), false);
    let original = directory
        .path()
        .join("calendar-before-restore-original.json");
    let in_memory = directory
        .path()
        .join("calendar-open-before-restore-original.json");
    app.report_restore(RestoreResult {
        original: Some(original.clone()),
        working_snapshot: Some(in_memory.clone()),
        restore_point: None,
        restore_point_warning: true,
    });
    assert!(app.backup_open);
    assert!(app.backup_status.is_none());
    let result = app.restore_result.as_ref().unwrap();
    assert_eq!(result.original.as_ref(), Some(&original));
    assert_eq!(result.working_snapshot.as_ref(), Some(&in_memory));
    assert!(result.restore_point_warning);
    assert!(result.restore_point.is_none());
}

#[test]
fn latest_backup_updates_after_creation_and_on_reopening_the_window() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let open = || {
        App::new(
            Store::open(&profile).unwrap(),
            Err("Без сети".into()),
            false,
        )
    };
    let mut app = open();
    app.store
        .lock()
        .unwrap()
        .transaction(|state| state.create_workspace("Команда".into(), "Анна".into()))
        .unwrap();
    app.open_backup_window();
    assert_eq!(app.latest_backup, Ok(None));
    app.create_profile_backup(&egui::Context::default());
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.backup_creation_busy() {
        assert!(Instant::now() < deadline, "Создание копии не завершилось");
        app.poll_backup_creation();
        thread::sleep(Duration::from_millis(1));
    }
    let copy = app.latest_backup.as_ref().unwrap().clone().unwrap();
    assert!(matches!(&app.backup_creation, BackupCreation::Finished {
        message, error: false, notice: false
    } if message == "Копия создана."));
    drop(app);

    let mut app = open();
    app.open_backup_window();
    assert_eq!(app.latest_backup, Ok(Some(copy)));
    assert_eq!(app.backup_status, None);
    let folder = Store::backup_folder(&profile);
    std::fs::rename(&folder, profile.join("backups-displaced")).unwrap();
    std::fs::write(&folder, "Вместо каталога").unwrap();
    app.open_backup_window();
    assert!(app.latest_backup.is_err());
    app.backup_open = false;
    app.finish_backup_creation(Err("Нет доступа к папке.".into()));
    assert!(matches!(
        &app.backup_creation,
        BackupCreation::Finished {
            error: true,
            notice: true,
            ..
        }
    ));
    app.open_backup_window();
    assert!(matches!(&app.backup_creation, BackupCreation::Finished {
        message, error: true, notice: false
    } if message.contains("Нет доступа к папке.")));
}

#[test]
fn new_event_starts_as_a_one_time_event_with_a_clean_draft() {
    let today = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
    let mut editor = Editor::new(2026, today);
    assert!(!editor.has_changes());
    assert!(!editor.event.yearly);
    assert_eq!(editor.event.preparation_days, 0);
    editor.event.title = "Поздравления партнёрам".into();
    assert!(editor.has_changes());
    assert_eq!(editor.validated().unwrap().1.date, today);
}

fn copy_test_event(profile: &std::path::Path) -> (App, Id, Id) {
    let store = godovoy_plan::storage::Store::open(profile).unwrap();
    let owner = store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.create_workspace("Команда".into(), "Анна".into())?;
            Ok(state.device)
        })
        .unwrap();
    let app = App::new(store, Err("Unused in UI test".into()), false);
    let date = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    let mut source = Editor::new(date.year(), date);
    source.event.title = "Шаблон события".into();
    source.event.description = "План подготовки".into();
    source.event.category = Category::Team;
    source.event.owner = Some(owner);
    source.event.yearly = true;
    source.event.preparation_days = 30;
    let task = Id::new_v4();
    source.event.tasks.push(Task {
        id: task,
        title: "Согласовать программу".into(),
        days_before: 14,
    });
    app.save_event(&source, false).unwrap();
    (app, source.id, task)
}

#[test]
fn repeated_task_and_note_preserve_calendar_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let (mut app, event, task) = copy_test_event(&directory.path().join("profile"));
    app.check(event, 2027, task, true);
    app.load_note((event, 2027), "");
    app.note = "Согласовано".into();
    app.save_note(event, 2027, "");
    let before = app.reader.snapshot().state;

    app.check(event, 2027, task, true);
    app.save_note(event, 2027, "Согласовано");
    let after = app.reader.snapshot().state;
    assert!(Arc::ptr_eq(&before, &after));
    let saved = after.follow_event(event, 2027).unwrap();
    assert!(saved.checked(task));
    assert_eq!(saved.note(), "Согласовано");
}

#[test]
fn event_copy_from_template_keeps_fields_but_starts_with_a_fresh_plan() {
    let directory = tempfile::tempdir().unwrap();
    let (mut app, source, source_task) = copy_test_event(&directory.path().join("profile"));
    let year = 2027;
    app.check(source, year, source_task, true);
    app.load_note((source, year), "");
    app.note = "Итоги прошлого события".into();
    app.save_note(source, year, "");
    app.store
        .lock()
        .unwrap()
        .transaction(|state| {
            state
                .workspace_mut()?
                .calendar
                .plans
                .get_mut(&source)
                .unwrap()
                .source = Some(godovoy_plan::model::ExternalSource {
                name: "example".into(),
                external_id: "source-plan".into(),
            });
            Ok(())
        })
        .unwrap();

    let before = app.reader.snapshot();
    let calendar = &before.state.workspace.as_ref().unwrap().calendar;
    let original = calendar.event(source, year).unwrap();
    let draft = Editor::from_template(original);
    assert!(draft.has_changes());
    assert!(draft.existing.is_none());
    assert_ne!(draft.id, source);
    assert_ne!(draft.event.tasks[0].id, source_task);
    assert_eq!(draft.parsed_date().unwrap(), original.date);
    assert!(Arc::ptr_eq(&before.state, &app.reader.snapshot().state));

    assert_eq!(app.save_event(&draft, false).unwrap(), year);
    let after = app.reader.snapshot();
    let updated = &after.state.workspace.as_ref().unwrap().calendar;
    let copy = updated.event(draft.id, year).unwrap();
    let mut expected = original.event.clone();
    expected.date = original.date;
    expected.tasks[0].id = draft.event.tasks[0].id;
    assert_eq!(copy.event, &expected);
    assert_eq!(copy.completed(), 0);
    assert!(copy.note().is_empty());
    assert!(updated.plans[&draft.id].source.is_none());
    assert_eq!(updated.plans[&source], calendar.plans[&source]);
}

#[test]
fn event_copy_from_restored_conflict_preserves_input_through_explicit_creation() {
    for cancelled in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        let (mut app, source, source_task) = copy_test_event(&profile);
        let year = 2027;
        let snapshot = app.reader.snapshot();
        let original = snapshot
            .state
            .workspace
            .as_ref()
            .unwrap()
            .calendar
            .event(source, year)
            .unwrap();
        let mut editor = Editor::edit(original);
        assert!(
            editor
                .original_issue(&snapshot.state.workspace.as_ref().unwrap().calendar)
                .is_none()
        );
        editor.event.description = "Моя правка".into();
        editor.date_input = "28.".into();
        app.editor = Some(editor);
        app.flush_local_drafts().unwrap();
        app.store
            .lock()
            .unwrap()
            .transaction(|state| {
                let revision = state.revision()?;
                let change = if cancelled {
                    YearChange::Cancel
                } else {
                    let mut updated = original.event.clone();
                    updated.title = "Правка коллеги".into();
                    YearChange::Edit(updated.into())
                };
                state
                    .workspace_mut()?
                    .calendar
                    .plans
                    .get_mut(&source)
                    .unwrap()
                    .years
                    .entry(year)
                    .or_default()
                    .change = Some(Record::new(revision, change));
                Ok(())
            })
            .unwrap();
        drop(app);

        let store = godovoy_plan::storage::Store::open(&profile).unwrap();
        let mut reopened = App::new(store, Err("Unused in UI test".into()), false);
        let mut stale = reopened.editor.take().unwrap();
        let before = reopened.reader.snapshot();
        let calendar = &before.state.workspace.as_ref().unwrap().calendar;
        assert!(stale.original_issue(calendar).is_some());
        let partial = std::mem::replace(&mut stale.date_input, "28.02.2027".into());
        assert!(reopened.save_event(&stale, false).is_err());
        stale.date_input = partial;

        let mut draft = stale.into_new_copy();
        assert_eq!(draft.date_input, "28.");
        assert_eq!(draft.event.date, original.date);
        assert_eq!(draft.event.description, "Моя правка");
        assert_ne!(draft.id, source);
        assert_ne!(draft.event.tasks[0].id, source_task);
        assert!(draft.has_changes());
        assert!(Arc::ptr_eq(
            &before.state,
            &reopened.reader.snapshot().state
        ));

        draft.date_input = "12.03.2027".into();
        assert_eq!(reopened.save_event(&draft, false).unwrap(), year);
        let after = reopened.reader.snapshot();
        let updated = &after.state.workspace.as_ref().unwrap().calendar;
        assert_eq!(updated.plans[&source], calendar.plans[&source]);
        assert_eq!(
            updated.event(draft.id, year).unwrap().event.date,
            NaiveDate::from_ymd_opt(year, 3, 12).unwrap()
        );
    }
}

#[test]
fn calendar_deadline_preserves_the_previous_year() {
    let event = NaiveDate::from_ymd_opt(2027, 1, 8).unwrap();
    let due = NaiveDate::from_ymd_opt(2026, 12, 21).unwrap();
    assert_eq!(editor_fields::days_before(event, due), 18);
    assert_eq!(deadline_label(due, event), "21 декабря 2026");
    assert_eq!(deadline_label(event, event), "8 января");
}

#[test]
fn note_draft_survives_switching_events() {
    let directory = tempfile::tempdir().unwrap();
    let store = godovoy_plan::storage::Store::open(&directory.path().join("profile")).unwrap();
    let mut app = App::new(store, Err("Unused in UI test".into()), false);
    let first = (Id::new_v4(), 2026);
    let second = (Id::new_v4(), 2026);
    app.load_note(first, "Изначальный текст");
    app.note = "Изменённый черновик".into();
    app.load_note(second, "Другое событие");
    assert_eq!(app.note, "Другое событие");
    app.load_note(first, "Изначальный текст");
    assert_eq!(app.note, "Изменённый черновик");
    assert_eq!(app.note_baseline, "Изначальный текст");

    app.load_note(second, "Другое событие");
    app.load_note(first, "Изменённый черновик");
    assert!(!app.has_unsaved_notes());
}

#[test]
fn note_merge_preserves_the_draft_when_the_saved_text_changes_again() {
    let directory = tempfile::tempdir().unwrap();
    let store = godovoy_plan::storage::Store::open(&directory.path().join("profile")).unwrap();
    store
        .lock()
        .unwrap()
        .transaction(|state| state.create_workspace("Команда".into(), "Анна".into()))
        .unwrap();
    let mut app = App::new(Arc::clone(&store), Err("Unused in UI test".into()), false);
    let date = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
    let mut editor = Editor::new(date.year(), date);
    editor.event.title = "Событие".into();
    let year = app.save_event(&editor, false).unwrap();
    let id = editor.id;
    let key = (id, year);
    let update_saved = |text: &str| {
        store
            .lock()
            .unwrap()
            .transaction(|state| {
                let revision = state.revision()?;
                let calendar = &mut state.workspace_mut()?.calendar;
                calendar
                    .plans
                    .get_mut(&id)
                    .unwrap()
                    .years
                    .entry(year)
                    .or_default()
                    .note = Some(Record::new(revision, text.to_owned()));
                Ok(())
            })
            .unwrap();
    };
    let saved = || {
        store
            .lock()
            .unwrap()
            .state()
            .workspace
            .as_ref()
            .unwrap()
            .calendar
            .event(id, year)
            .unwrap()
            .note()
            .to_owned()
    };

    app.load_note(key, "");
    app.note = "Мой черновик".into();
    update_saved("Правка коллеги");
    app.load_note(key, "Правка коллеги");
    app.save_note(id, year, "Правка коллеги");
    assert_eq!(saved(), "Правка коллеги");
    assert_eq!(app.note, "Мой черновик");

    app.begin_note_merge("Правка коллеги");
    app.note = "Общий итог".into();
    update_saved("Новая версия");
    app.save_note(id, year, "Правка коллеги");
    assert_eq!(saved(), "Новая версия");
    assert_eq!(app.note, "Общий итог");
    assert!(app.has_unsaved_notes());

    app.load_note(key, "Новая версия");
    app.begin_note_merge("Новая версия");
    app.save_note(id, year, "Новая версия");
    assert_eq!(saved(), "Общий итог");
    assert!(!app.note_merging);
    assert!(!app.has_unsaved_notes());

    app.note = "Новый черновик".into();
    update_saved("Хранимый вариант");
    app.take_saved_note("Хранимый вариант");
    assert_eq!(saved(), "Хранимый вариант");
    assert_eq!(app.note, "Хранимый вариант");
    assert!(!app.has_unsaved_notes());
}

#[test]
fn calendar_cache_tracks_filters_year_and_profile_updates() {
    use godovoy_plan::storage::Store;

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("profile")).unwrap();
    let event = Id::new_v4();
    store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.create_workspace("Команда".into(), "Анна".into())?;
            let revision = state.revision()?;
            state
                .workspace_mut()?
                .calendar
                .plans
                .entry(event)
                .or_default()
                .templates
                .insert(
                    2026,
                    Record::new(
                        revision,
                        Some(Arc::new(Event {
                            title: "Поздравления партнёрам".into(),
                            description: String::new(),
                            category: Category::Partners,
                            date: NaiveDate::from_ymd_opt(2026, 12, 20).unwrap(),
                            yearly: false,
                            preparation_days: 0,
                            owner: None,
                            tasks: Vec::new(),
                        })),
                    ),
                );
            Ok(())
        })
        .unwrap();

    let mut app = App::new(Arc::clone(&store), Err("Unused in UI test".into()), false);
    app.year = 2026;
    app.search = "ПАРТНЕРАМ".into();
    let state = store.lock().unwrap().snapshot();
    let workspace = state.workspace.as_ref().unwrap();
    let original = app.calendar_rows(workspace);
    assert_eq!(original[0].id, event);
    store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.last_sync = Some(123);
            Ok(())
        })
        .unwrap();
    let stamped = store.lock().unwrap().snapshot();
    assert!(Arc::ptr_eq(
        &original,
        &app.calendar_rows(stamped.workspace.as_ref().unwrap())
    ));
    app.search = " партнёрам ".into();
    assert!(Arc::ptr_eq(&original, &app.calendar_rows(workspace)));
    app.search.push_str(" отсутствует");
    assert!(app.calendar_rows(workspace).is_empty());
    app.search = "ёрам".into();
    assert_eq!(app.calendar_rows(workspace)[0].id, event);
    app.category = Some(Category::Team);
    assert!(app.calendar_rows(workspace).is_empty());
    app.category = None;
    app.year = 2027;
    assert!(app.calendar_rows(workspace).is_empty());

    app.year = 2026;
    assert_eq!(app.calendar_rows(workspace).len(), 1);
    store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.workspace_mut()?.calendar.plans.clear();
            Ok(())
        })
        .unwrap();
    let updated = store.lock().unwrap().snapshot();
    let workspace = updated.workspace.as_ref().unwrap();
    assert!(app.calendar_rows(workspace).is_empty());
}

#[test]
fn personal_cache_releases_stale_state_and_receipts_before_another_frame() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("profile")).unwrap();
    let mut app = App::new(store.clone(), Err("Без сети".into()), false);
    store
        .lock()
        .unwrap()
        .transaction(|state| state.create_workspace("Первый снимок".into(), "Ира".into()))
        .unwrap();
    let today = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
    let state = app.reader.snapshot().state;
    let marks = Arc::new(BTreeSet::new());
    let stale_state = Arc::downgrade(&state);
    let stale_marks = Arc::downgrade(&marks);
    app.personal_view(&state, &marks, today);
    let previous_rows = app.personal.as_ref().unwrap().pending.clone();
    app.personal_view(&state, &marks, today);
    assert!(Arc::ptr_eq(
        &previous_rows,
        &app.personal.as_ref().unwrap().pending
    ));

    drop(state);
    drop(marks);
    store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.workspace_mut()?.name = "Обновлённый снимок".into();
            Ok(())
        })
        .unwrap();
    assert!(stale_state.upgrade().is_none());
    assert!(stale_marks.upgrade().is_none());

    let current = app.reader.snapshot();
    app.personal_view(&current.state, &current.reminder_receipts, today);
    assert!(!Arc::ptr_eq(
        &previous_rows,
        &app.personal.as_ref().unwrap().pending
    ));
}

#[test]
fn personal_task_rows_preserve_groups_order_and_open_status() {
    let year = 2026;
    let date = NaiveDate::from_ymd_opt(year, 12, 20).unwrap();
    let event = Id::new_v4();
    let empty = Id::new_v4();
    let revision = Revision {
        sequence: 1,
        operation: Id::new_v4(),
    };
    let mut calendar = Calendar::default();
    let mut base = Event {
        title: "Подготовка".into(),
        date,
        category: Category::Team,
        description: String::new(),
        yearly: true,
        preparation_days: 30,
        owner: None,
        tasks: Vec::new(),
    };
    calendar
        .plans
        .entry(empty)
        .or_default()
        .templates
        .insert(year, Record::new(revision, Some(base.clone().into())));
    base.tasks = [1, 20, 25, 20]
        .into_iter()
        .map(|days_before| Task {
            id: Id::new_v4(),
            title: format!("Пункт {days_before}"),
            days_before,
        })
        .collect();
    let checked = base.tasks[2].id;
    let plan = calendar.plans.entry(event).or_default();
    plan.templates
        .insert(year, Record::new(revision, Some(base.into())));
    plan.years
        .entry(year)
        .or_default()
        .checks
        .insert(checked, Record::new(revision, true));

    let rows = task_rows(
        &calendar,
        &[(event, year), (empty, year)],
        3,
        date,
        TaskFilter::All,
    );
    assert_eq!(
        rows.iter().map(|row| row.task).collect::<Vec<_>>(),
        vec![None, Some(1), Some(3), Some(0), None],
    );
    assert!(
        rows[..4]
            .iter()
            .all(|row| row.event == event && row.year == year)
    );
    assert_eq!(rows[4].event, empty);
}

#[test]
fn personal_due_filters_follow_checklist_dates_across_new_year() {
    let today = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
    let year = 2027;
    let event = Id::new_v4();
    let empty = Id::new_v4();
    let owner = Id::new_v4();
    let revision = Revision {
        sequence: 1,
        operation: owner,
    };
    let mut calendar = Calendar::default();
    let mut plan = Event {
        title: "Январская встреча".into(),
        date: NaiveDate::from_ymd_opt(year, 1, 3).unwrap(),
        category: Category::Team,
        description: String::new(),
        yearly: false,
        preparation_days: 30,
        owner: Some(owner),
        tasks: Vec::new(),
    };
    calendar
        .plans
        .entry(empty)
        .or_default()
        .templates
        .insert(year, Record::new(revision, Some(plan.clone().into())));
    plan.tasks = [0, 1, 2, 3, 4, 5, 3]
        .into_iter()
        .map(|days_before| Task {
            id: Id::new_v4(),
            title: format!("Пункт за {days_before} дней"),
            days_before,
        })
        .collect();
    let completed = [plan.tasks[5].id, plan.tasks[6].id];
    let record = calendar.plans.entry(event).or_default();
    record
        .templates
        .insert(year, Record::new(revision, Some(plan.into())));
    record.years.entry(year).or_default().checks.extend(
        completed
            .into_iter()
            .map(|id| (id, Record::new(revision, true))),
    );

    let events: Vec<_> = planning::personal_events(&calendar, today, owner)
        .into_iter()
        .map(|event| (event.id, event.year))
        .collect();
    assert_eq!(events.len(), 2);
    let all = task_rows(&calendar, &events, 5, today, TaskFilter::All);
    assert_eq!(all.len(), 7);
    assert!(
        all.iter()
            .any(|row| row.event == empty && row.task.is_none())
    );
    for (filter, indices) in [
        (TaskFilter::TodayAndTomorrow, vec![None, Some(3), Some(2)]),
        (TaskFilter::Overdue, vec![None, Some(4)]),
    ] {
        let rows = task_rows(&calendar, &events, 5, today, filter);
        assert_eq!(rows.iter().map(|row| row.task).collect::<Vec<_>>(), indices);
        assert!(
            rows.iter()
                .all(|row| row.event == event && row.year == year)
        );
    }
    let before_any_deadline = NaiveDate::from_ymd_opt(2026, 12, 27).unwrap();
    assert!(
        task_rows(
            &calendar,
            &events,
            5,
            before_any_deadline,
            TaskFilter::Overdue
        )
        .is_empty()
    );
    assert!(
        task_rows(
            &calendar,
            &events,
            5,
            before_any_deadline,
            TaskFilter::TodayAndTomorrow
        )
        .is_empty()
    );
}

#[test]
fn search_matches_capital_letters_and_yo() {
    assert!(search_key("Поздравления партнёрам").contains(&search_key("  ПАРТНЕРАМ ")));
}

#[test]
fn compact_calendar_keeps_the_list_and_sync_navigation_visible() {
    let profile = tempfile::tempdir().unwrap();
    let (mut app, _, _) = copy_test_event(&profile.path().join("profile"));
    app.year = 2028;
    app.network = DesktopTransport::Started(Err("Без сети".into()));
    for size in [vec2(819.0, 574.0), vec2(960.0, 600.0), vec2(1024.0, 640.0)] {
        let ctx = egui::Context::default();
        crate::theme::setup(&ctx);
        let mut frame = eframe::Frame::_new_kittest();
        let mut shapes = Vec::new();
        for _ in 0..4 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                ..Default::default()
            };
            shapes = ctx
                .run_ui(input, |ui| eframe::App::ui(&mut app, ui, &mut frame))
                .shapes;
        }
        let label = |prefix: &str| {
            shapes
                .iter()
                .find_map(|shape| {
                    let egui::Shape::Text(text) = &shape.shape else {
                        return None;
                    };
                    let bounds = text.visual_bounding_rect();
                    (text.galley.text().starts_with(prefix)
                        && shape.clip_rect.contains_rect(bounds))
                    .then_some(bounds)
                })
                .unwrap_or_else(|| panic!("Надпись «{prefix}» недоступна при {size:?}"))
        };
        assert!(label("Напоминания").right() < label("Синхронизация").left());
        assert!(label("События за год").left() > label("Годовой круг").right());
        assert!(label("События за год").bottom() < size.y - 46.0);
    }
}

#[test]
fn a_task_deadline_reveals_its_checkbox_without_marking_it_complete() {
    let directory = tempfile::tempdir().unwrap();
    let (mut app, _, _) = copy_test_event(&directory.path().join("profile"));
    let date = NaiveDate::from_ymd_opt(2028, 4, 12).unwrap();
    let mut editor = Editor::new(date.year(), date);
    editor.event.title = "План подготовки".into();
    for number in 1..=40 {
        editor.event.tasks.push(Task {
            id: Id::new_v4(),
            title: format!("Пункт {number}"),
            days_before: 0,
        });
    }
    app.save_event(&editor, false).unwrap();
    let target = &editor.event.tasks[34];
    let before = app.reader.snapshot().state;
    let row = AgendaItem {
        year: date.year(),
        event: editor.id,
        kind: Milestone::Task(target.id),
        date,
    };

    app.open_milestone(row);
    app.open_event(date.year(), editor.id);
    assert!(app.reveal_task.is_none());
    app.open_milestone(row);
    let ctx = egui::Context::default();
    crate::theme::setup(&ctx);
    let mut frame = eframe::Frame::_new_kittest();
    let mut shapes = Vec::new();
    for tick in 0..40 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                vec2(960.0, 600.0),
            )),
            time: Some(f64::from(tick) / 60.0),
            ..Default::default()
        };
        shapes = ctx
            .run_ui(input, |ui| eframe::App::ui(&mut app, ui, &mut frame))
            .shapes;
    }

    let focused = ctx.memory(|memory| memory.focused()).unwrap();
    let checkbox = ctx.read_response(focused).unwrap();
    assert!(shapes.iter().any(|shape| {
        let egui::Shape::Text(text) = &shape.shape else {
            return false;
        };
        let bounds = text.visual_bounding_rect();
        text.galley.text() == target.title
            && shape.clip_rect.contains_rect(bounds)
            && checkbox.rect.contains(bounds.center())
    }));
    assert!(app.reveal_task.is_none());
    assert!(Arc::ptr_eq(&before, &app.reader.snapshot().state));
}

#[test]
fn agenda_exposes_one_button_across_the_task_row() {
    let directory = tempfile::tempdir().unwrap();
    let (mut app, _, _) = copy_test_event(&directory.path().join("profile"));
    let today = Local::now().date_naive();
    let mut editor = Editor::new(today.year(), today);
    editor.event.title = "Годовая выставка".into();
    editor.event.owner = Some(app.reader.snapshot().state.device);
    editor.event.tasks.push(Task {
        id: Id::new_v4(),
        title: "Согласовать поставку".into(),
        days_before: 0,
    });
    app.save_event(&editor, false).unwrap();

    app.page = Page::Agenda;
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    crate::theme::setup(&ctx);
    let mut frame = eframe::Frame::_new_kittest();
    let mut render = |events| {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                vec2(960.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        ctx.run_ui(input, |ui| eframe::App::ui(&mut app, ui, &mut frame))
    };
    render(Vec::new());
    let tree = render(Vec::new()).platform_output.accesskit_update.unwrap();
    let mut items = tree.nodes.iter().filter(|(_, node)| {
        node.label()
            .is_some_and(|label| label.contains("Согласовать поставку"))
    });
    let (_, button) = items.next().unwrap();
    assert!(items.next().is_none());
    assert_eq!(button.role(), egui::accesskit::Role::Button);
    assert!(button.children().is_empty());
    assert!(button.label().unwrap().contains("Анна"));
    let rect = button.bounds().unwrap();
    assert!(rect.width() > 800.0);
    let point = egui::pos2((rect.x1 - 2.0) as f32, ((rect.y0 + rect.y1) * 0.5) as f32);
    render(vec![egui::Event::PointerMoved(point)]);
    for pressed in [true, false] {
        render(vec![egui::Event::PointerButton {
            pos: point,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }]);
    }
    assert!(app.page == Page::Calendar);
    assert_eq!(app.selected, Some(editor.id));
    assert_eq!(
        app.reveal_task.unwrap().kind,
        Milestone::Task(editor.event.tasks[0].id)
    );
}

#[test]
fn searching_after_a_title_edit_uses_the_current_occurrence_and_unicode_casing() {
    let directory = tempfile::tempdir().unwrap();
    let (mut app, id, _) = copy_test_event(&directory.path().join("profile"));
    app.year = 2027;
    let rename = |app: &App, title: &str| {
        let state = app.reader.snapshot().state;
        let mut editor = Editor::edit(state.follow_event(id, 2027).unwrap());
        editor.scope = Scope::ThisYear;
        editor.event.title = title.into();
        app.save_event(&editor, false).unwrap();
    };
    let find = |app: &mut App, query: &str| {
        app.search = query.into();
        let state = app.reader.snapshot().state;
        app.calendar_rows(state.workspace.as_ref().unwrap())
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>()
    };

    let indexed = |app: &App| app.calendar_view.as_ref().unwrap().normalized_titles.is_some();
    rename(&app, "Отчёт ΟΣ İ");
    assert_eq!(find(&mut app, "ОТ"), [id]);
    assert_eq!(find(&mut app, " ОТЧЕТ "), [id]);
    assert_eq!(find(&mut app, "ος"), [id]);
    assert_eq!(find(&mut app, "i\u{307}"), [id]);
    assert!(indexed(&app));
    let ctx = egui::Context::default();
    crate::theme::setup(&ctx);
    let mut frame = eframe::Frame::_new_kittest();
    let mut render = |app: &mut App, events| {
        let input = egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                vec2(1100.0, 760.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| <App as eframe::App>::ui(app, ui, &mut frame));
    };
    app.search_focus = true;
    render(&mut app, Vec::new());
    render(
        &mut app,
        vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    assert!(!indexed(&app));
    assert_eq!(find(&mut app, "i"), [id]);
    assert!(indexed(&app));
    app.page = Page::Tasks;
    render(&mut app, Vec::new());
    assert!(!indexed(&app));
    assert_eq!(app.search, "i");
    rename(&app, "Обновлённое название");
    assert!(find(&mut app, "i\u{307}").is_empty());
    assert!(find(&mut app, "отчет").is_empty());
    assert_eq!(find(&mut app, "обновленное"), [id]);
    app.year = 2028;
    assert!(find(&mut app, "обновленное").is_empty());
    assert_eq!(find(&mut app, "шаблон"), [id]);
}
