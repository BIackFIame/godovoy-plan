use std::{
    collections::{BTreeSet, BinaryHeap},
    ops::RangeInclusive,
};

use chrono::{Datelike, Days, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::model::{Calendar, Id, Occurrence, YearChange};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Milestone {
    Preparation,
    Task(Id),
    Event,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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

// A device has at most one outgoing link. Traversing incoming links from the
// employee visits each matching device once when the root is excluded.
fn employee_assignments(calendar: &Calendar, member: Id) -> impl Fn(Id) -> bool + '_ {
    let employee = calendar.person_for(member);
    let aliases = (calendar.plans.len() >= calendar.device_people.len()).then(|| {
        let mut devices = Vec::new();
        let mut parent = employee;
        let mut index = 0;
        loop {
            devices.extend(calendar.device_people.iter().filter_map(|(&device, link)| {
                (device != employee && link.value == parent).then_some(device)
            }));
            let Some(&next) = devices.get(index) else {
                break;
            };
            parent = next;
            index += 1;
        }
        devices.sort_unstable();
        devices
    });
    move |owner| {
        owner == employee
            || match &aliases {
                Some(devices) => devices.binary_search(&owner).is_ok(),
                None => calendar.person_for(owner) == employee,
            }
    }
}

impl OwnerFilter {
    fn for_each_occurrence<'a>(
        self,
        calendar: &'a Calendar,
        years: RangeInclusive<i32>,
        visit: impl FnMut(Occurrence<'a>),
    ) {
        let events = years.flat_map(|year| calendar.occurrences(year));
        match self {
            Self::Everyone => events.for_each(visit),
            Self::Unassigned => events
                .filter(|event| event.event.owner.is_none())
                .for_each(visit),
            Self::Member(member) => {
                let assigned = employee_assignments(calendar, member);
                events
                    .filter(|event| event.event.owner.is_some_and(&assigned))
                    .for_each(visit);
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

fn personal_years(today: NaiveDate) -> RangeInclusive<i32> {
    today.year() - 1..=through(today, 366).year()
}

/// Look up one occurrence with the same visibility and due-date rules as the personal list.
pub fn personal_event(
    calendar: &Calendar,
    today: NaiveDate,
    member: Id,
    event_id: Id,
    year: i32,
) -> Option<Occurrence<'_>> {
    if !personal_years(today).contains(&year) {
        return None;
    }
    let row = calendar.event(event_id, year)?;
    let owner = row.event.owner?;
    (calendar.person_for(owner) == calendar.person_for(member) && row.needs_attention(today, owner))
        .then_some(row)
}

pub fn personal_events(calendar: &Calendar, today: NaiveDate, member: Id) -> Vec<Occurrence<'_>> {
    let is_employee = employee_assignments(calendar, member);
    let years = personal_years(today);
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
                .is_some_and(&is_employee)
            && !plan.years.range(years.clone()).any(|(_, record)| {
                matches!(record.change.as_ref().map(|r| &r.value),
                    Some(YearChange::Edit(event)) if event.owner.is_some_and(&is_employee))
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
    visit_due_reminders(events, today, |r| reminders.push(r));
    reminders.sort_unstable();
    reminders
}

fn visit_due_reminders(
    events: &[Occurrence<'_>],
    today: NaiveDate,
    mut visit: impl FnMut(Reminder),
) {
    let window = TaskWindow::new(NaiveDate::MIN, through(today, 1));
    for event in events.iter().copied() {
        visit(item(
            event,
            event.preparation_date(),
            Milestone::Preparation,
        ));
        visit_open_tasks(event, window, &mut visit);
    }
}

/// Count pending reminders without materializing rows for the mobile agenda index.
pub fn unread_reminder_count(
    events: &[Occurrence<'_>],
    today: NaiveDate,
    receipts: &BTreeSet<Reminder>,
) -> usize {
    let mut count = 0;
    visit_due_reminders(events, today, |r| {
        count += usize::from(!receipts.contains(&r));
    });
    count
}

/// Return one sorted reminder page. For the first pages, keep only the earliest
/// requested keys while visiting the calendar instead of sorting every reminder.
pub fn unread_reminder_page(
    events: &[Occurrence<'_>],
    today: NaiveDate,
    receipts: &BTreeSet<Reminder>,
    offset: usize,
    limit: usize,
) -> Vec<Reminder> {
    const MAX_HEAP_ROWS: usize = 512;
    let end = offset.saturating_add(limit);
    if limit == 0 {
        return Vec::new();
    }
    if end > MAX_HEAP_ROWS {
        // With receipts, scan their ordered keys alongside the sorted due list.
        // Without them, partition the unordered list and sort only this page.
        if !receipts.is_empty() {
            return unread_reminders(events, today, receipts)
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect();
        }
        let mut unread = Vec::new();
        visit_due_reminders(events, today, |r| unread.push(r));
        if offset >= unread.len() {
            return Vec::new();
        }
        unread.select_nth_unstable(offset);
        let rest = &mut unread[offset..];
        if rest.len() > limit {
            rest.select_nth_unstable(limit);
        }
        let n = rest.len().min(limit);
        let page = &mut rest[..n];
        page.sort_unstable();
        return page.to_vec();
    }
    let mut earliest = BinaryHeap::new();
    visit_due_reminders(events, today, |key| {
        if earliest.len() == end && earliest.peek().is_some_and(|last| key >= *last) {
            return;
        }
        if !receipts.contains(&key) {
            if earliest.len() < end {
                earliest.push(key);
            } else if let Some(mut last) = earliest.peek_mut() {
                *last = key;
            }
        }
    });
    earliest
        .into_sorted_vec()
        .into_iter()
        .skip(offset)
        .collect()
}

/// Pending keys in their normal display order, excluding acknowledged reminders.
pub fn unread_reminders(
    events: &[Occurrence<'_>],
    today: NaiveDate,
    receipts: &BTreeSet<Reminder>,
) -> Vec<Reminder> {
    let mut due = due_reminders(events, today);
    if receipts.is_empty() || due.is_empty() {
        return due;
    }
    if receipts.len() / 8 > due.len() {
        // A stale receipt file can cover many more dates than the current list.
        due.retain(|key| !receipts.contains(key));
    } else {
        let mut read = receipts.range(due[0]..).peekable();
        due.retain(|key| {
            while read.next_if(|item| *item < key).is_some() {}
            read.peek().is_none_or(|item| *item != key)
        });
    }
    due
}

pub fn agenda(
    calendar: &Calendar,
    today: NaiveDate,
    horizon: Horizon,
    owner: OwnerFilter,
) -> Vec<AgendaItem> {
    let end = through(today, horizon.days() - 1);
    let in_range = today..=end;
    let window = TaskWindow::new(today, end);
    let mut result = Vec::new();
    owner.for_each_occurrence(calendar, today.year()..=through(end, 366).year(), |event| {
        if in_range.contains(&event.date) {
            result.push(item(event, event.date, Milestone::Event));
        }
        let preparation = event.preparation_date();
        if in_range.contains(&preparation)
            && (event.event.tasks.is_empty() || event.has_open_tasks())
        {
            result.push(item(event, preparation, Milestone::Preparation));
        }
        visit_open_tasks(event, window, |r| result.push(r));
    });
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
    let window = TaskWindow::new(start, yesterday);
    let mut result = Vec::new();
    owner.for_each_occurrence(
        calendar,
        start.year()..=through(yesterday, 366).year(),
        |event| {
            visit_open_tasks(event, window, |r| result.push(r));
        },
    );
    result.sort_unstable();
    result
}

#[derive(Copy, Clone)]
struct TaskWindow {
    first_day: Option<i32>,
    last_day: i32,
}

impl TaskWindow {
    fn new(first: NaiveDate, last: NaiveDate) -> Self {
        // Underflowing due dates saturate at MIN and belong to windows starting there.
        Self {
            first_day: (first != NaiveDate::MIN).then(|| first.num_days_from_ce()),
            last_day: last.num_days_from_ce(),
        }
    }

    fn offsets(self, date: NaiveDate) -> RangeInclusive<i32> {
        let day = date.num_days_from_ce();
        (day - self.last_day)..=self.first_day.map_or(i32::MAX, |first| day - first)
    }
}

fn visit_open_tasks(
    event: Occurrence<'_>,
    window: TaskWindow,
    mut visit: impl FnMut(AgendaItem),
) {
    let offsets = window.offsets(event.date);
    if *offsets.end() < 0 {
        return;
    }
    for task in &event.event.tasks {
        if offsets.contains(&i32::from(task.days_before)) && !event.checked(task.id) {
            visit(item(event, event.due(task), Milestone::Task(task.id)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_window_matches_inclusive_saturating_due_dates() {
        let leap = NaiveDate::from_ymd_opt(2028, 2, 29).unwrap();
        let dates = [
            NaiveDate::MIN,
            NaiveDate::MIN.succ_opt().unwrap(),
            NaiveDate::MIN + Days::new(366),
            leap.pred_opt().unwrap(),
            leap,
            leap.succ_opt().unwrap(),
            NaiveDate::MAX.pred_opt().unwrap(),
            NaiveDate::MAX,
        ];
        for (i, &first) in dates.iter().enumerate() {
            for &last in &dates[i..] {
                for date in dates {
                    let offsets = TaskWindow::new(first, last).offsets(date);
                    for days in [0_u16, 1, 2, 89, 90, 365, 366, u16::MAX] {
                        let due = date
                            .checked_sub_days(Days::new(u64::from(days)))
                            .unwrap_or(NaiveDate::MIN);
                        assert_eq!(
                            offsets.contains(&i32::from(days)),
                            (first..=last).contains(&due),
                            "window {first}..={last}, event {date}, offset {days}",
                        );
                    }
                }
            }
        }
    }
}
