mod rows;
use super::{Session, alerts, success, year_check};
use chrono::{DateTime, Datelike, Local, NaiveDate};
use godovoy_plan::{
    Fallible,
    model::{Event, Id, Record},
    planning::{Milestone, Reminder, due_reminders, personal_events},
    storage::{LocalState, SharedStore, Store},
};
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
}

pub(super) struct LastState {
    snapshot: Weak<LocalState>,
    receipts: Weak<BTreeSet<Reminder>>,
    context: ViewContext,
    last_sync: Option<i64>,
    token: Id,
    check_result: Option<(Weak<LocalState>, Id)>,
}

#[derive(serde::Serialize)]
struct FullSnapshot<'a> {
    #[serde(flatten)]
    metadata: Value,
    events: rows::Rows<'a>,
    tasks: rows::Rows<'a>,
}

pub(super) fn state(
    session: &mut Session,
    year: i32,
    if_token: Option<Id>,
    accept_sync_stamp: bool,
    accept_check_patch: bool,
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
    let status = session
        .network
        .as_ref()
        .map(|n| n.status())
        .unwrap_or_default();
    let today = Local::now().date_naive();
    let context = ViewContext {
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
    let last_sync = status.last_sync.or(snapshot.last_sync);
    // Weak identity survives address reuse, without retaining any retired keys or calendar data.
    let observed = Arc::downgrade(&snapshot);
    let acknowledged = Arc::downgrade(&receipts);
    let changed = session.last_state.as_ref().and_then(|prior| {
        let (after, id) = prior.check_result.as_ref()?;
        (accept_check_patch
            && Some(prior.token) == if_token
            && after.ptr_eq(&observed)
            && prior.receipts.ptr_eq(&acknowledged)
            && prior.context == context
            && prior.last_sync == last_sync)
            .then_some((*id, prior.token))
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
    session.last_state = Some(LastState {
        check_result: None,
        snapshot: observed,
        receipts: acknowledged,
        context,
        last_sync,
        token,
    });
    if let Some(w) = &snapshot.workspace {
        let changed_row =
            changed.and_then(|(id, base)| w.calendar.event(id, year).map(|row| (row, base)));
        let mut personal = personal_events(&w.calendar, today, snapshot.device);
        let mut reminder_positions = Vec::new();
        let mut reminder_count = 0;
        let reminders = due_reminders(&personal, today)
            .into_iter()
            .filter(|r| !receipts.contains(r))
            .filter_map(|r| {
                if changed_row.is_some_and(|(row, _)| row.id != r.event) {
                    // Each reminder was just derived from a visible live occurrence and its own task.
                    reminder_count += 1;
                    return None;
                }
                let e = w.calendar.event(r.event, r.year)?;
                let title = match r.kind {
                    Milestone::Task(id) => e
                        .event
                        .tasks
                        .iter()
                        .find(|t| t.id == id)
                        .map(|t| format!("{} · {}", e.event.title, t.title))?,
                    _ => e.event.title.clone(),
                };
                if changed_row.is_some() {
                    reminder_positions.push(reminder_count);
                }
                reminder_count += 1;
                Some(json!({"eventId":r.event,"year":r.year,"title":title,"date":r.date}))
            })
            .collect::<Vec<_>>();
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
        result["reminders"] = Value::Array(reminders);
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
            result["reminderPositions"] = json!(reminder_positions);
            result["reminderCount"] = json!(reminder_count);
            result["checkPatch"] = json!(true);
            result["baseToken"] = json!(base);
            vec![row]
        } else {
            w.calendar.year(year)
        };
        return success(FullSnapshot {
            metadata: result,
            events: rows::Rows {
                calendar: &w.calendar,
                occurrences: events,
            },
            tasks: rows::Rows {
                calendar: &w.calendar,
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

pub(super) fn check_from_view(
    session: &mut Session,
    token: Id,
    id: Id,
    year: i32,
    task: Id,
    done: bool,
) -> Fallible<()> {
    let mut store = session.store.lock().map_err(|_| "Профиль недоступен.")?;
    let before = store.snapshot();
    let w = before.workspace.as_ref().ok_or("Профиль недоступен.")?;
    if store.successor_directory().is_some()
        || w.is_revoked(before.device)
        || !w.calendar.members.contains_key(&before.device)
    {
        return Err("Доступ этого устройства к календарю закрыт.".into());
    }
    let prior = session
        .last_state
        .as_mut()
        .filter(|view| {
            view.token == token
                && view.context.year == year
                && view.context.today == Local::now().date_naive()
                && view.snapshot.ptr_eq(&Arc::downgrade(&before))
        })
        .ok_or("Календарь изменился. Проверьте обновлённый пункт перед отметкой.")?;
    checked(&mut store, id, year, task, done)?;
    let after = store.snapshot();
    prior.check_result = (!Arc::ptr_eq(&before, &after)).then(|| (Arc::downgrade(&after), id));
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
    store: &SharedStore,
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
    store.lock().unwrap().transaction(|s| {
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
        let response = state(session, year, token, compact, false).unwrap();
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
