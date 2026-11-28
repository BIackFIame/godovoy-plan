use chrono::{Datelike, NaiveDate};
use godovoy_plan::{
    Fallible,
    model::{Category, Event, Id, Occurrence, Record, Task, YearChange},
    storage::SharedStore,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Selection {
    view_year: Option<i32>,
    base_token: Option<Id>,
    event_id: Id,
    year: i32,
    revision: String,
    scope: Scope,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Scope {
    Year,
    Future,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Edit {
    view_year: Option<i32>,
    base_token: Option<Id>,
    event_id: Id,
    year: i32,
    revision: String,
    scope: Scope,
    title: String,
    description: String,
    date: NaiveDate,
    yearly: bool,
    preparation_days: u16,
    owner_id: Option<Id>,
    category: Category,
    checklist: Vec<TaskInput>,
}

// Older callers also send the complete edit form on deletion. Accept that precise
// shape, while rejecting misspelled selector fields in either representation.
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum Delete {
    Selection(Selection),
    Form(Edit),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TaskInput {
    id: Option<Id>,
    title: String,
    days_before: u16,
}

impl Edit {
    fn take_selection(&mut self) -> Selection {
        Selection {
            view_year: self.view_year,
            base_token: self.base_token,
            event_id: self.event_id,
            year: self.year,
            revision: std::mem::take(&mut self.revision),
            scope: self.scope,
        }
    }

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

pub(super) fn edit_event(
    store: &SharedStore,
    prior: Option<&super::calendar::LastState>,
    mut edit: Edit,
) -> Fallible<()> {
    change_event(store, prior, edit.take_selection(), Some(edit))
}

pub(super) fn delete_event(
    store: &SharedStore,
    prior: Option<&super::calendar::LastState>,
    delete: Delete,
) -> Fallible<()> {
    let selection = match delete {
        Delete::Selection(selection) => selection,
        Delete::Form(mut form) => form.take_selection(),
    };
    change_event(store, prior, selection, None)
}

fn change_event(
    store: &SharedStore,
    prior: Option<&super::calendar::LastState>,
    target: Selection,
    edit: Option<Edit>,
) -> Fallible<()> {
    let mut store = store.lock().map_err(|_| "Профиль недоступен.")?;
    super::calendar::require_event_view(
        &store,
        prior,
        target.base_token,
        target.view_year,
        target.event_id,
        target.year,
    )?;
    store.transaction(|state| {
        let revision = state.revision()?;
        let workspace = state.workspace_mut()?;
        let old = workspace
            .calendar
            .event(target.event_id, target.year)
            .ok_or("Событие перенесено или удалено.")?;
        if format!("{}:{}", old.revision.sequence, old.revision.operation) != target.revision {
            return Err("Событие изменилось на другом устройстве. Откройте его заново.".into());
        }
        let event = if let Some(edit) = edit {
            workspace.check_assignment(old.event.owner, edit.owner_id)?;
            Some(edit.into_event(old)?)
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
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar;
    use serde_json::json;

    #[test]
    fn annual_edit_keeps_leap_anchor_and_task_identity_and_rejects_stale_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mut phone = crate::Mobile::open(dir.path().join("phone")).unwrap();
        let store = phone.session.as_ref().unwrap().store.clone();
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
        let selector =
            json!({"op":"deleteEvent","eventId":id,"year":2029,"scope":"year","revision":revision});
        let mut legacy_delete = edit.clone();
        legacy_delete["op"] = json!("deleteEvent");
        for mut command in [edit.clone(), selector, legacy_delete] {
            command["baseToke"] = json!(Id::new_v4());
            assert_eq!(
                phone.request(command).unwrap_err().to_string(),
                "Неверные параметры запроса."
            );
        }
        // Parsing directly into Command also catches duplicated JSON keys.
        let duplicated_year = edit.to_string().replacen("{", r#"{"year":2029,"#, 1);
        assert!(serde_json::from_str::<crate::Command>(&duplicated_year).is_err());
        let mut bad_task = edit.clone();
        bad_task["checklist"][0]["dayBefore"] = json!(7);
        assert_eq!(
            phone.request(bad_task).unwrap_err().to_string(),
            "Неверные параметры запроса."
        );
        calendar::check(&store, id, 2029, task, true).unwrap();
        let mut foreign = edit.clone();
        foreign["checklist"][0]["id"] = json!(Id::new_v4());
        assert!(phone.request(foreign).is_err());
        phone.request(edit.clone()).unwrap();
        assert!(phone.request(edit.clone()).is_err());
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
        phone.request(edit).unwrap();
        let snapshot = store.lock().unwrap().snapshot();
        let calendar = &snapshot.workspace.as_ref().unwrap().calendar;
        assert!(calendar.event(id, 2029).is_none());
        assert!(calendar.event(id, 2030).is_some());
    }
}
