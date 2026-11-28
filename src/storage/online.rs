//! Separate storage format for the authoritative host. It has no P2P keys or routes.
#[path = "online_evidence.rs"]
mod evidence;
#[path = "online_protection.rs"]
mod protection;
#[path = "online_audit.rs"]
mod security_audit;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "online_revocation_reserve.rs"]
mod revocation_reserve;
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
use blake2::{Blake2s256, Digest, digest::consts::U32};
use chrono::Utc;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Seek},
    path::{Path, PathBuf},
};
use subtle::ConstantTimeEq;

const ACCESS: &str = "online-access-v1.json";
const DATA: &str = "online-calendar-v1.json";
const LOCK: &str = "profile.lock";
const AUDIT_LIMIT: usize = 1024;
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
    if log.len() >= AUDIT_LIMIT {
        log.drain(..log.len() - (AUDIT_LIMIT - 1));
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
/// One validated session document per host, used only while holding its vault lock.
#[derive(Default)]
pub struct SessionAcl(Option<(Option<Vec<u8>>, Access)>);
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
        self.authorize_with_permissions(token, purpose)
            .map(|(id, grant, _)| (id, grant))
    }

    /// Checks the credential and resolves its permissions against this access snapshot.
    pub fn authorize_with_permissions(
        &self,
        token: &str,
        purpose: GrantPurpose,
    ) -> Option<(Id, &Grant, GrantPermissions)> {
        let (id, secret) = token.split_once('.')?;
        let id = Id::parse_str(id).ok()?;
        let grant = self.grants.get(&id)?;
        if secret.len() != 43 || grant.purpose != purpose {
            return None;
        }
        let digest = purpose.digest(secret);
        if !bool::from(digest.ct_eq(&grant.digest)) {
            return None;
        }
        let rights = match purpose {
            GrantPurpose::Employee => GrantPermissions {
                read_only: grant.read_only,
                allow_remote: grant.allow_remote,
            },
            GrantPurpose::Mcp => self.mcp_permissions(grant)?,
        };
        Some((id, grant, rights))
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
    #[serde(deserialize_with = "deserialize_calendar")]
    pub calendar: Calendar,
    pub audit: VecDeque<Audit>,
}

fn deserialize_calendar<'de, D: serde::Deserializer<'de>>(source: D) -> Result<Calendar, D::Error> {
    let calendar = Calendar::deserialize(source)?;
    calendar.validate().map_err(serde::de::Error::custom)?;
    Ok(calendar)
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

pub(super) fn read_bounded_json<T: DeserializeOwned>(file: File, limit: usize) -> Fallible<T> {
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
/// An opened vault data file. Metadata only guides the choice of reading strategy.
pub struct CalendarFile<'vault> {
    _vault: std::marker::PhantomData<&'vault ()>,
    file: File,
    metadata: fs::Metadata,
}

#[cfg(not(all(target_env = "musl", any(target_arch = "x86", target_arch = "x86_64"))))]
fn equal_source(expected: &[u8], actual: &[u8]) -> bool {
    expected == actual
}

// The portable musl memcmp used by the x86 Linux packages compares one byte
// at a time. Whole words let LLVM use the target's ordinary vector registers.
#[cfg(any(
    test,
    all(target_env = "musl", any(target_arch = "x86", target_arch = "x86_64"))
))]
fn equal_source_words(expected: &[u8], actual: &[u8]) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    let (expected, expected_tail) = expected.as_chunks::<8>();
    let (actual, actual_tail) = actual.as_chunks::<8>();
    let mut different = 0u64;
    for (left, right) in expected.iter().zip(actual) {
        different |= u64::from_ne_bytes(*left) ^ u64::from_ne_bytes(*right);
    }
    different == 0 && expected_tail == actual_tail
}

#[cfg(all(target_env = "musl", any(target_arch = "x86", target_arch = "x86_64")))]
use equal_source_words as equal_source;

fn file_matches_bytes(file: &mut File, expected: &[u8]) -> Fallible<bool> {
    let mut offset = 0;
    let mut chunk = [0; 16 * 1024];
    loop {
        let count = match file.read(&mut chunk) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 && offset == expected.len() {
            return Ok(true);
        }
        if count == 0
            || !expected
                .get(offset..offset + count)
                .is_some_and(|wanted| equal_source(wanted, &chunk[..count]))
        {
            file.rewind()?;
            return Ok(false);
        }
        offset += count;
    }
}

/// Compare all bytes of two calendar fragments, using the same implementation
/// as exact file verification on each server target.
pub fn same_calendar_bytes(expected: &[u8], actual: &[u8]) -> bool {
    equal_source(expected, actual)
}

impl CalendarFile<'_> {
    pub fn byte_len(&self) -> usize {
        self.metadata.len() as usize
    }

    pub fn modified(&self) -> Option<std::time::SystemTime> {
        self.metadata.modified().ok()
    }

    pub fn into_bytes(self) -> Fallible<Vec<u8>> {
        let mut bytes = Vec::with_capacity(self.byte_len());
        self.file
            .take(DATA_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > DATA_LIMIT {
            return Err("Хранилище превышает допустимый размер.".into());
        }
        Ok(bytes)
    }

    /// Compare the opened file completely, including EOF. On a mismatch leave
    /// it rewound so its actual contents can take the normal validation path.
    pub fn matches_bytes(&mut self, expected: &[u8]) -> Fallible<bool> {
        file_matches_bytes(&mut self.file, expected)
    }

    pub fn matches_snapshot(&mut self, expected: &[u8]) -> Fallible<bool> {
        if self.byte_len() != expected.len() {
            return Ok(false);
        }
        self.matches_bytes(expected)
    }

    /// Compare the current file, including EOF, to an independently encoded snapshot.
    /// A mismatch leaves the file rewound for normal decoding under the vault lock.
    pub fn matches_stream(&mut self, mut expected: impl Read) -> Fallible<bool> {
        let mut actual = [0; 16 * 1024];
        let mut reference = [0; 16 * 1024];
        loop {
            let count = match self.file.read(&mut actual) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                count => count?,
            };
            if count == 0 {
                match expected.read(&mut reference[..1]) {
                    Ok(0) => return Ok(true),
                    Ok(_) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            match expected.read_exact(&mut reference[..count]) {
                Ok(()) if equal_source(&actual[..count], &reference[..count]) => {}
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error.into()),
            }
        }
        self.file.rewind()?;
        Ok(false)
    }

    pub fn fingerprint(
        &mut self,
        mut source: impl Digest<OutputSize = U32> + std::io::Write,
    ) -> Fallible<[u8; 32]> {
        let mut bounded = (&mut self.file).take(DATA_LIMIT as u64 + 1);
        std::io::copy(&mut bounded, &mut source)?;
        if bounded.limit() == 0 {
            return Err("Хранилище превышает допустимый размер.".into());
        }
        self.file.rewind()?;
        Ok(source.finalize().into())
    }
}

impl LockedVault<'_> {
    fn open_bounded(&self, name: &str, limit: usize) -> Fallible<(File, fs::Metadata)> {
        let path = self.directory.join(name);
        let (file, metadata) = open_regular(&path)?;
        let size = metadata.len();
        if size > limit as u64 {
            return Err("Хранилище превышает допустимый размер.".into());
        }
        Ok((file, metadata))
    }
    fn read<T: DeserializeOwned>(&self, name: &str, limit: usize) -> Fallible<T> {
        let (file, _) = self.open_bounded(name, limit)?;
        read_bounded_json(file, limit)
    }
    fn access_bytes(mut file: File, length: usize) -> Fallible<Vec<u8>> {
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)?;
        if file.read(&mut [0])? != 0 {
            return Err("Размер файла прав изменился во время чтения.".into());
        }
        Ok(bytes)
    }
    fn decode_access(bytes: &[u8]) -> Fallible<Access> {
        let acl: Access = serde_json::from_slice(bytes)?;
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
        Ok(acl)
    }
    pub fn access(&self) -> Fallible<Access> {
        let (file, metadata) = self.open_bounded(ACCESS, ACCESS_LIMIT)?;
        let bytes = Self::access_bytes(file, metadata.len() as usize)?;
        let mut acl = Self::decode_access(&bytes)?;
        self.finish_protection(&mut acl)?;
        Ok(acl)
    }
    pub fn session_access<'a>(&self, cache: &'a mut SessionAcl) -> Fallible<&'a Access> {
        let (mut file, metadata) = self.open_bounded(ACCESS, ACCESS_LIMIT)?;
        let known = cache.0.as_ref().and_then(|(bytes, _)| bytes.as_deref());
        let hit = match known {
            Some(bytes) if bytes.len() as u64 == metadata.len() => {
                file_matches_bytes(&mut file, bytes)?
            }
            _ => false,
        };
        if !hit {
            cache.0 = None;
            let bytes = Self::access_bytes(file, metadata.len() as usize)?;
            let acl = Self::decode_access(&bytes)?;
            // A journal recovery rewrites ACCESS. Its original bytes must always replay recovery.
            let cacheable = !acl.has_pending_protection();
            cache.0 = Some((cacheable.then_some(bytes), acl));
        }
        let (_, acl) = cache.0.as_mut().ok_or("Не удалось прочитать права.")?;
        self.finish_protection(acl)?;
        Ok(acl)
    }
    fn commit_registry(&self, acl: &Access) -> Fallible<()> {
        save(&self.directory.join(ACCESS), acl, ACCESS_LIMIT)?;
        sync_directory(self.directory)
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
            self.commit_registry(&acl)?;
        }
        Ok(())
    }
    pub fn data(&self) -> Fallible<Data> {
        Ok(serde_json::from_slice(&self.data_bytes()?)?)
    }
    pub fn open_data(&self) -> Fallible<CalendarFile<'_>> {
        let (file, metadata) = self.open_bounded(DATA, DATA_LIMIT)?;
        Ok(CalendarFile {
            file,
            metadata,
            _vault: std::marker::PhantomData,
        })
    }
    pub fn data_bytes(&self) -> Fallible<Vec<u8>> {
        self.open_data()?.into_bytes()
    }
    pub fn save_data(&self, data: &Data) -> Fallible<()> {
        self.save_data_observed(data, |_| {})
    }

    /// Extend the caller's fingerprint with the exact bytes committed to disk.
    pub fn save_data_hashed(
        &self,
        data: &Data,
        mut hash: impl Digest<OutputSize = U32>,
    ) -> Fallible<[u8; 32]> {
        self.save_data_observed(data, |bytes| hash.update(bytes))?;
        Ok(hash.finalize().into())
    }

    /// Observe bytes during staging. They identify a committed version only after success.
    pub fn save_data_observed(&self, data: &Data, observe: impl FnMut(&[u8])) -> Fallible<()> {
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
        self.commit_registry(&access)?;
        Ok((id, employee, format!("{id}.{secret}")))
    }
    /// Before opening a restored backup to clients, invalidate every restored
    /// credential in one ledger commit without changing employee assignments.
    pub fn revoke_all(&self) -> Fallible<usize> {
        self.revoke_all_with_reserve(false)
    }

    pub fn revoke_all_with_reserve(&self, force: bool) -> Fallible<usize> {
        self.revoke_grants(None, force)
    }

    pub fn revoke_employee_keys(&self, employee: Id) -> Fallible<usize> {
        self.revoke_grants(Some(employee), false)
    }

    fn revoke_grants(&self, employee: Option<Id>, force: bool) -> Fallible<usize> {
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if force { return Err("Резерв отзыва недоступен на этой платформе.".into()); }
        let mut access = self.access()?;
        if employee.is_some_and(|id| !access.employees.contains_key(&id)) {
            return Err(OperatorInput.into());
        }
        let matches = |grant: &Grant| employee.is_none_or(|id| grant.employee == id);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if access.grants.values().any(matches) {
            self.release_revoke_space(force);
        }
        let action = if employee.is_some() { "revoke_employee_requested" } else { "revoke_all_requested" };
        self.audit_security(action, None, employee, None)?;
        let before = access.grants.len();
        for (id, grant) in access.grants.extract_if(.., |_, grant| matches(grant)) {
            record(&mut access.audit, "revoke", id, grant.employee, None);
        }
        let count = before - access.grants.len();
        if count != 0 {
            self.commit_registry(&access)?;
        }
        Ok(count)
    }

    pub fn revoke(&self, id: Id) -> Fallible<()> {
        self.revoke_with_reserve(id, false)
    }

    pub fn revoke_with_reserve(&self, id: Id, force: bool) -> Fallible<()> {
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if force { return Err("Резерв отзыва недоступен на этой платформе.".into()); }
        let mut access = self.access()?;
        let grant = access
            .grants
            .remove(&id)
            .ok_or("Такого действующего ключа нет.")?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.release_revoke_space(force);
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
        self.commit_registry(&access)
    }
}

#[cfg(test)]
mod file_guards {
    use super::*;
    use std::io::{Seek, Write};

    #[test]
    fn appending_to_an_oversized_summary_retains_the_latest_entries_in_order() {
        for previous in [0, AUDIT_LIMIT - 1, AUDIT_LIMIT, AUDIT_LIMIT + 9] {
            let id = Id::nil();
            let mut log = (0..previous)
                .map(|i| Audit {
                    at: i as i64,
                    action: String::new(),
                    credential: id,
                    employee: id,
                    event: None,
                })
                .collect();
            record(&mut log, "revoke", id, id, None);
            assert_eq!(log.back().unwrap().action, "revoke");
            log.pop_back();
            let retained = previous.min(AUDIT_LIMIT - 1);
            assert!(
                log.iter()
                    .map(|entry| entry.at as usize)
                    .eq(previous - retained..previous)
            );
        }
    }

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

#[cfg(test)]
mod exact_source_tests {
    use super::equal_source_words;

    #[test]
    fn compare_open_file_observes_growth_truncation_and_rewinds_mismatches() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("calendar");
        let expected = vec![b'q'; 16 * 1024 + 7];
        for replacement in [
            expected[..expected.len() - 1].to_vec(),
            [expected.as_slice(), b"suffix"].concat(),
            {
                let mut changed = expected.clone();
                *changed.last_mut().unwrap() ^= 1;
                changed
            },
        ] {
            std::fs::write(&path, &expected).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let mut calendar = super::CalendarFile {
                metadata: file.metadata().unwrap(),
                file,
                _vault: std::marker::PhantomData,
            };
            std::fs::write(&path, &replacement).unwrap();
            assert!(!calendar.matches_bytes(&expected).unwrap());
            assert!(!calendar.matches_stream(expected.as_slice()).unwrap());
            assert_eq!(calendar.into_bytes().unwrap(), replacement);
        }
    }

    #[test]
    fn file_comparison_includes_unaligned_prefix_tail_and_length() {
        for len in [0, 1, 7, 8, 9, 16, 127, 16 * 1024 - 1, 16 * 1024] {
            let stored = vec![17; len + 1];
            let expected = &stored[1..];
            let mut data = expected.to_vec();
            assert!(equal_source_words(expected, &data));
            assert!(!equal_source_words(expected, &stored));
            for position in [0, len / 2, len.saturating_sub(1)] {
                if position < len {
                    data[position] ^= 0x80;
                    assert!(!equal_source_words(expected, &data));
                    data[position] ^= 0x80;
                }
            }
        }
    }
}
