use std::ops::RangeInclusive;

use chrono::{Datelike, Days, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::model::{Calendar, Id, Occurrence, YearChange};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Milestone {
    Preparation,
    Task(Id),
    Event,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Reminder {
    pub date: NaiveDate,
    pub event: Id,
    pub year: i32,
    pub kind: Milestone,
}

pub type AgendaItem = Reminder;
pub const OVERDUE_DAYS: u64 = 90;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Horizon {
    Days30,
    Days60,
    Days90,
}

impl Horizon {
    pub const ALL: [Self; 3] = [Self::Days30, Self::Days60, Self::Days90];

    pub const fn days(self) -> u64 {
        match self {
            Self::Days30 => 30,
            Self::Days60 => 60,
            Self::Days90 => 90,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum OwnerFilter {
    #[default]
    Everyone,
    Unassigned,
    Member(Id),
}

impl OwnerFilter {
    fn includes(self, calendar: &Calendar, owner: Option<Id>) -> bool {
        match self {
            Self::Everyone => true,
            Self::Unassigned => owner.is_none(),
            Self::Member(id) => {
                owner.is_some_and(|owner| calendar.person_for(owner) == calendar.person_for(id))
            }
        }
    }
}

fn item(event: Occurrence<'_>, date: NaiveDate, kind: Milestone) -> AgendaItem {
    AgendaItem {
        date,
        event: event.id,
        year: event.year,
        kind,
    }
}

fn through(date: NaiveDate, days: u64) -> NaiveDate {
    date.checked_add_days(Days::new(days))
        .unwrap_or(NaiveDate::MAX)
}

pub fn personal_events(calendar: &Calendar, today: NaiveDate, member: Id) -> Vec<Occurrence<'_>> {
    let employee = calendar.person_for(member);
    let years = today.year() - 1..=through(today, 366).year();
    let is_employee = |id| calendar.person_for(id) == employee;
    let mut events = Vec::new();
    for (&id, plan) in &calendar.plans {
        // Most plans have one template. Only an edit in a particular year can
        // assign one of its occurrences to a different employee.
        if plan.templates.len() == 1
            && let Some((_, template)) = plan.templates.first_key_value()
            && !template
                .value
                .as_ref()
                .and_then(|event| event.owner)
                .is_some_and(is_employee)
            && !plan.years.range(years.clone()).any(|(_, record)| {
                matches!(record.change.as_ref().map(|r| &r.value),
                    Some(YearChange::Edit(event)) if event.owner.is_some_and(is_employee))
            })
        {
            continue;
        }
        for year in years.clone() {
            if let Some(event) = plan.event(id, year)
                && let Some(owner) = event.event.owner
                && is_employee(owner)
                && event.needs_attention(today, owner)
            {
                events.push(event);
            }
        }
    }
    events.sort_unstable_by_key(|event| (event.date, event.id));
    events
}

pub fn due_reminders(events: &[Occurrence<'_>], today: NaiveDate) -> Vec<Reminder> {
    let mut reminders = Vec::new();
    for event in events.iter().copied() {
        reminders.push(item(
            event,
            event.preparation_date(),
            Milestone::Preparation,
        ));
        for task in event
            .event
            .tasks
            .iter()
            .filter(|task| !event.checked(task.id))
        {
            let due = event.due(task);
            if due <= through(today, 1) {
                reminders.push(item(event, due, Milestone::Task(task.id)));
            }
        }
    }
    reminders.sort_unstable();
    reminders
}

pub fn agenda(
    calendar: &Calendar,
    today: NaiveDate,
    horizon: Horizon,
    owner: OwnerFilter,
) -> Vec<AgendaItem> {
    let end = through(today, horizon.days() - 1);
    let in_range = today..=end;
    let mut result = Vec::new();
    for event in (today.year()..=through(end, 366).year())
        .flat_map(|year| calendar.occurrences(year))
        .filter(|event| owner.includes(calendar, event.event.owner))
    {
        if in_range.contains(&event.date) {
            result.push(item(event, event.date, Milestone::Event));
        }
        let preparation = event.preparation_date();
        if in_range.contains(&preparation)
            && (event.event.tasks.is_empty() || event.has_open_tasks())
        {
            result.push(item(event, preparation, Milestone::Preparation));
        }
        append_open_tasks(&mut result, event, &in_range);
    }
    result.sort_unstable();
    result
}

pub fn overdue(calendar: &Calendar, today: NaiveDate, owner: OwnerFilter) -> Vec<AgendaItem> {
    let Some(yesterday) = today.pred_opt() else {
        return Vec::new();
    };
    let start = today
        .checked_sub_days(Days::new(OVERDUE_DAYS))
        .unwrap_or(NaiveDate::MIN);
    let in_range = start..=yesterday;
    let mut result = Vec::new();
    for event in (start.year()..=through(yesterday, 366).year())
        .flat_map(|year| calendar.occurrences(year))
        .filter(|event| owner.includes(calendar, event.event.owner))
    {
        append_open_tasks(&mut result, event, &in_range);
    }
    result.sort_unstable();
    result
}

fn append_open_tasks(
    result: &mut Vec<AgendaItem>,
    event: Occurrence<'_>,
    dates: &RangeInclusive<NaiveDate>,
) {
    for task in &event.event.tasks {
        let due = event.due(task);
        if dates.contains(&due) && !event.checked(task.id) {
            result.push(item(event, due, Milestone::Task(task.id)));
        }
    }
}
