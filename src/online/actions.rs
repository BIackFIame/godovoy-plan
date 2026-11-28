use chrono::{Datelike, NaiveDate};
use godovoy_plan::{
    model::{Category, Event, Id, Occurrence, Record, Revision, Task, YearChange},
    planning::personal_events,
    storage::online::{Access, Data},
};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::BTreeSet;

pub(crate) const MIN_CALENDAR_YEAR: i32 = 1900;
pub(crate) const MAX_CALENDAR_YEAR: i32 = 9999;

pub(crate) fn valid_year(year: i32) -> bool {
    (MIN_CALENDAR_YEAR..=MAX_CALENDAR_YEAR).contains(&year)
}

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
impl<'a> Checklist<'a> {
    fn items(&self) -> impl Iterator<Item = ChecklistItem<'a>> {
        let event = self.0;
        event.event.tasks.iter().map(move |task| ChecklistItem {
            id: task.id,
            title: &task.title,
            days_before: task.days_before,
            due_date: event.due(task),
            done: event.checked(task.id),
        })
    }
}

impl Serialize for Checklist<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.items())
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

struct PackedCard<'v, 'a>(&'v EventView<'a>);
struct PackedChecklist<'v, 'a>(&'v Checklist<'a>);

impl Serialize for PackedChecklist<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.items().map(|item| {
            let ChecklistItem { id, title, days_before, due_date, done } = item;
            (id, title, days_before, due_date, done)
        }))
    }
}

#[derive(Serialize)]
struct PackedRevision(#[serde(serialize_with = "serialize_revision")] Revision);

impl Serialize for PackedCard<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Keep both encodings exhaustive over the same authorized fields.
        let EventView {
            id, year, title, description, category, date, yearly, owner_id, preparation_days,
            preparation_date, checklist, note, revision,
        } = self.0;
        (
            id, year, title, description, category, date, yearly, owner_id, preparation_days,
            preparation_date, PackedChecklist(checklist), note, PackedRevision(*revision),
        ).serialize(serializer)
    }
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

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum OverviewFormat {
    Full = 0,
    NamedRows = 1,
    TupleRows = 2,
    RemoteTupleRows = 3,
    AssignedTupleRows = 4,
}

impl OverviewFormat {
    pub(super) fn uses_tuples(self) -> bool {
        matches!(self, Self::TupleRows | Self::RemoteTupleRows | Self::AssignedTupleRows)
    }
}

const FIRST_PAGE_ROWS: u16 = 80;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum YearRows {
    All,
    First,
    FirstTwo,
    FirstFour,
    FirstEight,
    FirstSixteen,
}

impl YearRows {
    pub(super) fn prefix_size(self) -> Option<u16> {
        let pages = match self {
            Self::All => return None,
            Self::First => 1,
            Self::FirstTwo => 2,
            Self::FirstFour => 4,
            Self::FirstEight => 8,
            Self::FirstSixteen => 16,
        };
        Some(FIRST_PAGE_ROWS * pages)
    }

    fn select<T>(self, rows: &[T]) -> &[T] {
        let limit = self.prefix_size().map_or(rows.len(), usize::from);
        &rows[..rows.len().min(limit)]
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ViewContext<'a> {
    #[serde(skip)]
    pub(crate) policy_revision: u64,
    #[serde(skip)]
    pub(crate) overview_format: OverviewFormat,
    #[serde(skip)]
    pub(super) year_rows: YearRows,
    #[serde(skip)]
    pub(super) packed_cards: bool,
    #[serde(skip)]
    continuation: Option<(YearRows, String)>,
    #[serde(skip)]
    pub(super) accepts_page_delta: bool,
    workspace_name: &'a str,
    employee_id: Id,
    employees: Vec<Employee<'a>>,
    can_edit: bool,
    allow_free_text: bool,
    pub(crate) assigned_only: bool,
    year: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PublicYear {
    pub(super) year: i32,
    pub(super) allow_free_text: bool,
}

#[derive(Serialize)]
pub(crate) struct CalendarView<'a, E = Vec<EventView<'a>>> {
    #[serde(flatten)]
    context: ViewContext<'a>,
    #[serde(skip)]
    assigned_owner: Option<Id>,
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

// A roster position is valid only inside this complete authorized snapshot.
#[derive(Serialize)]
#[serde(untagged)]
enum OwnerRef {
    Position(usize),
    Former(Id),
}

struct OverviewRows<'v, 'a> {
    events: &'v [EventView<'a>],
    personal: bool,
    format: OverviewFormat,
    roster: &'v [Employee<'a>],
}

impl Serialize for OverviewRows<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let rows = self.events.iter().map(|row| OverviewRow::from_event(row, self.personal));
        if self.format.uses_tuples() {
            serializer.collect_seq(rows.map(|r| {
                let owner = r.owner_id.map(|id| {
                    // ViewContext builds this list in BTreeMap key order.
                    match self.roster.binary_search_by_key(&id, |person| person.id) {
                        Ok(index) => OwnerRef::Position(index),
                        Err(_) => OwnerRef::Former(id),
                    }
                });
                (r.id, r.year, r.title, r.date, r.category, owner, r.completed, r.total, r.next)
            }))
        } else {
            serializer.collect_seq(rows)
        }
    }
}

#[derive(Serialize)]
struct Overview<'v, 'a> {
    #[serde(flatten)]
    context: &'v ViewContext<'a>,
    overview: bool,
    #[serde(rename = "assignedOwnerId", skip_serializing_if = "Option::is_none")]
    assigned_owner: Option<Id>,
    #[serde(rename = "rowFormat", skip_serializing_if = "Option::is_none")]
    row_format: Option<&'static str>,
    #[serde(rename = "eventCount", skip_serializing_if = "Option::is_none")]
    event_count: Option<usize>,
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

struct CardRows<'v, 'a> {
    rows: &'v [EventView<'a>],
    annual: &'v [EventView<'a>],
}

impl Serialize for CardRows<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(untagged)]
        enum CardOrIndex<'v, 'a> {
            Card(PackedCard<'v, 'a>),
            Index(usize),
        }

        // Only refer to a row from the same authorized year. Adjacent-year tasks
        // keep their full occurrence even when an event ID repeats.
        serializer.collect_seq(self.rows.iter().map(|row| {
            match self.annual.binary_search_by_key(&(row.date, row.id), |e| (e.date, e.id)) {
                Ok(index) if self.annual[index].year == row.year => CardOrIndex::Index(index),
                _ => CardOrIndex::Card(PackedCard(row)),
            }
        }))
    }
}

impl<'a> CalendarView<'a> {
    pub(super) fn continuation(&self) -> Option<(YearRows, &str)> {
        self.context.continuation.as_ref().map(|(prior, tag)| (*prior, tag.as_str()))
    }

    pub(super) fn encode_continuation(&self, prior: YearRows) -> serde_json::Result<Option<Vec<u8>>> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Continuation<'v, 'a> {
            year: i32,
            offset: usize,
            event_count: usize,
            events: OverviewRows<'v, 'a>,
        }
        let Some(offset) = prior.prefix_size().map(usize::from) else { return Ok(None) };
        let events = self.context.year_rows.select(&self.events);
        if events.len() <= offset || events.len() != self.events.len().min(2 * offset) {
            return Ok(None);
        }
        serde_json::to_vec(&Continuation {
            year: self.context.year,
            offset,
            event_count: self.events.len(),
            events: OverviewRows {
                events: &events[offset..],
                personal: false,
                format: self.context.overview_format,
                roster: &self.context.employees,
            },
        }).map(Some)
    }

    pub(super) fn visible_year_rows(&self) -> YearRows {
        let rows = self.context.year_rows;
        if self.is_overview() && rows.select(&self.events).len() < self.events.len() {
            rows
        } else {
            YearRows::All
        }
    }

    pub(crate) fn is_overview(&self) -> bool {
        // A check or a note can add/remove personal tasks without changing the
        // number of annual rows or checklist items. The representation stays stable for deltas.
        if self.context.overview_format == OverviewFormat::Full {
            return false;
        }
        self.events.len() >= usize::from(FIRST_PAGE_ROWS)
            || (matches!(self.context.overview_format, OverviewFormat::RemoteTupleRows | OverviewFormat::AssignedTupleRows)
                && self.events.iter()
                    .map(|row| row.checklist.0.event.tasks.len())
                    .sum::<usize>() >= 128)
    }

    pub(crate) fn tuple_rows(&self) -> bool {
        self.context.overview_format.uses_tuples()
    }

    pub(super) fn compact_assignee(&self) -> Option<Id> {
        self.assigned_owner.filter(|_| self.is_overview())
    }

    pub(crate) fn policy_revision(&self) -> u64 {
        self.context.policy_revision
    }

    pub(crate) fn annual_digest_key(&self) -> Option<PublicYear> {
        self.context.public_rows_key().filter(|_| self.is_overview())
    }

    pub(crate) fn annual_rows(&self) -> &[EventView<'a>] {
        &self.events
    }

    pub(super) fn annual_row_index(&self, event: Id) -> Option<usize> {
        self.events.iter().position(|row| row.id == event)
    }

    pub(crate) fn personal_view(&self) -> (&ViewContext<'a>, &[EventView<'a>]) {
        (&self.context, &self.tasks)
    }

    pub(crate) fn event(&self, id: Id, year: i32) -> Option<&EventView<'a>> {
        self.events
            .iter()
            .chain(&self.tasks)
            .find(|event| event.id == id && event.year == year)
    }

    pub(super) fn write_full(&self, writer: impl std::io::Write) -> serde_json::Result<()> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct FullCards<'v, 'a> {
            #[serde(flatten)]
            context: &'v ViewContext<'a>,
            card_format: &'static str,
            #[serde(rename = "assignedOwnerId", skip_serializing_if = "Option::is_none")]
            assigned_owner: Option<Id>,
            events: CardRows<'v, 'a>,
            tasks: CardRows<'v, 'a>,
        }

        if self.context.packed_cards && !self.is_overview() {
            serde_json::to_writer(writer, &FullCards {
                context: &self.context,
                card_format: "tuple-v1",
                assigned_owner: self.assigned_owner.filter(|_| !self.events.is_empty() || !self.tasks.is_empty()),
                events: CardRows { rows: &self.events, annual: &[] },
                tasks: CardRows { rows: &self.tasks, annual: &self.events },
            })
        } else {
            serde_json::to_writer(writer, self)
        }
    }

    pub(crate) fn encode(&self) -> serde_json::Result<Vec<u8>> {
        if self.is_overview() {
            let events = self.context.year_rows.select(&self.events);
            serde_json::to_vec(&Overview {
                context: &self.context,
                overview: true,
                assigned_owner: self.assigned_owner,
                row_format: self.context.overview_format.uses_tuples().then_some("tuple-v2"),
                event_count: (events.len() < self.events.len()).then_some(self.events.len()),
                events: OverviewRows { events, personal: false, format: self.context.overview_format, roster: &self.context.employees },
                tasks: OverviewRows { events: &self.tasks, personal: true, format: self.context.overview_format, roster: &self.context.employees },
            })
        } else {
            let mut body = Vec::new();
            self.write_full(&mut body)?;
            Ok(body)
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
            events: rows(self.context.year_rows.select(&self.events), false),
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
            overview_format: OverviewFormat::Full,
            year_rows: YearRows::All,
            packed_cards: false,
            continuation: None,
            accepts_page_delta: false,
            workspace_name: &acl.name,
            employee_id: employee,
            employees,
            can_edit,
            allow_free_text: acl.protection().settings.free_text_allowed(),
            assigned_only,
            year,
        }
    }

    pub(crate) fn offer_overview(mut self, format: OverviewFormat) -> Self {
        self.overview_format = if self.assigned_only { OverviewFormat::Full } else { format };
        self
    }

    pub(super) fn offer_assigned_overview(mut self, enabled: bool) -> Self {
        if self.assigned_only && enabled {
            self.overview_format = OverviewFormat::AssignedTupleRows;
        }
        self
    }

    pub(super) fn offer_packed_cards(mut self, offered: bool) -> Self {
        self.packed_cards = offered && self.overview_format.uses_tuples();
        self
    }

    pub(super) fn offer_continuation(mut self, prior: Option<(YearRows, String)>) -> Self {
        if !self.assigned_only && self.overview_format.uses_tuples() {
            self.continuation = prior;
        }
        self
    }

    pub(super) fn offer_page(mut self, rows: YearRows) -> Self {
        if !self.assigned_only && self.overview_format.uses_tuples() {
            self.year_rows = rows;
        }
        self
    }

    pub(crate) fn public_rows_key(&self) -> Option<PublicYear> {
        (!self.assigned_only).then_some(PublicYear {
            year: self.year,
            allow_free_text: self.allow_free_text,
        })
    }
    pub(crate) fn event_rows(&self, data: &'a Data) -> Vec<EventView<'a>> {
        sorted_occurrences(
            data,
            self.year,
            self.assigned_only.then_some(self.employee_id),
        )
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
        let assigned_owner = (self.overview_format == OverviewFormat::AssignedTupleRows)
            .then(|| data.calendar.person_for(self.employee_id));
        let tasks = personal_events(&data.calendar, today, self.employee_id)
            .into_iter()
            .filter(|event| {
                assigned_owner.is_none_or(|owner| {
                    event.event.owner.is_some_and(|id| data.calendar.person_for(id) == owner)
                })
            })
            .map(|e| occurrence(data, e, self.allow_free_text))
            .collect();
        CalendarView {
            context: self,
            assigned_owner,
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
        #[serde(default)]
        only_if_assigned: bool,
    },
}
pub(crate) fn assigned_to(data: &Data, event: Occurrence<'_>, employee: Id) -> bool {
    event
        .event
        .owner
        .is_some_and(|owner| data.calendar.person_for(owner) == data.calendar.person_for(employee))
}

pub(crate) fn sorted_occurrences(
    data: &Data,
    year: i32,
    employee: Option<Id>,
) -> Vec<Occurrence<'_>> {
    let mut events: Vec<_> = data
        .calendar
        .occurrences(year)
        .filter(|event| employee.is_none_or(|id| assigned_to(data, *event, id)))
        .collect();
    events.sort_unstable_by_key(|event| (event.date, event.id));
    events
}

impl Command {
    pub fn allowed_for_assignee(&self, data: &Data, employee: Id) -> bool {
        match self {
            Self::Check { event_id, year, .. } | Self::Note { event_id, year, .. } => {
                valid_year(*year)
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
#[derive(Debug)]
pub(super) enum Rejection {
    Invalid(&'static str),
    Conflict(&'static str),
}

impl From<&'static str> for Rejection {
    fn from(message: &'static str) -> Self {
        Self::Invalid(message)
    }
}

fn stale() -> Rejection {
    Rejection::Conflict("Событие изменилось. Обновите календарь.")
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
pub(super) enum Mutation {
    Create,
    Edit,
    Delete,
    Check,
    Note,
}

impl Mutation {
    pub(super) fn audit_labels(self) -> (&'static str, &'static str) {
        match self {
            Self::Create => ("create", "create_requested"),
            Self::Edit => ("edit", "edit_requested"),
            Self::Delete => ("delete", "delete_requested"),
            Self::Check => ("check", "check_requested"),
            Self::Note => ("note", "note_requested"),
        }
    }
}

pub(super) fn apply(cmd: Command, data: &mut Data, acl: &Access) -> Result<(Mutation, Id), Rejection> {
    let year = match &cmd {
        Command::CreateEvent { fields } => fields.date.year(),
        Command::EditEvent { year, .. }
        | Command::DeleteEvent { year, .. }
        | Command::Check { year, .. }
        | Command::Note { year, .. } => *year,
    };
    if !valid_year(year) {
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
            Ok((Mutation::Create, id))
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
            Ok((Mutation::Edit, event_id))
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
            Ok((Mutation::Delete, event_id))
        }
        Command::Check {
            event_id,
            year,
            task_id,
            done,
            ..
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
            Ok((Mutation::Check, event_id))
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
            Ok((Mutation::Note, event_id))
        }
    }
}
