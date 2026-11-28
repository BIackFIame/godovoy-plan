use chrono::{Datelike, Days, NaiveDate};
use godovoy_plan::model::{Calendar, Id};

/// One local notification per day, independent of the visible calendar year.
pub(super) fn dates(calendar: &Calendar, member: Id, today: NaiveDate) -> Vec<NaiveDate> {
    let end = today
        .checked_add_days(Days::new(59))
        .unwrap_or(NaiveDate::MAX);
    let last_event = end
        .checked_add_days(Days::new(366))
        .unwrap_or(NaiveDate::MAX);
    let employee = calendar.person_for(member);
    let mut days = 0u64;
    let last = (end - today).num_days();
    let all_days = (1u64 << (last + 1)) - 1;
    for event in (today.year()..=last_event.year()).flat_map(|year| calendar.occurrences(year)) {
        let prep = event.preparation_date();
        // In a validated calendar, each task is due between preparation and the event.
        if event.date < today
            || prep > end
            || event.event.owner.map(|owner| calendar.person_for(owner)) != Some(employee)
        {
            continue;
        }
        let start = (prep - today).num_days();
        let event_day = (event.date - today).num_days();
        let mut open = event.event.tasks.is_empty();
        for task in &event.event.tasks {
            let due = (event_day - i64::from(task.days_before)).max(start);
            if (due < 0 || due > last + 1) && (open || start < 0) {
                continue;
            }
            if event.checked(task.id) {
                continue;
            }
            open = true;
            for day in [due, (due - 1).max(start)] {
                if (0..=last).contains(&day) {
                    days |= 1u64 << day;
                }
            }
        }
        if open && (0..=last).contains(&start) {
            days |= 1u64 << start;
        }
        if days == all_days {
            break;
        }
    }
    (0..=last)
        .filter(|day| days & (1u64 << day) != 0)
        .map(|day| today + Days::new(day as u64))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use godovoy_plan::model::{Category, Event, Record, Revision, Task};

    #[test]
    fn final_day_alert_includes_tomorrows_deadline_and_excludes_closed_tasks() {
        let today = NaiveDate::from_ymd_opt(2028, 1, 1).unwrap();
        let member = Id::new_v4();
        let id = Id::new_v4();
        let task_id = Id::new_v4();
        let revision = Revision {
            sequence: 1,
            operation: Id::new_v4(),
        };
        let mut calendar = Calendar::default();
        let event = Event {
            title: "Подготовка".into(),
            description: String::new(),
            category: Category::Team,
            date: today + Days::new(71),
            yearly: false,
            owner: Some(member),
            preparation_days: 20,
            tasks: vec![Task {
                id: task_id,
                title: "Проверить".into(),
                days_before: 11,
            }],
        };
        calendar
            .plans
            .entry(id)
            .or_default()
            .templates
            .insert(2028, Record::new(revision, Some(event.into())));
        assert_eq!(
            dates(&calendar, member, today),
            [today + Days::new(51), today + Days::new(59)]
        );
        calendar
            .plans
            .get_mut(&id)
            .unwrap()
            .years
            .entry(2028)
            .or_default()
            .checks
            .insert(task_id, Record::new(revision, true));
        assert!(dates(&calendar, member, today).is_empty());
    }

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
