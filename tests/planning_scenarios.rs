use chrono::{Datelike, Duration, NaiveDate};
use godovoy_plan::model::{Calendar, Category, Event, Id, Record, Revision, Task, YearChange};
use godovoy_plan::planning::{self, Horizon, Milestone, OwnerFilter, Reminder};

const PLAN: Id = Id::from_u128(1);
const OWNER: Id = Id::from_u128(2);
const COLLEAGUE: Id = Id::from_u128(3);
const EARLY: Id = Id::from_u128(4);
const LATE: Id = Id::from_u128(5);

fn date(value: &str) -> NaiveDate {
    value.parse().unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Schedule {
    Annual,
    Once,
    MovedThisYear,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Progress {
    Open,
    EarlyDone,
    LateDone,
    Complete,
    NoChecklist,
}

impl Progress {
    fn done(self, id: Id) -> bool {
        self == Self::Complete
            || (self == Self::EarlyDone && id == EARLY)
            || (self == Self::LateDone && id == LATE)
    }
}

struct Scenario {
    template: NaiveDate,
    target: NaiveDate,
    preparation: NaiveDate,
    early: NaiveDate,
    late: NaiveDate,
    offsets: [u16; 3],
    schedule: Schedule,
}

impl Scenario {
    fn new(dates: [&str; 5], offsets: [u16; 3], schedule: Schedule) -> Self {
        let [template, target, preparation, early, late] = dates.map(date);
        Self {
            template,
            target,
            preparation,
            early,
            late,
            offsets,
            schedule,
        }
    }

    fn calendar(&self, progress: Progress) -> Calendar {
        let mut calendar = Calendar::default();
        let rev = Revision {
            sequence: 1,
            operation: OWNER,
        };
        let plan = calendar.plans.entry(PLAN).or_default();
        let [prep, early, late] = self.offsets;
        let tasks = if progress == Progress::NoChecklist {
            Vec::new()
        } else {
            [
                (EARLY, "Утвердить список", early),
                (LATE, "Разослать письма", late),
            ]
            .map(|(id, title, days_before)| Task {
                id,
                title: title.into(),
                days_before,
            })
            .into()
        };
        let mut event = Event {
            title: "Работа с партнёрами".into(),
            category: Category::Partners,
            description: String::new(),
            date: self.template,
            yearly: self.schedule != Schedule::Once,
            owner: Some(OWNER),
            preparation_days: prep,
            tasks,
        };
        plan.templates.insert(
            self.template.year(),
            Record::new(rev, Some(event.clone().into())),
        );
        if self.schedule != Schedule::Once {
            let previous = plan.years.entry(self.target.year() - 1).or_default();
            for task in &event.tasks {
                previous.checks.insert(task.id, Record::new(rev, true));
            }
            previous.note = Some(Record::new(rev, "Итог прошлого года".into()));
        }
        let year = plan.years.entry(self.target.year()).or_default();
        for task in &event.tasks {
            if progress.done(task.id) {
                year.checks.insert(task.id, Record::new(rev, true));
            }
        }
        if self.schedule == Schedule::MovedThisYear {
            event.date = self.target;
            year.change = Some(Record::new(rev, YearChange::Edit(event.into())));
        }
        calendar.validate().unwrap();
        calendar
    }

    fn dates_to_check(&self) -> [NaiveDate; 10] {
        [
            self.preparation - Duration::days(1),
            self.preparation,
            self.early - Duration::days(2),
            self.early - Duration::days(1),
            self.early,
            self.late - Duration::days(1),
            self.late,
            self.target,
            self.late + Duration::days(90),
            self.late + Duration::days(91),
        ]
    }
}

// Rows from other years of the same annual plan can legitimately overlap this one.
fn rows_for_year(rows: Vec<Reminder>, year: i32) -> Vec<(NaiveDate, Milestone)> {
    rows.into_iter()
        .filter(|row| row.year == year && row.event == PLAN)
        .map(|row| (row.date, row.kind))
        .collect()
}

fn check_day(s: &Scenario, calendar: &Calendar, progress: Progress, today: NaiveDate) {
    let year = s.target.year();
    let case = format!("target={} today={today} progress={progress:?}", s.target);
    let empty = progress == Progress::NoChecklist;
    let mut outstanding = Vec::new();
    if !empty {
        for (id, due) in [(EARLY, s.early), (LATE, s.late)] {
            if !progress.done(id) {
                outstanding.push((due, Milestone::Task(id)));
            }
        }
    }
    let needs_attention =
        today >= s.preparation && (!outstanding.is_empty() || (empty && today <= s.target));
    let personal = planning::personal_events(calendar, today, OWNER);
    assert_eq!(
        personal
            .iter()
            .filter(|e| e.year == year && e.id == PLAN)
            .count(),
        usize::from(needs_attention),
        "{case} personal"
    );
    assert!(
        planning::personal_events(calendar, today, COLLEAGUE).is_empty(),
        "{case} colleague"
    );

    let mut expected = Vec::new();
    if needs_attention {
        expected.push((s.preparation, Milestone::Preparation));
        expected.extend(
            outstanding
                .iter()
                .filter(|(due, _)| *due <= today + Duration::days(1))
                .copied(),
        );
    }
    assert_eq!(
        rows_for_year(planning::due_reminders(&personal, today), year),
        expected,
        "{case} reminders"
    );

    let mut milestones = Vec::new();
    if empty || !outstanding.is_empty() {
        milestones.push((s.preparation, Milestone::Preparation));
    }
    milestones.extend(outstanding.iter().copied());
    milestones.push((s.target, Milestone::Event));
    for (horizon, days) in [
        (Horizon::Days30, 30),
        (Horizon::Days60, 60),
        (Horizon::Days90, 90),
    ] {
        let visible: Vec<_> = milestones
            .iter()
            .copied()
            .filter(|(day, _)| (0..days).contains(&(*day - today).num_days()))
            .collect();
        for owner in [OwnerFilter::Everyone, OwnerFilter::Member(OWNER)] {
            assert_eq!(
                rows_for_year(planning::agenda(calendar, today, horizon, owner), year),
                visible,
                "{case} agenda {horizon:?}, {owner:?}"
            );
        }
        assert!(
            planning::agenda(calendar, today, horizon, OwnerFilter::Unassigned).is_empty(),
            "{case} unassigned"
        );
    }
    let overdue: Vec<_> = outstanding
        .into_iter()
        .filter(|(day, _)| (1..=90).contains(&(today - *day).num_days()))
        .collect();
    assert_eq!(
        rows_for_year(
            planning::overdue(calendar, today, OwnerFilter::Member(OWNER)),
            year
        ),
        overdue,
        "{case} overdue"
    );
    assert!(
        planning::overdue(calendar, today, OwnerFilter::Member(COLLEAGUE)).is_empty(),
        "{case} overdue colleague"
    );
}

#[test]
fn planning_calendar_scenarios() {
    use Schedule::*;
    // Independently specified calendar dates keep the expected deadlines separate
    // from the event offsets used as input by the application.
    let scenarios = [
        Scenario::new(
            [
                "2026-01-07",
                "2027-01-07",
                "2026-11-23",
                "2026-12-13",
                "2026-12-31",
            ],
            [45, 25, 7],
            Annual,
        ),
        Scenario::new(
            [
                "2024-02-29",
                "2025-02-28",
                "2025-01-19",
                "2025-02-07",
                "2025-02-23",
            ],
            [40, 21, 5],
            Annual,
        ),
        Scenario::new(
            [
                "2024-02-29",
                "2028-02-29",
                "2028-01-25",
                "2028-02-11",
                "2028-02-23",
            ],
            [35, 18, 6],
            Annual,
        ),
        Scenario::new(
            [
                "2026-03-20",
                "2027-04-01",
                "2027-03-02",
                "2027-03-12",
                "2027-03-27",
            ],
            [30, 20, 5],
            MovedThisYear,
        ),
        Scenario::new(
            [
                "2027-12-31",
                "2027-12-31",
                "2027-10-27",
                "2027-11-21",
                "2027-12-22",
            ],
            [65, 40, 9],
            Once,
        ),
        Scenario::new(
            [
                "2028-01-02",
                "2028-01-02",
                "2027-01-01",
                "2027-10-04",
                "2027-12-18",
            ],
            [366, 90, 15],
            Once,
        ),
    ];
    let mut passed = 0;
    for s in scenarios {
        for progress in [
            Progress::Open,
            Progress::EarlyDone,
            Progress::LateDone,
            Progress::Complete,
            Progress::NoChecklist,
        ] {
            let calendar = s.calendar(progress);
            let year = s.target.year();
            let event = calendar.event(PLAN, year).unwrap();
            assert_eq!(event.date, s.target);
            assert_eq!(event.preparation_date(), s.preparation);
            assert_eq!(event.note(), "");
            match s.schedule {
                Once => assert!(calendar.event(PLAN, year + 1).is_none()),
                Annual | MovedThisYear => {
                    assert_eq!(
                        calendar.event(PLAN, year - 1).unwrap().note(),
                        "Итог прошлого года"
                    );
                    let next = calendar.event(PLAN, year + 1).unwrap();
                    assert_eq!(next.completed(), 0);
                    assert_eq!(next.date.month(), s.template.month());
                    if s.schedule == MovedThisYear {
                        assert_eq!(next.date.day(), s.template.day());
                    }
                }
            }
            for today in s.dates_to_check() {
                check_day(&s, &calendar, progress, today);
                println!(
                    "PASS planning target={} progress={progress:?} today={today}",
                    s.target
                );
                passed += 1;
            }
        }
    }
    assert_eq!(passed, 300);
}
