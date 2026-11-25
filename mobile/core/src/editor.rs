use chrono::{Datelike, NaiveDate};
use godovoy_plan::{
    Fallible,
    model::{Category, Event, Id, Occurrence, Record, Task, YearChange},
    storage::SharedStore,
};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Selection {
    event_id: Id,
    year: i32,
    revision: String,
    scope: Scope,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum Scope {
    Year,
    Future,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fields {
    title: String,
    description: String,
    date: NaiveDate,
    yearly: bool,
    preparation_days: u16,
    owner_id: Option<Id>,
    category: Category,
    checklist: Vec<TaskInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskInput {
    id: Option<Id>,
    title: String,
    days_before: u16,
}

impl Fields {
    fn into_event(self, old: Occurrence<'_>) -> Fallible<Event> {
        if self.date.year() != old.year {
            return Err("Измените дату в пределах выбранного года.".into());
        }
        let mut tasks = Vec::with_capacity(self.checklist.len());
        for entry in self.checklist {
            if entry
                .id
                .is_some_and(|id| !old.event.tasks.iter().any(|task| task.id == id))
            {
                return Err("Пункт чек-листа больше недоступен. Откройте событие заново.".into());
            }
            tasks.push(Task {
                id: entry.id.unwrap_or_else(Id::new_v4),
                title: entry.title.trim().to_owned(),
                days_before: entry.days_before,
            });
        }
        let event = Event {
            title: self.title.trim().to_owned(),
            description: self.description,
            category: self.category,
            date: if self.yearly && self.date == old.date {
                old.event.date
            } else {
                self.date
            },
            yearly: self.yearly,
            preparation_days: self.preparation_days,
            owner: self.owner_id,
            tasks,
        };
        event.validate()?;
        Ok(event)
    }
}

pub(super) fn edit_event(store: &SharedStore, json: Value) -> Fallible<Value> {
    let fields = match json.get("op").and_then(Value::as_str) {
        Some("editEvent") => Some(Fields::deserialize(&json)?),
        Some("deleteEvent") => None,
        _ => return Err("Неизвестная операция редактора.".into()),
    };
    let target: Selection = serde_json::from_value(json)?;
    store
        .lock()
        .map_err(|_| "Профиль недоступен.")?
        .transaction(|state| {
            let revision = state.revision()?;
            let workspace = state.workspace_mut()?;
            let old = workspace
                .calendar
                .event(target.event_id, target.year)
                .ok_or("Событие перенесено или удалено.")?;
            if format!("{}:{}", old.revision.sequence, old.revision.operation) != target.revision {
                return Err("Событие изменилось на другом устройстве. Откройте его заново.".into());
            }
            let event = if let Some(fields) = fields {
                workspace.check_assignment(old.event.owner, fields.owner_id)?;
                Some(fields.into_event(old)?)
            } else {
                None
            };
            let plan = workspace
                .calendar
                .plans
                .get_mut(&target.event_id)
                .ok_or("Событие удалено.")?;
            if event.as_ref().is_some_and(|e| e.yearly) && plan.imported_single_has_other_dates() {
                return Err("Для ежегодного повтора создайте отдельное событие.".into());
            }
            match target.scope {
                Scope::Year => {
                    let change = event.map_or(YearChange::Cancel, |e| YearChange::Edit(e.into()));
                    plan.years.entry(target.year).or_default().change =
                        Some(Record::new(revision, change));
                }
                Scope::Future => {
                    plan.templates
                        .insert(target.year, Record::new(revision, event.map(Into::into)));
                    if let Some(year) = plan.years.get_mut(&target.year) {
                        year.change = Some(Record::new(revision, YearChange::UseTemplate));
                    }
                }
            }
            Ok(Value::Null)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar;
    use godovoy_plan::storage::Store;
    use serde_json::json;

    #[test]
    fn annual_edit_keeps_leap_anchor_and_task_identity_and_rejects_stale_revision() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("phone")).unwrap();
        store
            .lock()
            .unwrap()
            .transaction(|s| s.create_workspace("Команда".into(), "Анна".into()))
            .unwrap();
        let task = Id::new_v4();
        calendar::create(
            &store,
            Event {
                title: "Исходное".into(),
                description: String::new(),
                category: Category::Team,
                date: "2028-02-29".parse().unwrap(),
                yearly: true,
                preparation_days: 14,
                owner: None,
                tasks: vec![Task {
                    id: task,
                    title: "Список".into(),
                    days_before: 7,
                }],
            },
        )
        .unwrap();
        let snapshot = store.lock().unwrap().snapshot();
        let event = snapshot.workspace.as_ref().unwrap().calendar.year(2029)[0];
        let id = event.id;
        let revision = format!("{}:{}", event.revision.sequence, event.revision.operation);
        let mut edit = json!({"op":"editEvent","eventId":id,"year":2029,"revision":revision,
            "scope":"future","title":"Отправить письма","description":"","date":"2029-02-28",
            "yearly":true,"category":"Team","preparationDays":14,"ownerId":null,
            "checklist":[{"id":task,"title":"Адресаты","daysBefore":7}]});
        calendar::check(&store, id, 2029, task, true).unwrap();
        let mut foreign = edit.clone();
        foreign["checklist"][0]["id"] = json!(Id::new_v4());
        assert!(edit_event(&store, foreign).is_err());
        edit_event(&store, edit.clone()).unwrap();
        assert!(edit_event(&store, edit.clone()).is_err());
        let snapshot = store.lock().unwrap().snapshot();
        let calendar = &snapshot.workspace.as_ref().unwrap().calendar;
        assert_eq!(calendar.event(id, 2028).unwrap().event.title, "Исходное");
        assert_eq!(
            calendar.event(id, 2032).unwrap().date.to_string(),
            "2032-02-29"
        );
        let edited = calendar.event(id, 2029).unwrap();
        assert!(edited.checked(task));
        assert_eq!(edited.event.tasks[0].title, "Адресаты");
        edit["op"] = json!("deleteEvent");
        edit["revision"] = json!(format!(
            "{}:{}",
            edited.revision.sequence, edited.revision.operation
        ));
        edit["scope"] = json!("year");
        edit_event(&store, edit).unwrap();
        let snapshot = store.lock().unwrap().snapshot();
        let calendar = &snapshot.workspace.as_ref().unwrap().calendar;
        assert!(calendar.event(id, 2029).is_none());
        assert!(calendar.event(id, 2030).is_some());
    }
}
