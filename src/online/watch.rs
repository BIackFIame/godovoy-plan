//! Activity on the open year is advertised only for a view that could read the edit.
//! Periodic calendar reads remain authoritative across listeners, restarts and eviction.
use super::{Command, EntryPoint, actions};
use axum::http::HeaderValue;
use godovoy_plan::{
    model::Id,
    storage::online::{Access, Data},
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const CAPACITY: usize = 512;
const YEARS_PER_KEY: usize = 4;
const IDLE: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
struct Scope {
    year: i32,
    credential: Id,
    assignee: Option<Id>,
    text: bool,
}

#[derive(Clone, Copy)]
enum Field {
    Checklist,
    Note,
}

#[derive(Clone, Copy)]
pub(super) struct Edit {
    event: Id,
    year: i32,
    field: Field,
}

#[derive(Default)]
pub(super) struct Watches(BTreeMap<Scope, (HeaderValue, Instant)>);

impl Watches {
    pub(super) fn current(
        &mut self,
        acl: &Access,
        credential: Id,
        employee: Id,
        year: i32,
        register: bool,
    ) -> Option<HeaderValue> {
        let scope = Scope {
            year,
            credential,
            assignee: acl.assigned_only.contains(&employee).then_some(employee),
            text: acl.protection().settings.free_text_allowed(),
        };
        let now = Instant::now();
        if let Some((stamp, last)) = self.0.get_mut(&scope) {
            *last = now;
            return Some(stamp.clone());
        }
        if !register {
            return None;
        }
        self.0
            .retain(|_, (_, last)| now.duration_since(*last) < IDLE);
        if self.0.len() == CAPACITY
            || self.0.keys().filter(|s| s.credential == credential).count() >= YEARS_PER_KEY
        {
            return None;
        }
        let stamp = HeaderValue::from_static("0");
        self.0.insert(scope, (stamp.clone(), now));
        Some(stamp)
    }

    fn changed(&mut self, data: &Data, acl: &Access, entry: EntryPoint, edit: Edit) {
        let Some(event) = data.calendar.follow_event(edit.event, edit.year) else {
            return;
        };
        let now = Instant::now();
        let free_text = acl.protection().settings.free_text_allowed();
        for (scope, (stamp, last)) in &mut self.0 {
            let Some(grant) = acl.grants.get(&scope.credential) else {
                continue;
            };
            let assigned = acl
                .assigned_only
                .contains(&grant.employee)
                .then_some(grant.employee);
            if now.duration_since(*last) < IDLE
                && (entry != EntryPoint::Remote || grant.allow_remote)
                && scope.assignee == assigned
                && scope.text == free_text
                && scope.year == event.year
                && (!matches!(edit.field, Field::Note) || scope.text)
                && scope
                    .assignee
                    .is_none_or(|person| actions::assigned_to(data, event, person))
            {
                // Independent opaque values reveal neither counts nor another scope's activity.
                *stamp = HeaderValue::from_bytes(
                    Id::new_v4().simple().encode_lower(&mut [0; 32]).as_bytes(),
                )
                .expect("UUIDs use ASCII hex digits");
            }
        }
    }
}

pub(super) fn committed(
    watches: &std::sync::Mutex<Watches>,
    data: &Data,
    acl: &Access,
    entry: EntryPoint,
    change: Option<Edit>,
) {
    if let Some(edit) = change
        && let Ok(mut watches) = watches.lock()
    {
        watches.changed(data, acl, entry, edit);
    }
}

pub(super) fn edit(cmd: &Command, data: &Data) -> Option<Edit> {
    match cmd {
        Command::Check {
            event_id,
            year,
            task_id,
            done,
            ..
        } => {
            let event = data.calendar.follow_event(*event_id, *year)?;
            (event.checked(*task_id) != *done).then_some(Edit {
                event: *event_id,
                year: event.year,
                field: Field::Checklist,
            })
        }
        Command::Note {
            event_id,
            year,
            text,
            ..
        } => {
            let event = data.calendar.follow_event(*event_id, *year)?;
            (event.note() != text.trim()).then_some(Edit {
                event: *event_id,
                year: event.year,
                field: Field::Note,
            })
        }
        _ => None,
    }
}
