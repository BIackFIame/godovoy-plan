//! A live standby process remains tied to the one epoch it started in.
use chrono::Utc;
use godovoy_plan::{Fallible, model::Id, storage::online::LockedVault};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

#[derive(Clone, Default)]
pub(super) enum Mode {
    #[default]
    Primary,
    Standby(Arc<Mutex<Clock>>),
}
pub(super) struct Clock {
    epoch: Id,
    at: Instant,
    effective: i64,
}
impl Mode {
    pub(super) fn standby(vault: &LockedVault<'_>) -> Fallible<Self> {
        let window = vault
            .standby_window()?
            .ok_or("Сначала откройте окно через standby-begin.")?;
        let window = vault.observe_standby(window.id, Utc::now().timestamp(), 0)?;
        Ok(Self::Standby(Arc::new(Mutex::new(Clock {
            epoch: window.id,
            at: Instant::now(),
            effective: window.effective_time(),
        }))))
    }
    pub(super) fn check(&self, vault: &LockedVault<'_>) -> Fallible<Option<i64>> {
        self.check_at(vault, Utc::now().timestamp(), Instant::now())
    }
    pub(super) fn check_at(
        &self,
        vault: &LockedVault<'_>,
        wall: i64,
        now: Instant,
    ) -> Fallible<Option<i64>> {
        match self {
            Self::Primary => {
                if vault.standby_window()?.is_some() {
                    return Err("Основной режим закрыт до standby-end.".into());
                }
                Ok(None)
            }
            Self::Standby(clock) => {
                let mut clock = clock.lock().map_err(|_| "Ошибка часов резервного узла.")?;
                let elapsed = now
                    .checked_duration_since(clock.at)
                    .ok_or("Ошибка монотонных часов.")?
                    .as_secs();
                let floor = clock
                    .effective
                    .saturating_add(i64::try_from(elapsed).unwrap_or(i64::MAX));
                let window = vault.observe_standby(clock.epoch, wall, floor)?;
                // Advance the anchor only when the persisted or real clock moved ahead;
                // resetting it for every poll would discard fractional seconds.
                if window.effective_time() > floor {
                    clock.at = now;
                    clock.effective = window.effective_time();
                }
                Ok(Some(window.deadline))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use godovoy_plan::{model::Calendar, storage::online::OnlineVault};
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn frozen_or_reversed_clock_cannot_reopen_an_observed_deadline() {
        let tmp = TempDir::new().unwrap();
        let vault = OnlineVault::init(
            &tmp.path().join("vault"),
            "UTC".into(),
            Calendar::default(),
            Default::default(),
        )
        .unwrap();
        let lock = vault.lock().unwrap();
        let first = lock.begin_standby(1).unwrap();
        let gate = Mode::standby(&lock).unwrap();
        let wall = lock.standby_window().unwrap().unwrap().effective_time();
        let now = Instant::now();
        let seconds = Duration::from_secs(1);
        assert!(gate.check_at(&lock, wall, now + seconds * 25).is_ok());
        assert!(lock.standby_window().unwrap().unwrap().effective_time() >= wall + 25);
        // A restarted process retains the already consumed time on disk.
        let restarted = Mode::standby(&lock).unwrap();
        assert!(
            restarted
                .check_at(&lock, wall, Instant::now() + seconds * 36)
                .is_err()
        );
        assert!(gate.check_at(&lock, wall, now + seconds * 26).is_err());
        assert!(Mode::standby(&lock).is_err());
        assert_eq!(lock.begin_standby(1440).unwrap().deadline, first.deadline);
        lock.end_standby().unwrap();
        let second = lock.begin_standby(1).unwrap();
        assert_ne!(first.id, second.id);
        let next = Mode::standby(&lock).unwrap();
        assert!(
            next.check_at(&lock, second.started_at + 20, Instant::now())
                .is_ok()
        );
        assert!(
            next.check_at(&lock, second.started_at + 19, Instant::now())
                .is_err()
        );
        assert!(
            next.check_at(&lock, second.started_at + 21, Instant::now())
                .is_err()
        );
        assert!(gate.check(&lock).is_err());
    }
}
