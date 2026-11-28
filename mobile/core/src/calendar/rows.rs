use chrono::NaiveDate;
use godovoy_plan::model::{Calendar, Category, Id, Occurrence};
use serde::{Serialize, Serializer};

pub(super) struct Rows<'a> {
    pub calendar: &'a Calendar,
    pub occurrences: Vec<Occurrence<'a>>,
    pub checklist_tuples: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskRow<'a> {
    id: Id,
    title: &'a str,
    due_date: NaiveDate,
    days_before: u16,
    done: bool,
}

struct Checklist<'a> {
    event: Occurrence<'a>,
    tuples: bool,
}

impl Serialize for Checklist<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let event = self.event;
        if self.tuples {
            return serializer.collect_seq(event.event.tasks.iter().map(|task| {
                (
                    task.id,
                    &task.title,
                    event.due(task),
                    task.days_before,
                    event.checked(task.id),
                )
            }));
        }
        serializer.collect_seq(event.event.tasks.iter().map(|task| TaskRow {
            id: task.id,
            title: &task.title,
            due_date: event.due(task),
            days_before: task.days_before,
            done: event.checked(task.id),
        }))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EventRow<'a> {
    id: Id,
    year: i32,
    title: &'a str,
    date: NaiveDate,
    description: &'a str,
    yearly: bool,
    category: Category,
    owner_id: Option<Id>,
    employee_id: Option<Id>,
    preparation_date: NaiveDate,
    preparation_days: u16,
    note: &'a str,
    revision: String,
    checklist: Checklist<'a>,
}

impl Serialize for Rows<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.occurrences.iter().copied().map(|occurrence| {
            let event = occurrence.event;
            EventRow {
                id: occurrence.id,
                year: occurrence.year,
                title: &event.title,
                date: occurrence.date,
                description: &event.description,
                yearly: event.yearly,
                category: event.category,
                owner_id: event.owner,
                employee_id: event.owner.map(|id| self.calendar.person_for(id)),
                preparation_date: occurrence.preparation_date(),
                preparation_days: event.preparation_days,
                note: occurrence.note(),
                revision: format!(
                    "{}:{}",
                    occurrence.revision.sequence, occurrence.revision.operation
                ),
                checklist: Checklist {
                    event: occurrence,
                    tuples: self.checklist_tuples,
                },
            }
        }))
    }
}
