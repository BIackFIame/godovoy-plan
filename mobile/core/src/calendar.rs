mod index;
mod rows;
use super::{Session, alerts, success, year_check};
use chrono::{DateTime, Datelike, Local, NaiveDate};
use godovoy_plan::{
    Fallible,
    model::{Calendar, Event, Id, Record},
    planning::{
        Milestone, Reminder, personal_event, personal_events, unread_reminder_count,
        unread_reminders,
    },
    storage::{LocalState, SharedStore, Store},
};
pub(super) use index::{agenda_card, reminder_page};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    net::SocketAddrV4,
    sync::{Arc, Weak},
};

#[derive(PartialEq)]
struct ViewContext {
    year: i32,
    today: NaiveDate,
    address: Option<SocketAddrV4>,
    warning: Option<String>,
    paused: bool,
    checklist_tuples: bool,
    accepts_index: bool,
}

pub(super) struct LastState {
    snapshot: Weak<LocalState>,
    receipts: Weak<BTreeSet<Reminder>>,
    context: ViewContext,
    last_sync: Option<i64>,
    token: Id,
    check_result: Option<(Weak<LocalState>, Id)>,
    indexed: bool,
}

#[derive(serde::Serialize)]
struct FullSnapshot<'a> {
    #[serde(flatten)]
    metadata: Value,
    events: rows::Rows<'a>,
    tasks: rows::Rows<'a>,
}

fn view_context(
    session: &Session,
    snapshot: &LocalState,
    paused: bool,
    year: i32,
    accept_checklist_tuples: bool,
    accept_agenda_index: bool,
) -> (ViewContext, Option<i64>) {
    let status = session
        .network
        .as_ref()
        .map(|n| n.status())
        .unwrap_or_default();
    let today = Local::now().date_naive();
    let context = ViewContext {
        checklist_tuples: accept_checklist_tuples,
        accepts_index: accept_agenda_index,
        year,
        today,
        address: session.network.as_ref().and_then(|n| n.address()),
        warning: session
            .network_error
            .clone()
            .or(status.merge_error)
            .or(status.warning)
            .or(status.route_warning),
        paused: session.network.as_ref().map_or(paused, |n| n.paused()),
    };
    (context, status.last_sync.or(snapshot.last_sync))
}

fn reminder_row(calendar: &Calendar, reminder: Reminder) -> Option<Value> {
    let occurrence = calendar.event(reminder.event, reminder.year)?;
    let event = occurrence.event;
    let (title, task_id) = match reminder.kind {
        Milestone::Task(id) => {
            let task = event.tasks.iter().find(|task| task.id == id)?;
            (format!("{} · {}", event.title, task.title), Some(id))
        }
        _ => (event.title.clone(), None),
    };
    Some(
        json!({"eventId":reminder.event,"year":reminder.year,"title":title,"date":reminder.date,"taskId":task_id}),
    )
}

#[derive(Default)]
pub(super) struct CheckPatch {
    pub full: bool,
    pub indexed: bool,
}

impl CheckPatch {
    fn accepts(&self, indexed: bool) -> bool {
        if indexed { self.indexed } else { self.full }
    }
}

pub(super) fn state(
    session: &mut Session,
    year: i32,
    if_token: Option<Id>,
    accept_sync_stamp: bool,
    accept_check_patch: CheckPatch,
    accept_checklist_tuples: bool,
    accept_agenda_index: bool,
) -> Fallible<String> {
    year_check(year)?;
    let (snapshot, receipts, paused) = {
        let guard = session.store.lock().map_err(|_| "Профиль недоступен.")?;
        let state = guard.state();
        if guard.successor_directory().is_some()
            || state.workspace.as_ref().is_some_and(|w| {
                w.is_revoked(state.device) || !w.calendar.members.contains_key(&state.device)
            })
        {
            return Err("Доступ к этому профилю изменился. Откройте актуальный календарь.".into());
        }
        (
            guard.snapshot(),
            guard.reminder_receipts(),
            guard.sync_paused(),
        )
    };
    let (context, last_sync) = view_context(
        session,
        &snapshot,
        paused,
        year,
        accept_checklist_tuples,
        accept_agenda_index,
    );
    let today = context.today;
    // Weak identity survives address reuse, without retaining any retired keys or calendar data.
    let observed = Arc::downgrade(&snapshot);
    let acknowledged = Arc::downgrade(&receipts);
    let changed = session.last_state.as_ref().and_then(|prior| {
        let (after, id) = prior.check_result.as_ref()?;
        (accept_check_patch.accepts(prior.indexed)
            && Some(prior.token) == if_token
            && after.ptr_eq(&observed)
            && prior.receipts.ptr_eq(&acknowledged)
            && prior.context == context
            && prior.last_sync == last_sync)
            .then_some((*id, prior.token, prior.indexed))
    });
    let previous = session.last_state.as_mut().filter(|item| {
        item.snapshot.ptr_eq(&observed)
            && item.receipts.ptr_eq(&acknowledged)
            && item.context == context
    });
    let timestamp = || {
        last_sync
            .and_then(|s| DateTime::from_timestamp(s, 0))
            .map(|d| d.to_rfc3339())
    };
    let reusable = if let Some(prior) = previous {
        if Some(prior.token) == if_token {
            if prior.last_sync == last_sync {
                return success(json!({"unchanged":true}));
            }
            if accept_sync_stamp {
                let base = prior.token;
                prior.token = Id::new_v4();
                prior.last_sync = last_sync;
                return success(
                    json!({"syncStamp":true,"year":year,"baseToken":base,"lastSync":timestamp(),"stateToken":prior.token}),
                );
            }
        }
        (prior.last_sync == last_sync).then_some(prior.token)
    } else {
        None
    };
    let token = reusable.unwrap_or_else(Id::new_v4);
    let mut result = json!({"deviceId":snapshot.device,"employeeId":null,"employees":[],"workspaceName":null,"year":year,"members":[],"reminders":[],"notificationDates":[],
        "lanEnabled":snapshot.lan_enabled(), "address":context.address, "networkError":context.warning, "paused":context.paused, "stateToken":token,
        "supportsSyncStamp":true,"supportsCheckPatch":true,"lastSync":timestamp(),
        "canManage":false,"accessManagerId":null});
    if accept_checklist_tuples {
        result["checklistFormat"] = json!("tuple-v1");
    }
    session.last_state = Some(LastState {
        check_result: None,
        indexed: false,
        snapshot: observed,
        receipts: acknowledged,
        context,
        last_sync,
        token,
    });
    if let Some(w) = &snapshot.workspace {
        let mut personal = personal_events(&w.calendar, today, snapshot.device);
        let indexed = accept_agenda_index && index::large(&w.calendar, year, &personal);
        let changed_row = changed
            .filter(|(_, _, was_indexed)| *was_indexed == indexed)
            .and_then(|(id, base, _)| w.calendar.event(id, year).map(|row| (row, base)));
        let mut reminder_positions = Vec::new();
        let mut reminder_count = 0;
        if indexed {
            result["agendaIndex"] = json!(true);
            result["supportsIndexCheckPatch"] = json!(true);
            result["reminderCount"] = json!(unread_reminder_count(&personal, today, &receipts));
        } else {
            let reminders = unread_reminders(&personal, today, &receipts)
                .into_iter()
                .filter_map(|r| {
                    if changed_row.is_some_and(|(row, _)| row.id != r.event) {
                        reminder_count += 1;
                        return None;
                    }
                    let row = reminder_row(&w.calendar, r)?;
                    if changed_row.is_some() {
                        reminder_positions.push(reminder_count);
                    }
                    reminder_count += 1;
                    Some(row)
                })
                .collect();
            result["reminders"] = Value::Array(reminders);
        }
        result["notificationDates"] = json!(alerts::dates(&w.calendar, snapshot.device, today));
        result["employeeId"] = json!(w.calendar.person_for(snapshot.device));
        result["employees"] = Value::Array(
            w.employees()
                .into_iter()
                .map(|(id, name)| json!({"id":id,"name":name}))
                .collect::<Vec<_>>(),
        );
        result["workspaceName"] = json!(w.name);
        result["accessManagerId"] = json!(w.access_manager);
        result["canManage"] = json!(w.access_manager.is_none_or(|id| id == snapshot.device));
        result["members"] = Value::Array(
            w.calendar
                .members
                .iter()
                .map(|(id, n)| json!({"id":id,"name":n.value,"revoked":w.is_revoked(*id),"employeeId":w.calendar.person_for(*id)}))
                .collect::<Vec<_>>()
        );
        let events = if let Some((row, base)) = changed_row {
            let mut task_positions = Vec::new();
            let task_count = personal.len();
            let mut position = 0;
            personal.retain(|e| {
                let keep = e.id == row.id;
                if keep {
                    task_positions.push(position);
                }
                position += 1;
                keep
            });
            result["taskPositions"] = json!(task_positions);
            result["taskCount"] = json!(task_count);
            result["checkPatch"] = json!(true);
            result["baseToken"] = json!(base);
            if !indexed {
                result["reminderPositions"] = json!(reminder_positions);
                result["reminderCount"] = json!(reminder_count);
            }
            Some(vec![row])
        } else {
            None
        };
        if indexed {
            let reply = if let Some(events) = events {
                result["tasks"] = json!(index::personal_rows(&w.calendar, personal));
                result["events"] = json!(rows::Rows {
                    calendar: &w.calendar,
                    occurrences: events,
                    checklist_tuples: false
                });
                success(result)?
            } else {
                success(index::Snapshot::new(result, &w.calendar, year, personal))?
            };
            if let Some(view) = session.last_state.as_mut() {
                view.indexed = true;
            }
            return Ok(reply);
        }
        let events = events.unwrap_or_else(|| w.calendar.year(year));
        return success(FullSnapshot {
            metadata: result,
            events: rows::Rows {
                calendar: &w.calendar,
                checklist_tuples: accept_checklist_tuples,
                occurrences: events,
            },
            tasks: rows::Rows {
                calendar: &w.calendar,
                checklist_tuples: accept_checklist_tuples,
                occurrences: personal,
            },
        });
    }
    result["events"] = Value::Array(Vec::new());
    result["tasks"] = Value::Array(Vec::new());
    success(result)
}

pub(super) fn create(store: &SharedStore, event: Event) -> Fallible<()> {
    event.validate()?;
    store.lock().unwrap().transaction(|s| {
        let revision = s.revision()?;
        let w = s.workspace_mut()?;
        w.check_assignment(None, event.owner)?;
        w.calendar
            .plans
            .entry(Id::new_v4())
            .or_default()
            .templates
            .insert(
                event.date.year(),
                Record::new(revision, Some(Arc::new(event))),
            );
        Ok(())
    })
}

pub(super) fn check(store: &SharedStore, id: Id, year: i32, task: Id, done: bool) -> Fallible<()> {
    let mut guard = store.lock().map_err(|_| "Профиль недоступен.")?;
    checked(&mut guard, id, year, task, done)
}

pub(super) fn require_event_view(
    store: &Store,
    prior: Option<&LastState>,
    token: Option<Id>,
    view_year: Option<i32>,
    id: Id,
    year: i32,
) -> Fallible<()> {
    let Some(token) = token else {
        return if view_year.is_none() {
            Ok(())
        } else {
            Err("Для правки из открытого календаря требуется актуальный снимок.".into())
        };
    };
    let view_year = view_year.unwrap_or(year);
    let before = store.snapshot();
    let w = before.workspace.as_ref().ok_or("Профиль недоступен.")?;
    if store.successor_directory().is_some()
        || w.is_revoked(before.device)
        || !w.calendar.members.contains_key(&before.device)
    {
        return Err("Доступ этого устройства к календарю закрыт.".into());
    }
    let prior = prior
        .filter(|view| {
            view.token == token
                && view.context.year == view_year
                && view
                    .receipts
                    .ptr_eq(&Arc::downgrade(&store.reminder_receipts()))
                && view.context.today == Local::now().date_naive()
                && view.snapshot.ptr_eq(&Arc::downgrade(&before))
        })
        .ok_or("Календарь изменился. Откройте событие заново.")?;
    if year != view_year
        && personal_event(&w.calendar, prior.context.today, before.device, id, year).is_none()
    {
        return Err("Событие больше недоступно в личных задачах.".into());
    }
    Ok(())
}

pub(super) fn check_from_view(
    session: &mut Session,
    token: Id,
    view_year: i32,
    id: Id,
    year: i32,
    task: Id,
    done: bool,
) -> Fallible<()> {
    let mut store = session.store.lock().map_err(|_| "Профиль недоступен.")?;
    let before = store.snapshot();
    require_event_view(
        &store,
        session.last_state.as_ref(),
        Some(token),
        Some(view_year),
        id,
        year,
    )?;
    let prior = session.last_state.as_mut().ok_or("Календарь изменился.")?;
    checked(&mut store, id, year, task, done)?;
    let after = store.snapshot();
    prior.check_result =
        (year == view_year && !Arc::ptr_eq(&before, &after)).then(|| (Arc::downgrade(&after), id));
    Ok(())
}
fn checked(store: &mut Store, id: Id, year: i32, task: Id, done: bool) -> Fallible<()> {
    year_check(year)?;
    store.transaction(|s| {
        let e = s.follow_event(id, year)?;
        if !e.event.tasks.iter().any(|t| t.id == task) {
            return Err("Пункт чек-листа изменился.".into());
        }
        if e.checked(task) == done {
            return Ok(());
        }
        let active_year = e.year;
        let revision = s.revision()?;
        s.workspace_mut()?
            .calendar
            .plans
            .get_mut(&id)
            .ok_or("Событие больше недоступно.")?
            .years
            .entry(active_year)
            .or_default()
            .checks
            .insert(task, Record::new(revision, done));
        Ok(())
    })
}

pub(super) fn note(
    session: &Session,
    view_year: Option<i32>,
    token: Option<Id>,
    id: Id,
    year: i32,
    text: &str,
    base: Option<&str>,
) -> Fallible<()> {
    year_check(year)?;
    let text = text.trim();
    if text.chars().count() > 4000 {
        return Err("Максимальная длина заметки — 4000 символов.".into());
    }
    let mut store = session.store.lock().map_err(|_| "Профиль недоступен.")?;
    require_event_view(
        &store,
        session.last_state.as_ref(),
        token,
        view_year,
        id,
        year,
    )?;
    store.transaction(|s| {
        let e = s.follow_event(id, year)?;
        if e.note() == text {
            return Ok(());
        }
        if base.is_some_and(|base| base != e.note()) {
            return Err("Заметка уже изменилась. Откройте её заново.".into());
        }
        let active_year = e.year;
        let revision = s.revision()?;
        s.workspace_mut()?
            .calendar
            .plans
            .get_mut(&id)
            .ok_or("Событие больше недоступно.")?
            .years
            .entry(active_year)
            .or_default()
            .note = Some(Record::new(revision, text.to_owned()));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(session: &mut Session, year: i32, token: Option<Id>, compact: bool) -> Value {
        let response = state(
            session,
            year,
            token,
            compact,
            CheckPatch::default(),
            false,
            false,
        )
        .unwrap();
        let mut decoded: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(decoded["ok"], true);
        decoded["data"].take()
    }

    #[test]
    fn sync_stamp_requires_exact_view_and_is_consumed_once() {
        let root = tempfile::tempdir().unwrap();
        let mut session = Session::load(&root.path().join("phone"), false).unwrap();
        let original = snapshot(&mut session, 2027, None, false);
        let original_token = serde_json::from_value(original["stateToken"].clone()).unwrap();
        assert_eq!(original["supportsSyncStamp"], true);
        // Isolate a change to the displayed timestamp; real LAN transitions have a separate test.
        session.last_state.as_mut().unwrap().last_sync = Some(1);
        let update = snapshot(&mut session, 2027, Some(original_token), true);
        assert_eq!(update["syncStamp"], true);
        assert_eq!(update["year"], 2027);
        assert_eq!(update["baseToken"], original["stateToken"]);
        assert!(update.get("events").is_none() && update.get("lastSync").is_some());
        let token = serde_json::from_value(update["stateToken"].clone()).unwrap();
        assert_ne!(token, original_token);
        assert_eq!(
            snapshot(&mut session, 2027, Some(token), true),
            json!({"unchanged":true})
        );
        let mut patched = original;
        patched["lastSync"] = update["lastSync"].clone();
        patched["stateToken"] = update["stateToken"].clone();
        assert_eq!(
            snapshot(&mut session, 2027, Some(original_token), true),
            patched
        );
        session.last_state.as_mut().unwrap().last_sync = Some(2);
        assert!(snapshot(&mut session, 2027, Some(token), false)["events"].is_array());
        assert!(snapshot(&mut session, 2028, Some(token), true)["events"].is_array());
        let token = session.last_state.as_ref().unwrap().token;
        session.network_error = Some("Sync interrupted".into());
        let warning = snapshot(&mut session, 2028, Some(token), true);
        assert_eq!(warning["networkError"], "Sync interrupted");
        let cache = session.last_state.as_mut().unwrap();
        cache.context.today = cache.context.today.pred_opt().unwrap();
        let token = cache.token;
        assert!(snapshot(&mut session, 2028, Some(token), true)["events"].is_array());
    }
}
