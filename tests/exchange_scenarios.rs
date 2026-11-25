use std::{path::Path, sync::Arc};

use chrono::NaiveDate;
use godovoy_plan::{
    exchange::{export_file, import_file},
    model::{Calendar, Category, Event, Id, Record, Task, YearChange},
    storage::{LocalState, SharedStore, Store},
};

const YEAR: i32 = 2026;
const PLAN: Id = Id::from_u128(1);
const OTHER: Id = Id::from_u128(2);
const ORDER: Id = Id::from_u128(10);
const ADDRESS: Id = Id::from_u128(11);

fn event(title: &str, year: i32) -> Arc<Event> {
    Arc::new(Event {
        title: title.into(),
        description: String::new(),
        date: NaiveDate::from_ymd_opt(year, 12, 15).unwrap(),
        category: Category::Partners,
        yearly: true,
        preparation_days: 30,
        owner: None,
        tasks: [(ORDER, "Заказать открытки"), (ADDRESS, "Сверить адреса")]
            .map(|(id, title)| Task {
                id,
                title: title.into(),
                days_before: 15,
            })
            .to_vec(),
    })
}

fn edit(profile: &SharedStore, write: impl FnOnce(&mut LocalState)) {
    profile
        .lock()
        .unwrap()
        .transaction(|state| {
            write(state);
            Ok(())
        })
        .unwrap();
}

fn calendar(profile: &SharedStore) -> Calendar {
    profile
        .lock()
        .unwrap()
        .state()
        .workspace
        .as_ref()
        .unwrap()
        .calendar
        .clone()
}

#[derive(Clone, Copy)]
enum Action {
    Note(i32, &'static str),
    Check(i32, Id, bool),
    Template(i32, &'static str),
    Retire(i32),
    Cancel(i32),
    Reinstate(i32),
    Override(i32, &'static str),
    Create(Id, &'static str),
    RenameMember(&'static str),
}

impl Action {
    fn apply(self, profile: &SharedStore) {
        edit(profile, |state| {
            let revision = state.revision().unwrap();
            let author = state.device;
            let calendar = &mut state.workspace_mut().unwrap().calendar;
            let plan = calendar.plans.get_mut(&PLAN).unwrap();
            match self {
                Self::Note(year, content) => {
                    plan.years.entry(year).or_default().note =
                        Some(Record::new(revision, content.into()));
                }
                Self::Check(year, id, done) => {
                    plan.years
                        .entry(year)
                        .or_default()
                        .checks
                        .insert(id, Record::new(revision, done));
                }
                Self::Template(year, title) => {
                    plan.templates
                        .insert(year, Record::new(revision, Some(event(title, year))));
                }
                Self::Retire(year) => {
                    plan.templates.insert(year, Record::new(revision, None));
                }
                Self::Override(year, title) => {
                    plan.years.entry(year).or_default().change =
                        Some(Record::new(revision, YearChange::Edit(event(title, year))));
                }
                Self::Cancel(year) => {
                    plan.years.entry(year).or_default().change =
                        Some(Record::new(revision, YearChange::Cancel));
                }
                Self::Reinstate(year) => {
                    plan.years.entry(year).or_default().change =
                        Some(Record::new(revision, YearChange::UseTemplate));
                }
                Self::Create(id, title) => {
                    calendar
                        .plans
                        .entry(id)
                        .or_default()
                        .templates
                        .insert(YEAR, Record::new(revision, Some(event(title, YEAR))));
                }
                Self::RenameMember(name) => {
                    calendar
                        .members
                        .insert(author, Record::new(revision, name.into()));
                }
            }
        });
    }
}

#[derive(Debug, Clone, Copy)]
enum Route {
    CrossedFiles,
    LeftThenRight,
    RightThenLeft,
    LateOldFile,
    ThirdColleague,
    ReopenMidExchange,
}

const ROUTES: [Route; 6] = [
    Route::CrossedFiles,
    Route::LeftThenRight,
    Route::RightThenLeft,
    Route::LateOldFile,
    Route::ThirdColleague,
    Route::ReopenMidExchange,
];

fn reopen(store: SharedStore, path: &Path) -> SharedStore {
    drop(store);
    Store::open_existing(path).unwrap()
}

fn matrix(
    name: &str,
    initial: &[Action],
    left_edits: &[Action],
    right_edits: &[Action],
    verify: impl Fn(&Calendar),
) {
    for route in ROUTES {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let left = Store::open(&root.join("left")).unwrap();
        let mut right = Store::open(&root.join("right")).unwrap();
        let relay = Store::open(&root.join("relay")).unwrap();
        edit(&left, |state| {
            state
                .create_workspace("Команда".into(), "Анна".into())
                .unwrap();
            let revision = state.revision().unwrap();
            let calendar = &mut state.workspace_mut().unwrap().calendar;
            for (store, name) in [(&right, "Борис"), (&relay, "Вера")] {
                let device = store.lock().unwrap().state().device;
                calendar
                    .members
                    .insert(device, Record::new(revision, name.into()));
            }
            calendar.plans.entry(PLAN).or_default().templates.insert(
                YEAR - 1,
                Record::new(revision, Some(event("Партнёры", YEAR - 1))),
            );
        });
        for action in initial {
            action.apply(&left);
        }
        let base = left.lock().unwrap().state().workspace.clone();
        for store in [&right, &relay] {
            edit(store, |state| state.workspace.clone_from(&base));
        }
        let baseline = root.join("baseline.yrplan");
        export_file(&left, &baseline).unwrap();
        for action in left_edits {
            action.apply(&left);
        }
        for action in right_edits {
            action.apply(&right);
        }
        let a = root.join("a.yrplan");
        let b = root.join("b.yrplan");
        let merged_file = root.join("merged.yrplan");
        export_file(&left, &a).unwrap();
        export_file(&right, &b).unwrap();
        match route {
            Route::CrossedFiles | Route::LateOldFile => {
                import_file(&left, &b).unwrap();
                import_file(&right, &a).unwrap();
                if matches!(route, Route::LateOldFile) {
                    for store in [&left, &right] {
                        assert!(!import_file(store, &baseline).unwrap());
                    }
                }
            }
            Route::LeftThenRight | Route::ReopenMidExchange => {
                import_file(&right, &a).unwrap();
                if matches!(route, Route::ReopenMidExchange) {
                    right = reopen(right, &root.join("right"));
                }
                export_file(&right, &merged_file).unwrap();
                import_file(&left, &merged_file).unwrap();
            }
            Route::RightThenLeft => {
                import_file(&left, &b).unwrap();
                export_file(&left, &merged_file).unwrap();
                import_file(&right, &merged_file).unwrap();
            }
            Route::ThirdColleague => {
                import_file(&relay, &b).unwrap();
                import_file(&relay, &a).unwrap();
                verify(&calendar(&relay));
                export_file(&relay, &merged_file).unwrap();
                for store in [&left, &right] {
                    import_file(store, &merged_file).unwrap();
                }
            }
        }
        let left = reopen(left, &root.join("left"));
        let right = reopen(right, &root.join("right"));
        let outcome = calendar(&left);
        assert_eq!(outcome, calendar(&right), "{name} / {route:?}");
        verify(&outcome);
        for store in [&left, &right] {
            for old in [&a, &b] {
                assert!(!import_file(store, old).unwrap(), "{name} / {route:?}");
            }
            assert_eq!(outcome, calendar(store));
        }
        println!("CASE PASS {name} / {route:?}");
    }
}

use Action::*;

#[test]
fn checklist_and_diary() {
    matrix(
        "checklist_and_diary",
        &[],
        &[Check(YEAR, ORDER, true)],
        &[Note(YEAR, "Адреса получены")],
        |c| {
            let item = c.event(PLAN, YEAR).unwrap();
            assert!(item.checked(ORDER));
            assert_eq!(item.note(), "Адреса получены");
        },
    );
}

#[test]
fn different_checklist_items() {
    matrix(
        "different_checklist_items",
        &[],
        &[Check(YEAR, ORDER, true)],
        &[Check(YEAR, ADDRESS, true)],
        |c| {
            assert_eq!(c.event(PLAN, YEAR).unwrap().completed(), 2);
            assert_eq!(c.event(PLAN, YEAR + 1).unwrap().completed(), 0);
        },
    );
}

#[test]
fn competing_diary_edits() {
    // The second edit on the right has a higher logical revision than the left edit.
    matrix(
        "competing_diary_edits",
        &[],
        &[Note(YEAR, "Первая смета")],
        &[Note(YEAR, "Черновик"), Note(YEAR, "Смета принята")],
        |c| {
            assert_eq!(c.event(PLAN, YEAR).unwrap().note(), "Смета принята");
        },
    );
}

#[test]
fn cancel_once_and_continue_next_year() {
    matrix(
        "cancel_once_and_continue_next_year",
        &[],
        &[Cancel(YEAR)],
        &[Note(YEAR + 1, "Новый тираж")],
        |c| {
            assert!(c.event(PLAN, YEAR).is_none());
            assert_eq!(c.event(PLAN, YEAR + 1).unwrap().note(), "Новый тираж");
            assert!(c.event(PLAN, YEAR - 1).is_some());
        },
    );
}

#[test]
fn retire_series_preserving_year_in_progress() {
    matrix(
        "retire_series_preserving_year_in_progress",
        &[],
        &[Retire(YEAR + 1)],
        &[Check(YEAR, ORDER, true)],
        |c| {
            assert!(c.event(PLAN, YEAR).unwrap().checked(ORDER));
            assert!(c.event(PLAN, YEAR + 1).is_none());
            assert!(c.event(PLAN, YEAR + 2).is_none());
        },
    );
}

#[test]
fn update_series_and_override_this_year() {
    matrix(
        "update_series_and_override_this_year",
        &[],
        &[Template(YEAR, "Новогодняя акция")],
        &[Override(YEAR, "Юбилейный выпуск")],
        |c| {
            assert_eq!(c.event(PLAN, YEAR - 1).unwrap().event.title, "Партнёры");
            assert_eq!(c.event(PLAN, YEAR).unwrap().event.title, "Юбилейный выпуск");
            assert_eq!(
                c.event(PLAN, YEAR + 1).unwrap().event.title,
                "Новогодняя акция"
            );
        },
    );
}

#[test]
fn restore_cancelled_year_and_amend_next_year() {
    matrix(
        "restore_cancelled_year_and_amend_next_year",
        &[Cancel(YEAR)],
        &[Reinstate(YEAR)],
        &[Override(YEAR + 1, "Новое место")],
        |c| {
            assert_eq!(c.event(PLAN, YEAR).unwrap().event.title, "Партнёры");
            assert_eq!(c.event(PLAN, YEAR + 1).unwrap().event.title, "Новое место");
        },
    );
}

#[test]
fn reopen_task_and_clear_old_note() {
    matrix(
        "reopen_task_and_clear_old_note",
        &[Check(YEAR, ORDER, true), Note(YEAR, "Готово")],
        &[Check(YEAR, ORDER, false)],
        &[Note(YEAR, "")],
        |c| {
            let item = c.event(PLAN, YEAR).unwrap();
            assert!(!item.checked(ORDER));
            assert_eq!(item.note(), "");
        },
    );
}

#[test]
fn create_second_event_and_complete_first() {
    matrix(
        "create_second_event_and_complete_first",
        &[],
        &[Create(OTHER, "Форум")],
        &[Check(YEAR, ORDER, true)],
        |c| {
            assert_eq!(c.plans.len(), 2);
            assert!(c.event(PLAN, YEAR).unwrap().checked(ORDER));
            assert_eq!(c.event(OTHER, YEAR).unwrap().event.title, "Форум");
            assert_eq!(c.event(OTHER, YEAR).unwrap().completed(), 0);
        },
    );
}

#[test]
fn rename_colleague_and_reschedule_series_title() {
    matrix(
        "rename_colleague_and_reschedule_series_title",
        &[],
        &[RenameMember("Анна Ивановна")],
        &[Template(YEAR + 1, "Новая программа")],
        |c| {
            let names: Vec<_> = c
                .members
                .values()
                .map(|record| record.value.as_str())
                .collect();
            assert_eq!(names.len(), 3);
            assert!(names.contains(&"Анна Ивановна"));
            assert!(!names.contains(&"Анна"));
            assert_eq!(c.event(PLAN, YEAR).unwrap().event.title, "Партнёры");
            assert_eq!(
                c.event(PLAN, YEAR + 1).unwrap().event.title,
                "Новая программа"
            );
        },
    );
}

#[test]
fn offline_file_followed_by_live_sync_and_stale_file() {
    use std::{
        net::{Ipv4Addr, SocketAddrV4},
        thread,
        time::{Duration, Instant},
    };

    use godovoy_plan::network::{Config, Network};

    let root = tempfile::tempdir().unwrap();
    let left_path = root.path().join("left");
    let right_path = root.path().join("right");
    let left = Store::open(&left_path).unwrap();
    let right = Store::open(&right_path).unwrap();
    edit(&left, |state| {
        state
            .create_workspace("Филиал".into(), "Анна".into())
            .unwrap();
        let revision = state.revision().unwrap();
        state
            .workspace_mut()
            .unwrap()
            .calendar
            .plans
            .entry(PLAN)
            .or_default()
            .templates
            .insert(
                YEAR,
                Record::new(revision, Some(event("Почтовая отправка", YEAR))),
            );
    });

    let start = |store: &SharedStore| {
        Network::start(
            store.clone(),
            Config {
                listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
                discovery_port: None,
            },
        )
        .unwrap()
    };
    let host = start(&left);
    let colleague = start(&right);
    colleague
        .join_async(
            host.make_invite(&colleague.pairing_request()).unwrap(),
            "Борис".into(),
            host.address(),
        )
        .recv_timeout(Duration::from_secs(12))
        .unwrap()
        .unwrap();
    host.set_profile_paused(true).unwrap();
    colleague.set_profile_paused(true).unwrap();

    Check(YEAR, ORDER, true).apply(&left);
    Note(YEAR, "Адреса подтверждены").apply(&right);
    let packet = root.path().join("week-one.yrplan");
    export_file(&right, &packet).unwrap();
    assert!(import_file(&left, &packet).unwrap());
    assert_eq!(calendar(&left).event(PLAN, YEAR).unwrap().completed(), 1);
    assert_eq!(calendar(&right).event(PLAN, YEAR).unwrap().completed(), 0);
    Note(YEAR, "Письма отправлены").apply(&right);
    Check(YEAR, ADDRESS, true).apply(&left);

    host.set_profile_paused(false).unwrap();
    colleague.set_profile_paused(false).unwrap();
    let limit = Instant::now() + Duration::from_secs(15);
    loop {
        let ready = [&left, &right].iter().all(|store| {
            let c = calendar(store);
            let item = c.event(PLAN, YEAR).unwrap();
            item.completed() == 2 && item.note() == "Письма отправлены"
        });
        if ready {
            break;
        }
        assert!(
            Instant::now() < limit,
            "TCP peers did not converge within 15 seconds"
        );
        thread::sleep(Duration::from_millis(50));
    }
    drop(host);
    drop(colleague);
    let left = reopen(left, &left_path);
    let right = reopen(right, &right_path);
    let expected = calendar(&left);
    assert_eq!(calendar(&right), expected);
    for store in [&left, &right] {
        assert!(!import_file(store, &packet).unwrap());
        assert_eq!(calendar(store), expected);
    }
    println!("CASE PASS offline_file_followed_by_live_sync_and_stale_file / LoopbackTcp");
}
