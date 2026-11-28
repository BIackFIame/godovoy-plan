use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        mpsc::{Receiver, TryRecvError},
    },
};

use chrono::NaiveDate;
use eframe::egui;
use godovoy_plan::{
    Fallible,
    planning::Reminder,
    storage::{ReminderRecovery, ReminderStore},
};

use super::spawn_worker;

#[derive(Default)]
pub(super) struct ReminderWork {
    progress: Progress,
    queued: BTreeSet<Reminder>,
    queued_date: Option<NaiveDate>,
}

#[derive(Clone)]
enum Operation {
    Repair,
    Save {
        today: NaiveDate,
        keys: Arc<[Reminder]>,
    },
}

enum Outcome {
    Recovered(ReminderRecovery),
    Saved,
}

#[derive(Default)]
enum Progress {
    #[default]
    Idle,
    Running {
        operation: Operation,
        result: Receiver<Fallible<Outcome>>,
    },
    Finished(Result<Outcome, String>),
}

pub(super) enum Status<'a> {
    Idle,
    Repairing,
    Saving,
    Recovered(ReminderRecovery),
    Failed(&'a str),
}

impl ReminderWork {
    pub(super) fn busy(&self) -> bool {
        matches!(self.progress, Progress::Running { .. }) || !self.queued.is_empty()
    }

    pub(super) fn status(&self) -> Status<'_> {
        match &self.progress {
            Progress::Running {
                operation: Operation::Repair,
                ..
            } => Status::Repairing,
            Progress::Running {
                operation: Operation::Save { .. },
                ..
            } => Status::Saving,
            Progress::Finished(Ok(Outcome::Recovered(result))) => Status::Recovered(*result),
            Progress::Finished(Err(error)) => Status::Failed(error),
            _ if !self.queued.is_empty() => Status::Saving,
            _ => Status::Idle,
        }
    }

    pub(super) fn submitted(&self, key: &Reminder) -> bool {
        self.active_keys().binary_search(key).is_ok() || self.queued.contains(key)
    }

    fn active_keys(&self) -> &[Reminder] {
        match &self.progress {
            Progress::Running {
                operation: Operation::Save { keys, .. },
                ..
            } => keys,
            _ => &[],
        }
    }

    pub(super) fn enqueue(&mut self, keys: &[Reminder], visible: &[Reminder], today: NaiveDate) {
        self.queued.retain(|key| visible.binary_search(key).is_ok());
        let active = match &self.progress {
            Progress::Running {
                operation: Operation::Save { keys, .. },
                ..
            } => &**keys,
            _ => &[],
        };
        self.queued.extend(
            keys.iter()
                .filter(|key| active.binary_search(key).is_err())
                .copied(),
        );
        self.queued_date = (!self.queued.is_empty()).then_some(today);
    }

    pub(super) fn repair(&mut self, store: &Arc<ReminderStore>, ctx: &egui::Context) {
        if !self.busy() {
            self.start(Operation::Repair, store, ctx);
        }
    }

    pub(super) fn poll(
        &mut self,
        store: &Arc<ReminderStore>,
        ctx: &egui::Context,
    ) -> Option<String> {
        if let Progress::Running { operation, result } = &self.progress {
            let result = match result.try_recv() {
                Ok(result) => result.map_err(|error| operation.error(&error.to_string())),
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => Err(operation.error("Операция прервана.")),
            };
            let error = result.as_ref().err().cloned();
            self.progress = Progress::Finished(result);
            if error.is_some() {
                self.queued.clear();
                self.queued_date = None;
                return error;
            }
        }
        if let Some(today) = self.queued_date.take() {
            let keys = std::mem::take(&mut self.queued)
                .into_iter()
                .collect::<Vec<_>>()
                .into();
            self.start(Operation::Save { today, keys }, store, ctx);
            if let Progress::Finished(Err(error)) = &self.progress {
                return Some(error.clone());
            }
        }
        None
    }

    fn start(&mut self, operation: Operation, store: &Arc<ReminderStore>, ctx: &egui::Context) {
        let worker_operation = operation.clone();
        let store = Arc::clone(store);
        self.progress = match spawn_worker("reminder-write", ctx, move || {
            let outcome = match worker_operation {
                Operation::Repair => store.recover().map(Outcome::Recovered),
                Operation::Save { today, keys } => {
                    store.acknowledge(today, &keys).map(|()| Outcome::Saved)
                }
            };
            Some(outcome)
        }) {
            Ok(result) => Progress::Running { operation, result },
            Err(error) => Progress::Finished(Err(operation.error(&error.to_string()))),
        };
        ctx.request_repaint();
    }
}

impl Operation {
    fn error(&self, reason: &str) -> String {
        match self {
            Self::Repair => format!("Не удалось восстановить отметки. {reason}"),
            Self::Save { .. } => format!("Не удалось сохранить отметки о прочтении. {reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use godovoy_plan::{model::Id, planning::Milestone};
    use std::sync::mpsc::sync_channel;

    use super::*;

    #[test]
    fn rapid_clicks_deduplicate_active_marks_and_queue_other_visible_rows() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let key = |id| Reminder {
            date: today,
            event: Id::from_u128(id),
            year: 2026,
            kind: Milestone::Preparation,
        };
        let (send, receive) = sync_channel(1);
        let mut work = ReminderWork {
            progress: Progress::Running {
                operation: Operation::Save {
                    today,
                    keys: Arc::from([key(1)]),
                },
                result: receive,
            },
            ..Default::default()
        };
        work.enqueue(&[key(2)], &[key(1), key(2), key(3)], today);
        work.enqueue(&[key(1), key(2), key(3)], &[key(1), key(2), key(3)], today);
        assert!(work.busy() && work.submitted(&key(1)) && work.submitted(&key(3)));
        assert_eq!(work.queued, BTreeSet::from([key(2), key(3)]));
        work.enqueue(&[key(3)], &[key(1), key(3)], today);
        assert_eq!(work.queued, BTreeSet::from([key(3)]));
        drop(send);
    }
}
