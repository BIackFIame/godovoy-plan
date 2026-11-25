use std::{
    collections::BTreeSet,
    sync::mpsc::{Receiver, TryRecvError, sync_channel},
    time::{Duration, Instant},
};

use godovoy_plan::{APP_NAME, Fallible, planning::Reminder};

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
];

enum Kind {
    Reminders(Vec<Reminder>),
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
    requested: BTreeSet<Reminder>,
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

    pub fn new_reminders(&mut self, pending: &[Reminder]) -> Vec<Reminder> {
        self.requested
            .retain(|item| pending.binary_search(item).is_ok());
        pending
            .iter()
            .copied()
            .filter(|item| !self.requested.contains(item))
            .collect()
    }

    pub fn reminders(&mut self, batch: Vec<Reminder>, now: Instant) {
        if !batch.is_empty() {
            self.start(Kind::Reminders(batch), now);
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
                self.requested.extend(batch);
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
                Kind::Reminders(batch).system_message().body(),
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
        delivery.reminders(Vec::new(), Instant::now());
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
        let mut delivery = Delivery::default();
        assert_eq!(delivery.new_reminders(&[item]), [item]);
        delivery.finish(Kind::Reminders(vec![item]), false, now);
        assert!(!delivery.ready(now));
        assert!(delivery.has_error());
        assert!(delivery.ready(now + RETRY_DELAYS[0]));
        assert_eq!(delivery.new_reminders(&[item]), [item]);
        delivery.finish(Kind::Test, true, now);
        assert_eq!(delivery.new_reminders(&[item]), [item]);
        delivery.finish(Kind::Reminders(vec![item]), true, now);
        assert!(!delivery.has_error());
        assert!(delivery.new_reminders(&[item]).is_empty());
        assert!(delivery.new_reminders(&[]).is_empty());
        assert_eq!(delivery.new_reminders(&[item]), [item]);
    }
}
