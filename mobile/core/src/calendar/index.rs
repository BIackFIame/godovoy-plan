use super::{Session, rows, success, year_check};
use chrono::{Local, NaiveDate};
use godovoy_plan::{
    Fallible,
    model::{Calendar, Id, Occurrence},
    planning::{Reminder, personal_event, personal_events, unread_reminder_page},
    storage::LocalState,
};
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

// The Java UI may hold both the old and the next snapshot during a refresh.
// Include repeated personal rows and potential reminder titles in the eager-view budget.
const EAGER_VIEW_BUDGET: usize = 2 * 1024 * 1024;
const MAX_REMINDER_PAGE: usize = 64;

pub(super) fn large(calendar: &Calendar, year: i32, personal: &[Occurrence<'_>]) -> bool {
    let mut budget = EAGER_VIEW_BUDGET;
    for (is_personal, row) in calendar
        .occurrences(year)
        .map(|r| (false, r))
        .chain(personal.iter().copied().map(|r| (true, r)))
    {
        let event = row.event;
        let mut bytes = 320 + event.title.len() + event.description.len() + row.note().len();
        for task in &event.tasks {
            bytes += 144 + task.title.len();
            if is_personal {
                bytes += 160 + event.title.len() + task.title.len();
            }
        }
        let Some(left) = budget.checked_sub(bytes) else {
            return true;
        };
        budget = left;
    }
    false
}

#[derive(Serialize)]
pub(super) struct Snapshot<'a> {
    #[serde(flatten)]
    metadata: Value,
    events: IndexRows<'a>,
    tasks: IndexRows<'a>,
}
impl<'a> Snapshot<'a> {
    pub(super) fn new(
        metadata: Value,
        calendar: &'a Calendar,
        year: i32,
        personal: Vec<Occurrence<'a>>,
    ) -> Self {
        Self {
            metadata,
            events: IndexRows {
                calendar,
                items: calendar.year(year),
                personal: false,
            },
            tasks: IndexRows {
                calendar,
                items: personal,
                personal: true,
            },
        }
    }
}
pub(super) fn personal_rows<'a>(
    calendar: &'a Calendar,
    items: Vec<Occurrence<'a>>,
) -> impl Serialize + 'a {
    IndexRows {
        calendar,
        items,
        personal: true,
    }
}

struct IndexRows<'a> {
    calendar: &'a Calendar,
    items: Vec<Occurrence<'a>>,
    personal: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexRow<'a> {
    id: Id,
    year: i32,
    title: &'a str,
    date: NaiveDate,
    owner_id: Option<Id>,
    employee_id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_open_task: Option<NextTask<'a>>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NextTask<'a> {
    id: Id,
    title: &'a str,
    due_date: NaiveDate,
}
impl Serialize for IndexRows<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.items.iter().copied().map(|row| {
            let event = row.event;
            let next_open_task = if self.personal {
                event
                    .tasks
                    .iter()
                    .filter(|t| !row.checked(t.id))
                    // The largest lead has the earliest deadline. Keep the first task on ties.
                    .min_by_key(|task| std::cmp::Reverse(task.days_before))
                    .map(|task| NextTask {
                        id: task.id,
                        title: &task.title,
                        due_date: row.due(task),
                    })
            } else {
                None
            };
            IndexRow {
                id: row.id,
                year: row.year,
                title: &event.title,
                date: row.date,
                owner_id: event.owner,
                employee_id: event.owner.map(|id| self.calendar.person_for(id)),
                next_open_task,
            }
        }))
    }
}

const STALE: &str = "Календарь изменился. Откройте актуальный список.";
type Current = (Arc<LocalState>, Arc<BTreeSet<Reminder>>);
fn current(session: &Session, token: Id, year: i32) -> Fallible<Current> {
    year_check(year)?;
    let view = session
        .last_state
        .as_ref()
        .filter(|v| {
            v.indexed
                && v.token == token
                && v.context.year == year
                && v.context.today == Local::now().date_naive()
        })
        .ok_or(STALE)?;
    let store = session.store.lock().map_err(|_| STALE)?;
    let state = store.snapshot();
    let receipts = store.reminder_receipts();
    let workspace = state.workspace.as_ref().ok_or(STALE)?;
    if store.successor_directory().is_some()
        || workspace.is_revoked(state.device)
        || !workspace.calendar.members.contains_key(&state.device)
        || !view.snapshot.ptr_eq(&Arc::downgrade(&state))
        || !view.receipts.ptr_eq(&Arc::downgrade(&receipts))
    {
        return Err(STALE.into());
    }
    let paused = store.sync_paused();
    drop(store);
    let (context, last_sync) = super::view_context(
        session,
        &state,
        paused,
        year,
        view.context.checklist_tuples,
        view.context.accepts_index,
    );
    if view.context != context || view.last_sync != last_sync {
        return Err(STALE.into());
    }
    Ok((state, receipts))
}
fn card_from(
    calendar: &Calendar,
    member: Id,
    today: NaiveDate,
    year: i32,
    event_year: i32,
    id: Id,
) -> Option<Occurrence<'_>> {
    if event_year == year {
        calendar.event(id, year)
    } else {
        personal_event(calendar, today, member, id, event_year)
    }
}
pub(crate) fn agenda_card(
    session: &Session,
    token: Id,
    year: i32,
    event_year: i32,
    id: Id,
) -> Fallible<String> {
    year_check(event_year)?;
    let (state, _) = current(session, token, year)?;
    let calendar = &state.workspace.as_ref().ok_or(STALE)?.calendar;
    let row = card_from(
        calendar,
        state.device,
        Local::now().date_naive(),
        year,
        event_year,
        id,
    )
    .ok_or(STALE)?;
    let reply = success(
        json!({"baseToken":token,"year":year,"events":rows::Rows{calendar,occurrences:vec![row],checklist_tuples:false}}),
    )?;
    current(session, token, year)?;
    Ok(reply)
}
pub(crate) fn reminder_page(
    session: &Session,
    token: Id,
    year: i32,
    offset: usize,
    limit: usize,
) -> Fallible<String> {
    if limit == 0 || limit > MAX_REMINDER_PAGE {
        return Err("Недопустимый размер страницы.".into());
    }
    let (state, receipts) = current(session, token, year)?;
    let calendar = &state.workspace.as_ref().ok_or(STALE)?.calendar;
    let today = Local::now().date_naive();
    let personal = personal_events(calendar, today, state.device);
    let mut reminders = Vec::new();
    for r in unread_reminder_page(&personal, today, &receipts, offset, limit) {
        reminders.push(super::reminder_row(calendar, r).ok_or(STALE)?);
    }
    let reply =
        success(json!({"baseToken":token,"year":year,"offset":offset,"reminders":reminders}))?;
    current(session, token, year)?;
    Ok(reply)
}
