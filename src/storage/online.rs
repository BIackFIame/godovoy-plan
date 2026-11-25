//! Separate storage format for the authoritative host. It has no P2P keys or routes.
#[path = "online_evidence.rs"]
mod evidence;
#[path = "online_protection.rs"]
mod protection;
#[path = "online_audit.rs"]
mod security_audit;
#[path = "online_snapshot.rs"]
mod snapshot;
pub use protection::{ProtectionConflict, ProtectionSettings, ProtectionState};
#[path = "online_operator.rs"]
mod operator;
pub use operator::{OperatorDetails, OperatorInput, OperatorRecord};
#[path = "online_standby.rs"]
mod standby;
pub use standby::StandbyWindow;

use super::{CALENDAR_MESSAGE_LIMIT, access, random_bytes, save, validate_profile_access};
use crate::{
    Fallible,
    model::{Calendar, Id},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use blake2::{Blake2b, Blake2s256, Digest, digest::consts::U32};
use chrono::Utc;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};
use subtle::ConstantTimeEq;

const ACCESS: &str = "online-access-v1.json";
const DATA: &str = "online-calendar-v1.json";
const LOCK: &str = "profile.lock";
const ACCESS_LIMIT: usize = 512 * 1024;
const DATA_LIMIT: usize = CALENDAR_MESSAGE_LIMIT + 512 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct Audit {
    pub at: i64,
    pub action: String,
    pub credential: Id,
    pub employee: Id,
    pub event: Option<Id>,
}
pub fn record(
    log: &mut VecDeque<Audit>,
    action: &str,
    credential: Id,
    employee: Id,
    event: Option<Id>,
) {
    if log.len() == 1024 {
        log.pop_front();
    }
    log.push_back(Audit {
        at: Utc::now().timestamp(),
        action: action.into(),
        credential,
        employee,
        event,
    });
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantPurpose {
    #[default]
    Employee,
    Mcp,
}

impl GrantPurpose {
    fn digest(self, secret: &str) -> [u8; 32] {
        let mut hash = Blake2s256::new();
        if self == Self::Mcp {
            // Older binaries ignore unknown fields. They must never accept an MCP
            // secret as an ordinary employee credential, even after a downgrade.
            hash.update(b"godovoy-plan/mcp-key/v1\0");
        }
        hash.update(secret.as_bytes());
        hash.finalize().into()
    }
}

#[derive(Serialize, Deserialize)]
pub struct Grant {
    #[serde(default)]
    pub purpose: GrantPurpose,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Id>,
    pub employee: Id,
    pub read_only: bool,
    #[serde(default)]
    pub allow_remote: bool,
    digest: [u8; 32],
}
#[derive(Default)]
pub struct GrantPermissions {
    pub read_only: bool,
    pub allow_remote: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Access {
    #[serde(
        default,
        deserialize_with = "protection::present",
        skip_serializing_if = "Option::is_none"
    )]
    protection: Option<protection::StoredProtection>,
    #[serde(default)]
    pub operator: OperatorRecord,
    pub name: String,
    pub employees: BTreeMap<Id, String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub assigned_only: BTreeSet<Id>,
    pub grants: BTreeMap<Id, Grant>,
    pub audit: VecDeque<Audit>,
}
impl Access {
    pub fn authorize(&self, token: &str) -> Option<(Id, &Grant)> {
        self.authorize_for(token, GrantPurpose::Employee)
    }

    pub fn authorize_for(&self, token: &str, purpose: GrantPurpose) -> Option<(Id, &Grant)> {
        let (id, secret) = token.split_once('.')?;
        let id = Id::parse_str(id).ok()?;
        let grant = self.grants.get(&id)?;
        if secret.len() != 43 || grant.purpose != purpose {
            return None;
        }
        let digest = purpose.digest(secret);
        if !bool::from(digest.ct_eq(&grant.digest))
            || (purpose == GrantPurpose::Mcp && self.mcp_permissions(grant).is_none())
        {
            return None;
        }
        Some((id, grant))
    }

    pub fn active_employee(&self, id: Id) -> bool {
        self.grants
            .values()
            .any(|g| g.purpose == GrantPurpose::Employee && g.employee == id)
    }

    /// The issuing employee key is the sole authority for a delegated credential.
    pub fn mcp_permissions(&self, grant: &Grant) -> Option<GrantPermissions> {
        let parent = self.grants.get(&grant.parent?)?;
        if grant.purpose != GrantPurpose::Mcp
            || parent.purpose != GrantPurpose::Employee
            || parent.employee != grant.employee
        {
            return None;
        }
        Some(GrantPermissions {
            read_only: grant.read_only || parent.read_only,
            allow_remote: grant.allow_remote && parent.allow_remote,
        })
    }
}
#[derive(Serialize, Deserialize)]
pub struct Data {
    pub calendar: Calendar,
    pub audit: VecDeque<Audit>,
}

#[derive(Clone)]
pub struct OnlineVault(PathBuf);
pub struct LockedVault<'a> {
    directory: &'a Path,
    _lock: File,
}

impl OnlineVault {
    pub fn init(
        directory: &Path,
        name: String,
        calendar: Calendar,
        employees: BTreeMap<Id, String>,
    ) -> Fallible<Self> {
        if name.trim().is_empty() || name.chars().count() > 80 {
            return Err("Название календаря должно содержать от 1 до 80 символов.".into());
        }
        calendar.validate()?;
        let absolute = std::path::absolute(directory)?;
        let parent = absolute
            .parent()
            .ok_or("Укажите новую папку для узла.")?
            .canonicalize()?;
        for ancestor in parent.ancestors() {
            if ancestor.join("calendar.json").try_exists()? || ancestor.join(ACCESS).try_exists()? {
                return Err("Каталог узла должен быть вне существующего профиля.".into());
            }
        }
        if directory.try_exists()? {
            return Err("Для нового узла нужна новая, ещё не существующая папка.".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(directory)?;
        }
        #[cfg(windows)]
        super::access::create_windows_profile(directory)?;
        validate_profile_access(directory)?;
        let file = access::open_lock(&directory.join(LOCK), OpenOptions::new().create_new(true))?;
        file.lock()?;
        let database = Data {
            calendar,
            audit: VecDeque::new(),
        };
        let access = Access {
            protection: None,
            operator: Default::default(),
            name,
            employees,
            assigned_only: BTreeSet::new(),
            grants: BTreeMap::new(),
            audit: VecDeque::new(),
        };
        save(&directory.join(DATA), &database, DATA_LIMIT)?;
        save(&directory.join(ACCESS), &access, ACCESS_LIMIT)?;
        sync_directory(directory)?;
        Ok(Self(directory.canonicalize()?))
    }
    pub fn open(directory: &Path) -> Fallible<Self> {
        super::check_private_directory(directory)?;
        if directory.join("calendar.json").try_exists()? {
            return Err("Нельзя использовать P2P-профиль как хранилище узла.".into());
        }
        for name in [ACCESS, DATA, LOCK] {
            if !fs::symlink_metadata(directory.join(name))?.is_file() {
                return Err("Повреждён каталог узла.".into());
            }
        }
        validate_profile_access(directory)?;
        Ok(Self(directory.canonicalize()?))
    }
    /// Every administrative change and authenticated read/write shares this lock.
    /// In particular an HTTP request cannot pass revocation and then commit a write.
    pub fn lock(&self) -> Fallible<LockedVault<'_>> {
        let path = self.0.join(LOCK);
        let file = access::open_lock(&path, &mut OpenOptions::new())?;
        file.lock()?;
        Ok(LockedVault {
            directory: &self.0,
            _lock: file,
        })
    }
}
fn open_regular(path: &Path) -> Fallible<(File, fs::Metadata)> {
    access::open_regular(path, OpenOptions::new().read(true))
}

fn read_bounded_json<T: DeserializeOwned>(file: File, limit: usize) -> Fallible<T> {
    let mut bounded = file.take(limit as u64 + 1);
    let value = serde_json::from_reader(std::io::BufReader::new(&mut bounded))?;
    if bounded.limit() == 0 {
        return Err("Хранилище превышает допустимый размер.".into());
    }
    Ok(value)
}
fn sync_directory(directory: &Path) -> Fallible<()> {
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}
impl LockedVault<'_> {
    fn open_bounded(&self, name: &str, limit: usize) -> Fallible<(File, usize)> {
        let path = self.directory.join(name);
        let (file, metadata) = open_regular(&path)?;
        let size = metadata.len();
        if size > limit as u64 {
            return Err("Хранилище превышает допустимый размер.".into());
        }
        Ok((file, size as usize))
    }
    fn read<T: DeserializeOwned>(&self, name: &str, limit: usize) -> Fallible<T> {
        let (file, _) = self.open_bounded(name, limit)?;
        read_bounded_json(file, limit)
    }
    pub fn access(&self) -> Fallible<Access> {
        // The ACL is small and read on each request under the vault lock.
        // Parse the current file directly from a bounded contiguous buffer.
        let mut acl: Access = {
            let (mut file, size) = self.open_bounded(ACCESS, ACCESS_LIMIT)?;
            let mut bytes = vec![0; size];
            file.read_exact(&mut bytes)?;
            if file.read(&mut [0])? != 0 {
                return Err("Размер файла прав изменился во время чтения.".into());
            }
            serde_json::from_slice(&bytes)?
        };
        if acl
            .assigned_only
            .iter()
            .any(|id| !acl.employees.contains_key(id))
        {
            return Err("Повреждена область доступа сотрудника.".into());
        }
        if acl.grants.iter().any(|(id, grant)| {
            id.is_nil()
                || !acl.employees.contains_key(&grant.employee)
                || match grant.purpose {
                    GrantPurpose::Employee => grant.parent.is_some(),
                    GrantPurpose::Mcp => acl.mcp_permissions(grant).is_none(),
                }
        }) {
            return Err("Повреждена привязка ключа к сотруднику.".into());
        }
        self.finish_protection(&mut acl)?;
        Ok(acl)
    }
    pub fn set_employee_scope(&self, employee: Id, assigned_only: bool) -> Fallible<()> {
        if self.standby_window()?.is_some() {
            return Err(
                "Область доступа можно изменить после завершения резервного режима.".into(),
            );
        }
        let mut acl = self.access()?;
        if !acl.employees.contains_key(&employee) {
            return Err(OperatorInput.into());
        }
        let (changed, action) = if assigned_only {
            (
                acl.assigned_only.insert(employee),
                "employee_scope_assigned_requested",
            )
        } else {
            (
                acl.assigned_only.remove(&employee),
                "employee_scope_shared_requested",
            )
        };
        if changed {
            self.audit_security(action, None, Some(employee), None)?;
            save(&self.directory.join(ACCESS), &acl, ACCESS_LIMIT)?;
            sync_directory(self.directory)?;
        }
        Ok(())
    }
    pub fn data(&self) -> Fallible<Data> {
        Ok(serde_json::from_slice(&self.data_bytes()?)?)
    }
    /// Read the exact bytes used for both the conditional view fingerprint and decoding.
    pub fn data_bytes(&self) -> Fallible<Vec<u8>> {
        let (file, size) = self.open_bounded(DATA, DATA_LIMIT)?;
        let mut bytes = Vec::with_capacity(size);
        file.take(DATA_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > DATA_LIMIT {
            return Err("Хранилище превышает допустимый размер.".into());
        }
        Ok(bytes)
    }
    pub fn save_data(&self, data: &Data) -> Fallible<()> {
        self.persist_data(data, |_| {})
    }

    /// Extend the caller's fingerprint with the exact bytes committed to disk.
    pub fn save_data_hashed(&self, data: &Data, mut hash: Blake2b<U32>) -> Fallible<[u8; 32]> {
        self.persist_data(data, |bytes| hash.update(bytes))?;
        Ok(hash.finalize().into())
    }

    fn persist_data(&self, data: &Data, observe: impl FnMut(&[u8])) -> Fallible<()> {
        data.calendar.validate()?;
        let path = self.directory.join(DATA);
        let (staging, _) = super::stage_observed(&path, data, DATA_LIMIT, observe)?;
        staging.persist(&path)?;
        sync_directory(self.directory)
    }
    pub fn grant(
        &self,
        name: &str,
        existing: Option<Id>,
        read_only: bool,
    ) -> Fallible<(Id, Id, String)> {
        self.grant_with_permissions(
            name,
            existing,
            GrantPermissions {
                read_only,
                ..Default::default()
            },
        )
    }
    pub fn grant_with_permissions(
        &self,
        name: &str,
        existing: Option<Id>,
        permissions: GrantPermissions,
    ) -> Fallible<(Id, Id, String)> {
        self.issue(name, existing, permissions, GrantPurpose::Employee)
    }

    pub fn grant_mcp(
        &self,
        name: &str,
        employee: Id,
        permissions: GrantPermissions,
    ) -> Fallible<(Id, Id, String)> {
        self.issue(name, Some(employee), permissions, GrantPurpose::Mcp)
    }

    fn issue(
        &self,
        name: &str,
        existing: Option<Id>,
        permissions: GrantPermissions,
        purpose: GrantPurpose,
    ) -> Fallible<(Id, Id, String)> {
        let mut access = self.access()?;
        if self.standby_window()?.is_some() {
            return Err("Выдача ключей приостановлена до завершения резервного режима.".into());
        }
        let parent = if purpose == GrantPurpose::Mcp {
            let employee = existing.ok_or("Укажите сотрудника для MCP.")?;
            let (parent_id, _) = access.grants.iter().find(|(_, g)| {
                g.employee == employee && g.purpose == GrantPurpose::Employee
                    && (permissions.read_only || !g.read_only)
                    && (!permissions.allow_remote || g.allow_remote)
            }).ok_or("Для MCP нужен действующий обычный ключ сотрудника с запрошенными правами на этом входе.")?;
            Some(*parent_id)
        } else {
            None
        };
        if permissions.allow_remote && !access.protection().settings.remote_allowed() {
            self.audit_security("protection_remote_grant_denied", None, existing, None)?;
            return Err("Внешний вход отключён политикой календаря.".into());
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
            return Err("Введите имя сотрудника (1–80 символов).".into());
        }
        if access.grants.len() >= 512 {
            return Err("Достигнут лимит действующих ключей.".into());
        }
        let employee = match existing {
            Some(id)
                if access
                    .employees
                    .get(&id)
                    .is_some_and(|stored| stored == name) =>
            {
                id
            }
            Some(_) => return Err("Имя и ID должны совпадать с сотрудником из status.".into()),
            None if access.employees.len() >= 256 => {
                return Err("Достигнут лимит сотрудников.".into());
            }
            None => {
                let id = Id::new_v4();
                access.employees.insert(id, name.to_owned());
                id
            }
        };
        let id = Id::new_v4();
        let secret = URL_SAFE_NO_PAD.encode(random_bytes::<32>()?);
        access.grants.insert(
            id,
            Grant {
                purpose,
                parent,
                employee,
                read_only: permissions.read_only,
                allow_remote: permissions.allow_remote,
                digest: purpose.digest(&secret),
            },
        );
        let (request, action) = match (purpose, permissions.allow_remote) {
            (GrantPurpose::Mcp, true) => ("grant_mcp_remote_requested", "grant_mcp_remote"),
            (GrantPurpose::Mcp, false) => ("grant_mcp_requested", "grant_mcp"),
            (GrantPurpose::Employee, true) => ("grant_remote_requested", "grant_remote"),
            (GrantPurpose::Employee, false) => ("grant_requested", "grant"),
        };
        self.audit_security(request, Some(id), Some(employee), None)?;
        record(&mut access.audit, action, id, employee, None);
        save(&self.directory.join(ACCESS), &access, ACCESS_LIMIT)?;
        sync_directory(self.directory)?;
        Ok((id, employee, format!("{id}.{secret}")))
    }
    /// Before opening a restored backup to clients, invalidate every restored
    /// credential in one ledger commit without changing employee assignments.
    pub fn revoke_all(&self) -> Fallible<usize> {
        let mut access = self.access()?;
        self.audit_security("revoke_all_requested", None, None, None)?;
        let count = access.grants.len();
        if count != 0 {
            for (id, grant) in std::mem::take(&mut access.grants) {
                record(&mut access.audit, "revoke", id, grant.employee, None);
            }
            save(&self.directory.join(ACCESS), &access, ACCESS_LIMIT)?;
            sync_directory(self.directory)?;
        }
        Ok(count)
    }

    pub fn revoke(&self, id: Id) -> Fallible<()> {
        let mut access = self.access()?;
        let grant = access
            .grants
            .remove(&id)
            .ok_or("Такого действующего ключа нет.")?;
        self.audit_security("revoke_requested", Some(id), Some(grant.employee), None)?;
        record(&mut access.audit, "revoke", id, grant.employee, None);
        access.grants.retain(|child_id, child| {
            if child.parent == Some(id) {
                record(
                    &mut access.audit,
                    "revoke_mcp",
                    *child_id,
                    child.employee,
                    None,
                );
                false
            } else {
                true
            }
        });
        save(&self.directory.join(ACCESS), &access, ACCESS_LIMIT)?;
        sync_directory(self.directory)
    }
}

#[cfg(test)]
mod file_guards {
    use super::*;
    use std::io::{Seek, Write};

    #[test]
    fn bounded_json_checks_actual_bytes_after_open() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"42  ").unwrap();
        file.rewind().unwrap();
        let metadata_len = file.metadata().unwrap().len() as usize;
        assert_eq!(
            read_bounded_json::<u8>(file.try_clone().unwrap(), metadata_len).unwrap(),
            42
        );
        file.seek(std::io::SeekFrom::End(0)).unwrap();
        file.write_all(b" ").unwrap();
        file.rewind().unwrap();
        assert!(read_bounded_json::<u8>(file, metadata_len).is_err());
    }
}
