use super::{Session, alerts, year_check};
use chrono::{DateTime, Datelike, Local, NaiveDate};
use godovoy_plan::{
    Fallible,
    model::{Calendar, Event, Id, Occurrence, Record},
    planning::{Milestone, Reminder, due_reminders, personal_events},
    storage::{LocalState, SharedStore},
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
    last_sync: Option<i64>,
}

pub(super) struct LastState {
    snapshot: Weak<LocalState>,
    receipts: Weak<BTreeSet<Reminder>>,
    context: ViewContext,
    token: Id,
}

fn occurrence(calendar: &Calendar, e: Occurrence<'_>) -> Value {
    let tasks = e.event.tasks.iter().map(|t| json!({"id":t.id,"title":t.title,"dueDate":e.due(t),"daysBefore":t.days_before,"done":e.checked(t.id)})).collect::<Vec<_>>();
    json!({"id":e.id,"year":e.year,"title":e.event.title,"date":e.date,"description":e.event.description,
        "yearly":e.event.yearly,"category":e.event.category,"ownerId":e.event.owner,"employeeId":e.event.owner.map(|id|calendar.person_for(id)),
        "preparationDate":e.preparation_date(),"preparationDays":e.event.preparation_days,
        "note":e.note(),"revision":format!("{}:{}",e.revision.sequence,e.revision.operation),"checklist":tasks})
}

pub(super) fn state(session: &mut Session, year: i32, if_token: Option<Id>) -> Fallible<Value> {
    year_check(year)?;
    let (snapshot, receipts, paused) = {
        let guard = session.store.lock().unwrap();
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
        last_sync: status.last_sync.or(snapshot.last_sync),
    };
    // Weak identity survives address reuse, without retaining any retired keys or calendar data.
    let observed = Arc::downgrade(&snapshot);
    let acknowledged = Arc::downgrade(&receipts);
    let previous = session
        .last_state
        .as_ref()
        .filter(|item| {
            item.snapshot.ptr_eq(&observed)
                && item.receipts.ptr_eq(&acknowledged)
                && item.context == context
        })
        .map(|item| item.token);
    if previous.is_some() && if_token == previous {
        return Ok(json!({"unchanged":true}));
    }
    let token = previous.unwrap_or_else(Id::new_v4);
    let mut result = json!({"deviceId":snapshot.device,"employeeId":null,"employees":[],"workspaceName":null,"year":year,"members":[],"events":[],"tasks":[],"reminders":[],"notificationDates":[],
        "address":context.address, "networkError":context.warning, "paused":context.paused, "stateToken":token,
        "lastSync":context.last_sync.and_then(|s| DateTime::from_timestamp(s,0)).map(|d|d.to_rfc3339()),
        "canManage":false,"accessManagerId":null});
    session.last_state = Some(LastState {
        snapshot: observed,
        receipts: acknowledged,
        context,
        token,
    });
    if let Some(w) = &snapshot.workspace {
        let personal = personal_events(&w.calendar, today, snapshot.device);
        let reminders = due_reminders(&personal, today)
            .into_iter()
            .filter(|r| !receipts.contains(r))
            .filter_map(|r| {
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
                Some(json!({"eventId":r.event,"year":r.year,"title":title,"date":r.date}))
            })
            .collect::<Vec<_>>();
        result["notificationDates"] = json!(alerts::dates(&w.calendar, snapshot.device, today));
        result["employeeId"] = json!(w.calendar.person_for(snapshot.device));
        result["employees"] = json!(
            w.employees()
                .into_iter()
                .map(|(id, name)| json!({"id":id,"name":name}))
                .collect::<Vec<_>>()
        );
        result["workspaceName"] = json!(w.name);
        result["accessManagerId"] = json!(w.access_manager);
        result["canManage"] = json!(w.access_manager.is_none_or(|id| id == snapshot.device));
        result["members"] = json!(
            w.calendar
                .members
                .iter()
                .map(|(id, n)| json!({"id":id,"name":n.value,"revoked":w.is_revoked(*id),"employeeId":w.calendar.person_for(*id)}))
                .collect::<Vec<_>>()
        );
        result["events"] = Value::Array(
            w.calendar
                .year(year)
                .into_iter()
                .map(|e| occurrence(&w.calendar, e))
                .collect(),
        );
        result["tasks"] = Value::Array(
            personal
                .into_iter()
                .map(|e| occurrence(&w.calendar, e))
                .collect(),
        );
        result["reminders"] = Value::Array(reminders);
    }
    Ok(result)
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
    year_check(year)?;
    store.lock().unwrap().transaction(|s| {
        let revision = s.revision()?;
        let calendar = &mut s.workspace_mut()?.calendar;
        let e = calendar
            .follow_event(id, year)
            .ok_or("Событие больше недоступно.")?;
        if !e.event.tasks.iter().any(|t| t.id == task) {
            return Err("Пункт чек-листа изменился.".into());
        }
        if e.checked(task) == done {
            return Ok(());
        }
        let active_year = e.year;
        calendar
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
        let revision = s.revision()?;
        let calendar = &mut s.workspace_mut()?.calendar;
        let e = calendar
            .follow_event(id, year)
            .ok_or("Событие больше недоступно.")?;
        if e.note() == text {
            return Ok(());
        }
        if base.is_some_and(|base| base != e.note()) {
            return Err("Заметка уже изменилась. Откройте её заново.".into());
        }
        let active_year = e.year;
        calendar
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
