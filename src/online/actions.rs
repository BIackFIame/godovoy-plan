use chrono::{Datelike, NaiveDate};
use godovoy_plan::{
    model::{Category, Event, Id, Occurrence, Record, Revision, Task, YearChange},
    planning::personal_events,
    storage::online::{Access, Data},
};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::BTreeSet;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChecklistItem<'a> {
    id: Id,
    title: &'a str,
    days_before: u16,
    due_date: NaiveDate,
    done: bool,
}

// Serialize checklist entries straight into the response buffer.
struct Checklist<'a>(Occurrence<'a>);
impl Serialize for Checklist<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let event = self.0;
        serializer.collect_seq(event.event.tasks.iter().map(|task| ChecklistItem {
            id: task.id,
            title: &task.title,
            days_before: task.days_before,
            due_date: event.due(task),
            done: event.checked(task.id),
        }))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EventView<'a> {
    id: Id,
    year: i32,
    title: &'a str,
    description: &'a str,
    category: Category,
    date: NaiveDate,
    yearly: bool,
    owner_id: Option<Id>,
    preparation_days: u16,
    preparation_date: NaiveDate,
    checklist: Checklist<'a>,
    note: &'a str,
    #[serde(serialize_with = "serialize_revision")]
    revision: Revision,
}

fn serialize_revision<S: Serializer>(
    revision: &Revision,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&format_args!(
        "{}:{}",
        revision.sequence, revision.operation
    ))
}

pub(crate) fn occurrence<'a>(
    data: &Data,
    e: Occurrence<'a>,
    allow_free_text: bool,
) -> EventView<'a> {
    let event = e.event;
    EventView {
        id: e.id,
        year: e.year,
        title: &event.title,
        description: if allow_free_text {
            &event.description
        } else {
            ""
        },
        category: event.category,
        date: e.date,
        yearly: event.yearly,
        owner_id: event.owner.map(|id| data.calendar.person_for(id)),
        preparation_days: event.preparation_days,
        preparation_date: e.preparation_date(),
        checklist: Checklist(e),
        note: if allow_free_text { e.note() } else { "" },
        revision: e.revision,
    }
}

#[derive(Serialize)]
struct Employee<'a> {
    id: Id,
    name: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ViewContext<'a> {
    #[serde(skip)]
    pub(crate) policy_revision: u64,
    #[serde(skip)]
    pub(crate) offer_overview: bool,
    workspace_name: &'a str,
    employee_id: Id,
    employees: Vec<Employee<'a>>,
    can_edit: bool,
    allow_free_text: bool,
    assigned_only: bool,
    year: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PublicYear {
    year: i32,
    allow_free_text: bool,
}

#[derive(Serialize)]
pub(crate) struct CalendarView<'a, E = Vec<EventView<'a>>> {
    #[serde(flatten)]
    context: ViewContext<'a>,
    events: E,
    tasks: Vec<EventView<'a>>,
}

#[derive(Serialize)]
struct ChangedRow<'v, 'a> {
    index: usize,
    event: &'v EventView<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NextTask<'a> {
    id: Id,
    title: &'a str,
    due_date: NaiveDate,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OverviewRow<'a> {
    id: Id,
    year: i32,
    title: &'a str,
    date: NaiveDate,
    category: Category,
    owner_id: Option<Id>,
    completed: usize,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<NextTask<'a>>,
}

impl<'a> OverviewRow<'a> {
    fn from_event(event: &EventView<'a>, personal: bool) -> Self {
        let occurrence = event.checklist.0;
        let mut completed = 0;
        let mut next: Option<NextTask<'_>> = None;
        for task in &occurrence.event.tasks {
            if occurrence.checked(task.id) {
                completed += 1;
            } else if personal {
                let due_date = occurrence.due(task);
                if next
                    .as_ref()
                    .is_none_or(|previous| due_date < previous.due_date)
                {
                    next = Some(NextTask {
                        id: task.id,
                        title: &task.title,
                        due_date,
                    });
                }
            }
        }
        Self {
            id: event.id,
            year: event.year,
            title: event.title,
            date: event.date,
            category: event.category,
            owner_id: event.owner_id,
            completed,
            total: occurrence.event.tasks.len(),
            next,
        }
    }
}

struct OverviewRows<'v, 'a>(&'v [EventView<'a>], bool);
impl Serialize for OverviewRows<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(
            self.0
                .iter()
                .map(|row| OverviewRow::from_event(row, self.1)),
        )
    }
}

#[derive(Serialize)]
struct Overview<'v, 'a> {
    #[serde(flatten)]
    context: &'v ViewContext<'a>,
    overview: bool,
    events: OverviewRows<'v, 'a>,
    tasks: OverviewRows<'v, 'a>,
}

#[derive(Serialize)]
struct OverviewChange<'v, 'a> {
    id: Id,
    year: i32,
    events: Vec<OverviewChangeRow<'a>>,
    tasks: Vec<OverviewChangeRow<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'v EventView<'a>>,
}

#[derive(Serialize)]
struct OverviewChangeRow<'a> {
    index: usize,
    event: OverviewRow<'a>,
}

#[derive(Serialize)]
pub(crate) struct ChangedEvent<'v, 'a> {
    id: Id,
    year: i32,
    events: Vec<ChangedRow<'v, 'a>>,
    tasks: Vec<ChangedRow<'v, 'a>>,
}

impl ChangedEvent<'_, '_> {
    pub(crate) fn has_visible_rows(&self) -> bool {
        !self.events.is_empty() || !self.tasks.is_empty()
    }
}

impl<'a> CalendarView<'a> {
    pub(crate) fn is_overview(&self) -> bool {
        // A check or a note can add/remove personal tasks without changing the
        // number of annual rows. The representation therefore stays stable for deltas.
        self.context.offer_overview && self.events.len() >= 80
    }

    pub(crate) fn policy_revision(&self) -> u64 {
        self.context.policy_revision
    }

    pub(crate) fn event(&self, id: Id, year: i32) -> Option<&EventView<'a>> {
        self.events
            .iter()
            .chain(&self.tasks)
            .find(|event| event.id == id && event.year == year)
    }

    pub(crate) fn encode(&self) -> serde_json::Result<Vec<u8>> {
        if self.is_overview() {
            serde_json::to_vec(&Overview {
                context: &self.context,
                overview: true,
                events: OverviewRows(&self.events, false),
                tasks: OverviewRows(&self.tasks, true),
            })
        } else {
            serde_json::to_vec(self)
        }
    }

    pub(crate) fn encode_change(
        &self,
        id: Id,
        detail_year: Option<i32>,
    ) -> serde_json::Result<Vec<u8>> {
        if !self.is_overview() {
            return serde_json::to_vec(&self.changed_event(id));
        }
        let rows = |events: &[EventView<'a>], personal| {
            events
                .iter()
                .enumerate()
                .filter(|(_, row)| row.id == id)
                .map(|(index, row)| OverviewChangeRow {
                    index,
                    event: OverviewRow::from_event(row, personal),
                })
                .collect()
        };
        serde_json::to_vec(&OverviewChange {
            id,
            year: self.context.year,
            events: rows(&self.events, false),
            tasks: rows(&self.tasks, true),
            detail: detail_year.and_then(|year| self.event(id, year)),
        })
    }

    pub(crate) fn changed_event(&self, id: Id) -> ChangedEvent<'_, 'a> {
        fn rows<'v, 'a>(events: &'v [EventView<'a>], id: Id) -> Vec<ChangedRow<'v, 'a>> {
            events
                .iter()
                .enumerate()
                .filter(|(_, event)| event.id == id)
                .map(|(index, event)| ChangedRow { index, event })
                .collect()
        }
        ChangedEvent {
            id,
            year: self.context.year,
            events: rows(&self.events, id),
            tasks: rows(&self.tasks, id),
        }
    }
}

impl<'a> ViewContext<'a> {
    pub(crate) fn new(acl: &'a Access, year: i32, employee: Id, can_edit: bool) -> Self {
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
            .map(|(&id, name)| Employee { id, name })
            .collect();
        Self {
            policy_revision: acl.protection().revision,
            offer_overview: false,
            workspace_name: &acl.name,
            employee_id: employee,
            employees,
            can_edit,
            allow_free_text: acl.protection().settings.free_text_allowed(),
            assigned_only,
            year,
        }
    }

    pub(crate) fn offer_overview(mut self, offer: bool) -> Self {
        self.offer_overview = offer && !self.assigned_only;
        self
    }

    pub(crate) fn public_rows_key(&self) -> Option<PublicYear> {
        (!self.assigned_only).then_some(PublicYear {
            year: self.year,
            allow_free_text: self.allow_free_text,
        })
    }
    pub(crate) fn event_rows(&self, data: &'a Data) -> Vec<EventView<'a>> {
        let mut events: Vec<_> = data
            .calendar
            .occurrences(self.year)
            .filter(|e| !self.assigned_only || assigned_to(data, *e, self.employee_id))
            .collect();
        events.sort_unstable_by_key(|e| (e.date, e.id));
        events
            .into_iter()
            .map(|e| occurrence(data, e, self.allow_free_text))
            .collect()
    }
    pub(crate) fn with_rows<E>(
        self,
        data: &'a Data,
        today: NaiveDate,
        events: E,
    ) -> CalendarView<'a, E> {
        let tasks = personal_events(&data.calendar, today, self.employee_id)
            .into_iter()
            .map(|e| occurrence(data, e, self.allow_free_text))
            .collect();
        CalendarView {
            context: self,
            events,
            tasks,
        }
    }
    pub(crate) fn render(self, data: &'a Data, today: NaiveDate) -> CalendarView<'a> {
        let events = self.event_rows(data);
        self.with_rows(data, today, events)
    }
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
pub(crate) fn assigned_to(data: &Data, event: Occurrence<'_>, employee: Id) -> bool {
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
