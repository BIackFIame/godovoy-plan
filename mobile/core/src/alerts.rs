use chrono::{Datelike, Days, NaiveDate};
use godovoy_plan::model::{Calendar, Id};
use std::collections::BTreeSet;

/// One local notification per day, independent of the visible calendar year.
pub(super) fn dates(calendar: &Calendar, member: Id, today: NaiveDate) -> Vec<NaiveDate> {
    let end = today
        .checked_add_days(Days::new(59))
        .unwrap_or(NaiveDate::MAX);
    let last_event = end
        .checked_add_days(Days::new(366))
        .unwrap_or(NaiveDate::MAX);
    let mut dates = BTreeSet::new();
    for event in (today.year()..=last_event.year()).flat_map(|year| calendar.occurrences(year)) {
        if event.event.owner != Some(member)
            || (!event.event.tasks.is_empty() && event.completed() == event.event.tasks.len())
        {
            continue;
        }
        let prep = event.preparation_date();
        if (today..=end).contains(&prep) {
            dates.insert(prep);
        }
        for task in &event.event.tasks {
            if event.checked(task.id) {
                continue;
            }
            let due = event.due(task);
            let alert = due.pred_opt().unwrap_or(due).max(prep);
            for d in [alert, due] {
                if (today..=end).contains(&d) {
                    dates.insert(d);
                }
            }
        }
    }
    dates.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use godovoy_plan::model::{Category, Event, Record, Revision, Task};

    #[test]
    fn january_preparation_is_scheduled_in_december_for_assignee_only() {
        let mut calendar = Calendar::default();
        let member = Id::new_v4();
        let task = Id::new_v4();
        let rev = Revision {
            sequence: 1,
            operation: Id::new_v4(),
        };
        let day = |s: &str| s.parse::<NaiveDate>().unwrap();
        let id = Id::new_v4();
        let event = Event {
            title: "Партнёры".into(),
            description: String::new(),
            category: Category::Partners,
            date: day("2027-01-05"),
            yearly: true,
            owner: Some(member),
            preparation_days: 30,
            tasks: vec![Task {
                id: task,
                title: "Заказать открытки".into(),
                days_before: 10,
            }],
        };
        calendar
            .plans
            .entry(id)
            .or_default()
            .templates
            .insert(2027, Record::new(rev, Some(event.into())));
        let now = day("2026-12-01");
        assert_eq!(
            dates(&calendar, member, now),
            [day("2026-12-06"), day("2026-12-25"), day("2026-12-26")]
        );
        assert!(dates(&calendar, Id::new_v4(), now).is_empty());
        calendar
            .plans
            .get_mut(&id)
            .unwrap()
            .years
            .entry(2027)
            .or_default()
            .checks
            .insert(task, Record::new(rev, true));
        assert!(dates(&calendar, member, now).is_empty());
        assert!(!dates(&calendar, member, day("2027-12-01")).is_empty());
    }
}
