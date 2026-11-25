use chrono::{Datelike, Local, NaiveDate};
use godovoy_plan::{
    model::{Category, Event, Id, Occurrence, Record, Revision, Task, YearChange},
    planning::personal_events,
    storage::online::{Access, Data},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn occurrence(data: &Data, e: Occurrence<'_>, allow_free_text: bool) -> Value {
    let event = e.event;
    let description = if allow_free_text {
        event.description.as_str()
    } else {
        ""
    };
    let note = if allow_free_text { e.note() } else { "" };
    let checklist=event.tasks.iter().map(|t|json!({"id":t.id,"title":t.title,"daysBefore":t.days_before,"dueDate":e.due(t),"done":e.checked(t.id)})).collect::<Vec<_>>();
    json!({"id":e.id,"year":e.year,"title":event.title,"description":description,"category":event.category,
        "date":e.date,"yearly":event.yearly,"ownerId":event.owner.map(|id|data.calendar.person_for(id)),
        "preparationDays":event.preparation_days,"preparationDate":e.preparation_date(),"checklist":checklist,
        "note":note,"revision":format!("{}:{}",e.revision.sequence,e.revision.operation)})
}
pub fn state(data: &Data, acl: &Access, year: i32, employee: Id, can_edit: bool) -> Value {
    let allow_free_text = acl.protection().settings.free_text_allowed();
    let assigned_only = acl.assigned_only.contains(&employee);
    let active = acl
        .grants
        .values()
        .map(|g| g.employee)
        .collect::<BTreeSet<_>>();
    let employees = acl
        .employees
        .iter()
        .filter(|(id, _)| active.contains(id) && (!assigned_only || **id == employee))
        .map(|(id, name)| json!({"id":id,"name":name}))
        .collect::<Vec<_>>();
    let tasks = personal_events(&data.calendar, Local::now().date_naive(), employee);
    json!({"workspaceName":acl.name,"employeeId":employee,"employees":employees,"canEdit":can_edit,"allowFreeText":allow_free_text,"assignedOnly":assigned_only,"year":year,
        "events":data.calendar.year(year).into_iter().filter(|e| !assigned_only || assigned_to(data, *e, employee)).map(|e|occurrence(data,e,allow_free_text)).collect::<Vec<_>>(),
        "tasks":tasks.into_iter().map(|e|occurrence(data,e,allow_free_text)).collect::<Vec<_>>()})
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Scope {
    Year,
    Future,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskInput {
    id: Option<Id>,
    title: String,
    days_before: u16,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fields {
    title: String,
    #[serde(default)]
    description: String,
    category: Category,
    date: NaiveDate,
    yearly: bool,
    owner_id: Option<Id>,
    preparation_days: u16,
    checklist: Vec<TaskInput>,
}
impl Fields {
    fn build(
        self,
        data: &Data,
        acl: &Access,
        old: Option<Occurrence<'_>>,
    ) -> Result<Event, &'static str> {
        let previous = old.and_then(|e| e.event.owner);
        let unchanged = self.owner_id == previous.map(|id| data.calendar.person_for(id));
        let owner = if unchanged {
            previous
        } else {
            if self.owner_id.is_some_and(|id| !acl.active_employee(id)) {
                return Err("Ответственный больше недоступен.");
            }
            self.owner_id
        };
        if old.is_some_and(|e| self.date.year() != e.year) {
            return Err("Выберите дату в том же году.");
        }
        let mut tasks = Vec::new();
        for input in self.checklist {
            if let Some(id) = input.id
                && !old.is_some_and(|e| e.event.tasks.iter().any(|t| t.id == id))
            {
                return Err("Пункт чек-листа больше недоступен.");
            }
            tasks.push(Task {
                id: input.id.unwrap_or_else(Id::new_v4),
                title: input.title.trim().into(),
                days_before: input.days_before,
            });
        }
        let date = old
            .filter(|e| self.yearly && self.date == e.date)
            .map_or(self.date, |e| e.event.date);
        let event = Event {
            title: self.title.trim().into(),
            description: if acl.protection().settings.free_text_allowed() {
                self.description
            } else {
                old.map(|e| e.event.description.clone()).unwrap_or_default()
            },
            category: self.category,
            date,
            yearly: self.yearly,
            owner,
            preparation_days: self.preparation_days,
            tasks,
        };
        event.validate()?;
        Ok(event)
    }
}
#[derive(Deserialize)]
#[serde(
    tag = "op",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Command {
    CreateEvent {
        #[serde(flatten)]
        fields: Fields,
    },
    EditEvent {
        event_id: Id,
        year: i32,
        revision: String,
        scope: Scope,
        #[serde(flatten)]
        fields: Fields,
    },
    DeleteEvent {
        event_id: Id,
        year: i32,
        revision: String,
        scope: Scope,
    },
    Note {
        event_id: Id,
        year: i32,
        text: String,
        base: String,
    },
    Check {
        event_id: Id,
        year: i32,
        task_id: Id,
        done: bool,
    },
}
fn assigned_to(data: &Data, event: Occurrence<'_>, employee: Id) -> bool {
    event
        .event
        .owner
        .is_some_and(|owner| data.calendar.person_for(owner) == data.calendar.person_for(employee))
}
impl Command {
    pub fn allowed_for_assignee(&self, data: &Data, employee: Id) -> bool {
        match self {
            Self::Check { event_id, year, .. } | Self::Note { event_id, year, .. } => {
                (1900..=9999).contains(year)
                    && data
                        .calendar
                        .follow_event(*event_id, *year)
                        .is_some_and(|event| assigned_to(data, event, employee))
            }
            Self::CreateEvent { .. } | Self::EditEvent { .. } | Self::DeleteEvent { .. } => false,
        }
    }
    pub fn uses_free_text(&self) -> bool {
        match self {
            Self::CreateEvent { fields } | Self::EditEvent { fields, .. } => {
                !fields.description.is_empty()
            }
            Self::Note { .. } => true,
            _ => false,
        }
    }
}
pub struct Rejection {
    pub message: &'static str,
    pub conflict: bool,
}
impl From<&'static str> for Rejection {
    fn from(message: &'static str) -> Self {
        Self {
            message,
            conflict: false,
        }
    }
}
fn stale() -> Rejection {
    Rejection {
        message: "Событие изменилось. Обновите календарь.",
        conflict: true,
    }
}
struct Target {
    id: Id,
    year: i32,
    revision: String,
    scope: Scope,
}
fn update(
    data: &mut Data,
    acl: &Access,
    target: Target,
    fields: Option<Fields>,
    stamp: Revision,
) -> Result<(), Rejection> {
    let old = data
        .calendar
        .event(target.id, target.year)
        .ok_or_else(stale)?;
    if format!("{}:{}", old.revision.sequence, old.revision.operation) != target.revision {
        return Err(stale());
    }
    let preserve_override = old.overridden && !acl.protection().settings.free_text_allowed();
    let event = fields.map(|f| f.build(data, acl, Some(old))).transpose()?;
    let plan = data.calendar.plans.get_mut(&target.id).ok_or_else(stale)?;
    if event.as_ref().is_some_and(|e| e.yearly) && plan.imported_single_has_other_dates() {
        return Err("Для ежегодного повтора создайте отдельное событие.".into());
    }
    match target.scope {
        Scope::Year => {
            let change = event.map_or(YearChange::Cancel, |e| YearChange::Edit(e.into()));
            plan.years.entry(target.year).or_default().change = Some(Record::new(stamp, change));
        }
        Scope::Future => {
            let mut template = event;
            let mut change = YearChange::UseTemplate;
            if preserve_override && let Some(event) = &mut template {
                let base_description = plan
                    .templates
                    .range(..=target.year)
                    .next_back()
                    .and_then(|(_, record)| record.value.as_ref())
                    .map(|base| base.description.as_str())
                    .unwrap_or("");
                if base_description != event.description {
                    // Keep the year's hidden exception without propagating it into future occurrences.
                    change = YearChange::Edit(event.clone().into());
                    event.description = base_description.to_owned();
                }
            }
            plan.templates
                .insert(target.year, Record::new(stamp, template.map(Into::into)));
            if let Some(year) = plan.years.get_mut(&target.year) {
                year.change = Some(Record::new(stamp, change));
            }
        }
    }
    Ok(())
}
pub fn apply(cmd: Command, data: &mut Data, acl: &Access) -> Result<(&'static str, Id), Rejection> {
    let year = match &cmd {
        Command::CreateEvent { fields } => fields.date.year(),
        Command::EditEvent { year, .. }
        | Command::DeleteEvent { year, .. }
        | Command::Check { year, .. }
        | Command::Note { year, .. } => *year,
    };
    if !(1900..=9999).contains(&year) {
        return Err("Год должен быть от 1900 до 9999.".into());
    }
    let stamp = Revision {
        sequence: data
            .calendar
            .latest()
            .checked_add(1)
            .ok_or("Исчерпан счётчик операций.")?,
        operation: Id::new_v4(),
    };
    match cmd {
        Command::CreateEvent { fields } => {
            let e = fields.build(data, acl, None)?;
            let id = Id::new_v4();
            data.calendar
                .plans
                .entry(id)
                .or_default()
                .templates
                .insert(e.date.year(), Record::new(stamp, Some(e.into())));
            Ok(("create", id))
        }
        Command::EditEvent {
            event_id,
            year,
            revision,
            scope,
            fields,
        } => {
            update(
                data,
                acl,
                Target {
                    id: event_id,
                    year,
                    revision,
                    scope,
                },
                Some(fields),
                stamp,
            )?;
            Ok(("edit", event_id))
        }
        Command::DeleteEvent {
            event_id,
            year,
            revision,
            scope,
        } => {
            update(
                data,
                acl,
                Target {
                    id: event_id,
                    year,
                    revision,
                    scope,
                },
                None,
                stamp,
            )?;
            Ok(("delete", event_id))
        }
        Command::Check {
            event_id,
            year,
            task_id,
            done,
        } => {
            let e = data
                .calendar
                .follow_event(event_id, year)
                .ok_or_else(stale)?;
            if !e.event.tasks.iter().any(|t| t.id == task_id) {
                return Err(stale());
            }
            let actual = e.year;
            data.calendar
                .plans
                .get_mut(&event_id)
                .ok_or_else(stale)?
                .years
                .entry(actual)
                .or_default()
                .checks
                .insert(task_id, Record::new(stamp, done));
            Ok(("check", event_id))
        }
        Command::Note {
            event_id,
            year,
            text,
            base,
        } => {
            if text.chars().count() > 4000 {
                return Err("Заметка не должна превышать 4000 символов.".into());
            }
            let e = data
                .calendar
                .follow_event(event_id, year)
                .ok_or_else(stale)?;
            if e.note() != base && e.note() != text.trim() {
                return Err(stale());
            }
            let actual = e.year;
            data.calendar
                .plans
                .get_mut(&event_id)
                .ok_or_else(stale)?
                .years
                .entry(actual)
                .or_default()
                .note = Some(Record::new(stamp, text.trim().into()));
            Ok(("note", event_id))
        }
    }
}
