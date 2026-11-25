//! One failover window on the fenced, authoritative volume. Every operation holds its vault lock.
use super::{Id, LockedVault, save, sync_directory};
use crate::Fallible;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fs, io::ErrorKind};

pub(super) const MARKER: &str = "online-standby-v1.json";
const LIMIT: usize = 2048;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandbyWindow {
    pub id: Id,
    pub started_at: i64,
    pub deadline: i64,
    last_wall: i64,
    effective: i64,
    stopped: bool,
}
impl StandbyWindow {
    pub fn active_at(&self, now: i64) -> bool {
        !self.stopped
            && (self.last_wall..self.deadline).contains(&now)
            && self.effective < self.deadline
    }
    pub fn effective_time(&self) -> i64 {
        self.effective
    }
    fn valid(&self) -> bool {
        !self.id.is_nil()
            && self.started_at > 0
            && self
                .deadline
                .checked_sub(self.started_at)
                .is_some_and(|seconds| (60..=86400).contains(&seconds) && seconds % 60 == 0)
            && DateTime::<Utc>::from_timestamp(self.deadline, 0).is_some()
            && (self.started_at..=self.effective).contains(&self.last_wall)
            && self.effective <= self.deadline
    }
}
// The first activation permanently pins this marker's presence via the existing
// lock file. A missing marker after activation must never reopen the primary.
#[derive(Deserialize)]
#[serde(untagged)]
enum Marker {
    Window(StandbyWindow),
    Primary(Primary),
}
#[derive(Deserialize, Serialize)]
enum Primary {
    #[serde(rename = "primary")]
    Primary,
}
impl LockedVault<'_> {
    pub fn standby_window(&self) -> Fallible<Option<StandbyWindow>> {
        let pin = self._lock.metadata()?.len();
        if pin > 1 {
            return Err("Повреждена отметка обязательного состояния резервного режима.".into());
        }
        match fs::symlink_metadata(self.directory.join(MARKER)) {
            Err(e) if e.kind() == ErrorKind::NotFound && pin == 0 => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(metadata) if !metadata.is_file() => {
                return Err("Недопустимый файл резервного окна.".into());
            }
            Ok(_) => {}
        }
        let Marker::Window(window) = self.read::<Marker>(MARKER, LIMIT)? else {
            return Ok(None);
        };
        if !window.valid() {
            return Err("Повреждено резервное окно.".into());
        }
        Ok(Some(window))
    }
    fn save_marker(&self, state: &impl Serialize) -> Fallible<()> {
        save(&self.directory.join(MARKER), state, LIMIT)?;
        #[cfg(windows)]
        fs::OpenOptions::new()
            .write(true)
            .open(self.directory.join(MARKER))?
            .sync_all()?;
        sync_directory(self.directory)
    }
    pub fn begin_standby(&self, minutes: u16) -> Fallible<StandbyWindow> {
        if !(1..=1440).contains(&minutes) {
            return Err("Укажите целое число минут от 1 до 1440.".into());
        }
        if let Some(window) = self.standby_window()? {
            return self.advance_window(window, Utc::now().timestamp(), 0);
        }
        let started_at = Utc::now().timestamp();
        let window = StandbyWindow {
            id: Id::new_v4(),
            started_at,
            deadline: started_at + i64::from(minutes) * 60,
            last_wall: started_at,
            effective: started_at,
            stopped: false,
        };
        if !window.valid() {
            return Err("Проверьте системные часы.".into());
        }
        self.audit_security("standby_begin_requested", None, None, Some(window.id))?;
        if self._lock.metadata()?.len() == 0 {
            self.save_marker(&Primary::Primary)?;
            self._lock.set_len(1)?;
            self._lock.sync_all()?;
        }
        self.save_marker(&window)?;
        Ok(window)
    }
    pub fn end_standby(&self) -> Fallible<()> {
        if let Some(window) = self.standby_window()? {
            self.audit_security("standby_end_requested", None, None, Some(window.id))?;
            self.save_marker(&Primary::Primary)?;
        }
        Ok(())
    }
    /// floor is this process's monotonic estimate of the elapsed window.
    pub fn observe_standby(&self, id: Id, wall: i64, floor: i64) -> Fallible<StandbyWindow> {
        let window = self
            .standby_window()?
            .filter(|window| window.id == id)
            .ok_or("Резервное окно не открыто или сменилось.")?;
        let window = self.advance_window(window, wall, floor)?;
        if window.stopped {
            return Err("Резервное окно истекло или системные часы переведены назад.".into());
        }
        Ok(window)
    }
    pub fn standby_status(&self) -> Fallible<Option<StandbyWindow>> {
        self.standby_window()?
            .map(|window| self.advance_window(window, Utc::now().timestamp(), 0))
            .transpose()
    }
    fn advance_window(
        &self,
        mut window: StandbyWindow,
        wall: i64,
        floor: i64,
    ) -> Fallible<StandbyWindow> {
        let before = window;
        if window.stopped {
            return Ok(window);
        }
        if !window.active_at(wall) || floor >= window.deadline {
            window.stopped = true;
            self.save_marker(&window)?;
            self.audit_security("standby_blocked", None, None, Some(window.id))?;
            return Ok(window);
        }
        window.last_wall = wall;
        window.effective = window.effective.max(wall).max(floor);
        if window != before {
            self.save_marker(&window)?;
        }
        Ok(window)
    }
}
