use std::{
    sync::{
        Arc,
        mpsc::{Receiver, TryRecvError, sync_channel},
    },
    time::{Duration, Instant},
};

use godovoy_plan::{APP_NAME, Fallible, planning::Reminder};

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
];

enum Kind {
    Reminders(Arc<[Reminder]>),
    Test,
}

impl Kind {
    fn system_message(&self) -> SystemMessage {
        match self {
            Self::Reminders(_) => SystemMessage::Reminders,
            Self::Test => SystemMessage::Test,
        }
    }
}

// Only these messages can reach the OS, including its lock screen.
enum SystemMessage {
    Reminders,
    Test,
}

impl SystemMessage {
    fn body(&self) -> &'static str {
        match self {
            Self::Reminders => "Откройте раздел «Напоминания» в приложении.",
            Self::Test => "Это пробное напоминание «Годового плана».",
        }
    }
}

struct Job {
    kind: Kind,
    result: Receiver<bool>,
}

#[derive(Clone, Copy)]
pub enum TestStatus {
    Sending,
    Requested,
    Failed,
}

#[derive(Default)]
pub struct Delivery {
    requested: Option<Arc<[Reminder]>>,
    job: Option<Job>,
    retry_at: Option<Instant>,
    failures: usize,
    pub test_status: Option<TestStatus>,
}

impl Delivery {
    pub fn ready(&self, now: Instant) -> bool {
        self.job.is_none() && self.retry_at.is_none_or(|retry| now >= retry)
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub fn has_error(&self) -> bool {
        self.retry_at.is_some()
    }

    fn needs_notification(&mut self, pending: &Arc<[Reminder]>) -> bool {
        let Some(previous) = &self.requested else {
            return !pending.is_empty();
        };
        if Arc::ptr_eq(previous, pending) {
            return false;
        }
        // Both snapshots contain distinct reminder keys in their natural order.
        let mut acknowledged = previous.iter();
        if pending
            .iter()
            .all(|key| acknowledged.by_ref().any(|old| old == key))
        {
            self.requested = Some(Arc::clone(pending));
            return false;
        }
        // Forget withdrawn reminders even if sending the new alert fails.
        let mut current = pending.iter();
        if !previous
            .iter()
            .all(|key| current.by_ref().any(|new| new == key))
        {
            self.requested = Some(
                previous
                    .iter()
                    .copied()
                    .filter(|key| pending.binary_search(key).is_ok())
                    .collect(),
            );
        }
        true
    }

    pub fn reminders(&mut self, pending: &Arc<[Reminder]>, now: Instant) {
        if self.needs_notification(pending) {
            self.start(Kind::Reminders(Arc::clone(pending)), now);
        }
    }

    pub fn test(&mut self, now: Instant) {
        self.test_status = Some(TestStatus::Sending);
        self.start(Kind::Test, now);
    }

    pub fn poll(&mut self, now: Instant) {
        let accepted = match self.job.as_ref().map(|job| job.result.try_recv()) {
            Some(Ok(value)) => value,
            Some(Err(TryRecvError::Disconnected)) => false,
            _ => return,
        };
        if let Some(job) = self.job.take() {
            self.finish(job.kind, accepted, now);
        }
    }

    fn start(&mut self, kind: Kind, now: Instant) {
        let (send, result) = sync_channel(1);
        let message = kind.system_message();
        let worker = std::thread::Builder::new()
            .name("local-notification".into())
            .spawn(move || {
                let _ = send.send(send_to_system(message).is_ok());
            });
        match worker {
            Ok(_) => self.job = Some(Job { kind, result }),
            Err(_) => self.finish(kind, false, now),
        }
    }

    fn finish(&mut self, kind: Kind, accepted: bool, now: Instant) {
        match kind {
            Kind::Test => {
                self.test_status = Some(if accepted {
                    TestStatus::Requested
                } else {
                    TestStatus::Failed
                });
                if accepted {
                    self.retry_at = None;
                    self.failures = 0;
                }
            }
            Kind::Reminders(batch) if accepted => {
                self.requested = Some(batch);
                self.retry_at = None;
                self.failures = 0;
            }
            Kind::Reminders(_) => {
                self.retry_at = Some(now + RETRY_DELAYS[self.failures.min(RETRY_DELAYS.len() - 1)]);
                self.failures = self.failures.saturating_add(1);
            }
        }
    }
}

fn send_to_system(message: SystemMessage) -> Fallible<()> {
    #[cfg(target_os = "macos")]
    if matches!(&message, SystemMessage::Test) && !notify_rust::request_auth_blocking()? {
        return Err("Системное разрешение на уведомления не предоставлено.".into());
    }
    notify_rust::Notification::new()
        .appname(APP_NAME)
        .summary(APP_NAME)
        .body(message.body())
        .show()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use godovoy_plan::{model::Id, planning::Milestone};

    use super::*;

    #[test]
    fn system_reminders_show_the_same_message_for_preparation_and_checklist_batches() {
        let preparation = Reminder {
            date: NaiveDate::from_ymd_opt(2026, 12, 18).unwrap(),
            event: Id::new_v4(),
            year: 2027,
            kind: Milestone::Preparation,
        };
        let task = Reminder {
            kind: Milestone::Task(Id::new_v4()),
            ..preparation
        };
        for batch in [vec![preparation], vec![task], vec![preparation, task]] {
            assert_eq!(
                Kind::Reminders(batch.into()).system_message().body(),
                "Откройте раздел «Напоминания» в приложении."
            );
        }
        assert_eq!(
            Kind::Test.system_message().body(),
            "Это пробное напоминание «Годового плана»."
        );
    }

    #[test]
    fn empty_reminder_batch_does_not_start_a_system_request() {
        let mut delivery = Delivery::default();
        delivery.reminders(&Arc::from([]), Instant::now());
        assert!(!delivery.busy());
    }

    #[test]
    fn delivery_failure_retries_without_consuming_unread_keys() {
        let now = Instant::now();
        let item = Reminder {
            date: NaiveDate::from_ymd_opt(2026, 12, 18).unwrap(),
            event: Id::new_v4(),
            year: 2027,
            kind: Milestone::Preparation,
        };
        let pending = Arc::from([item]);
        let mut delivery = Delivery::default();
        assert!(delivery.needs_notification(&pending));
        delivery.finish(Kind::Reminders(pending.clone()), false, now);
        assert!(!delivery.ready(now));
        assert!(delivery.has_error());
        assert!(delivery.ready(now + RETRY_DELAYS[0]));
        assert!(delivery.needs_notification(&pending));
        delivery.finish(Kind::Test, true, now);
        assert!(delivery.needs_notification(&pending));
        delivery.finish(Kind::Reminders(pending.clone()), true, now);
        assert!(!delivery.has_error());
        assert!(!delivery.needs_notification(&pending));
        assert!(!delivery.needs_notification(&Arc::from([])));
        assert!(delivery.needs_notification(&pending));
    }

    #[test]
    fn changed_snapshots_preserve_only_successfully_requested_visible_keys() {
        let now = Instant::now();
        let keys = |mask: u8| -> Arc<[Reminder]> {
            (0..4)
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| Reminder {
                    date: NaiveDate::MIN,
                    event: Id::from_u128(i),
                    year: 2026,
                    kind: Milestone::Preparation,
                })
                .collect()
        };
        for prior in 0..16 {
            for next in 0..16 {
                for accepted in [false, true] {
                    let mut delivery = Delivery::default();
                    delivery.finish(Kind::Reminders(keys(prior)), true, now);
                    let pending = keys(next);
                    let needed = delivery.needs_notification(&pending);
                    assert_eq!(needed, next & !prior != 0);
                    if needed {
                        delivery.finish(Kind::Reminders(pending), accepted, now);
                    }
                    let retained = if accepted { next } else { prior & next };
                    assert_eq!(delivery.requested.as_deref().unwrap(), &*keys(retained));
                }
            }
        }
    }
}
