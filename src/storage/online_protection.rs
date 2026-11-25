//! Host-enforced rules for the authoritative calendar, under the shared volume lock.
use super::{ACCESS, ACCESS_LIMIT, Access, Id, LockedVault, save, sync_directory};
use crate::Fallible;
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, fs, io::ErrorKind};

const REQUIRED: &str = "online-protection-required-v1.json";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtectionSettings {
    pub russia: bool,
    pub belarus: bool,
    pub allow_remote: bool,
    pub allow_free_text: bool,
}
impl ProtectionSettings {
    fn active(self) -> bool {
        self.russia || self.belarus
    }
    pub fn remote_allowed(self) -> bool {
        !self.active() || self.allow_remote
    }
    pub fn free_text_allowed(self) -> bool {
        !self.active() || self.allow_free_text
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtectionState {
    pub revision: u64,
    pub settings: ProtectionSettings,
}

#[derive(Debug)]
pub struct ProtectionConflict;
impl fmt::Display for ProtectionConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Настройки уже изменились. Обновите страницу.")
    }
}
impl std::error::Error for ProtectionConflict {}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct StoredProtection {
    version: u8,
    state: ProtectionState,
    change_id: Id,
    pending_audit: bool,
}

// Support legacy absence, reject explicit null or unknown/malformed settings.
pub(super) fn present<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<StoredProtection>, D::Error> {
    StoredProtection::deserialize(d).map(Some)
}
impl Access {
    pub fn protection(&self) -> ProtectionState {
        self.protection
            .as_ref()
            .map(|p| p.state)
            .unwrap_or_default()
    }
}
impl LockedVault<'_> {
    fn policy_required(&self) -> Fallible<bool> {
        match fs::symlink_metadata(self.directory.join(REQUIRED)) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
            Ok(m) if m.is_file() && self.read::<u8>(REQUIRED, 8)? == 1 => Ok(true),
            Ok(_) => Err("Повреждена отметка обязательной политики.".into()),
        }
    }
    pub(super) fn finish_protection(&self, acl: &mut Access) -> Fallible<()> {
        let pinned = self.policy_required()?;
        let Some(policy) = acl.protection.as_mut() else {
            if pinned {
                return Err("Утрачена обязательная политика.".into());
            }
            return Ok(());
        };
        if policy.version != 1 || policy.state.revision == 0 || policy.change_id.is_nil() {
            return Err("Неподдерживаемая или повреждённая политика.".into());
        }
        if !pinned {
            if !policy.pending_audit {
                return Err("Утрачена отметка обязательной политики.".into());
            }
            save(&self.directory.join(REQUIRED), &1u8, 8)?;
            sync_directory(self.directory)?;
        }
        if policy.pending_audit {
            self.audit_protection("protection_applied", policy.change_id, policy.state)?;
            policy.pending_audit = false;
            save(&self.directory.join(ACCESS), acl, ACCESS_LIMIT)?;
            sync_directory(self.directory)?;
        }
        Ok(())
    }
    pub fn protection(&self) -> Fallible<ProtectionState> {
        Ok(self.access()?.protection())
    }
    pub fn set_protection(
        &self,
        expected_revision: u64,
        settings: ProtectionSettings,
    ) -> Fallible<ProtectionState> {
        let mut acl = self.access()?;
        if self.standby_window()?.is_some() {
            return Err(
                "Настройки защиты можно изменить после завершения резервного режима.".into(),
            );
        }
        let current = acl.protection();
        if current.revision != expected_revision {
            return Err(ProtectionConflict.into());
        }
        if current.settings == settings {
            return Ok(current);
        }
        let state = ProtectionState {
            revision: expected_revision
                .checked_add(1)
                .ok_or("Исчерпан счётчик версий политики.")?,
            settings,
        };
        let change_id = Id::new_v4();
        self.audit_protection("protection_requested", change_id, state)?;
        acl.protection = Some(StoredProtection {
            version: 1,
            state,
            change_id,
            pending_audit: true,
        });
        save(&self.directory.join(ACCESS), &acl, ACCESS_LIMIT)?;
        sync_directory(self.directory)?;
        self.finish_protection(&mut acl)?;
        Ok(state)
    }
}
