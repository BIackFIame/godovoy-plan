//! Separate storage format for the authoritative host. It has no P2P keys or routes.
#[path = "online_audit.rs"]
mod security_audit;

use super::{
    CALENDAR_MESSAGE_LIMIT, check_profile_file, random_bytes, save, validate_profile_access,
};
use crate::{
    Fallible,
    model::{Calendar, Id},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use blake2::{Blake2s256, Digest};
use chrono::Utc;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, VecDeque},
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

#[derive(Serialize, Deserialize)]
pub struct Grant {
    pub employee: Id,
    pub read_only: bool,
    digest: [u8; 32],
}
#[derive(Serialize, Deserialize)]
pub struct Access {
    pub name: String,
    pub employees: BTreeMap<Id, String>,
    pub grants: BTreeMap<Id, Grant>,
    pub audit: VecDeque<Audit>,
}
impl Access {
    pub fn authorize(&self, token: &str) -> Option<(Id, &Grant)> {
        let (id, secret) = token.split_once('.')?;
        let id = Id::parse_str(id).ok()?;
        let grant = self.grants.get(&id)?;
        if secret.len() != 43 {
            return None;
        }
        let digest: [u8; 32] = Blake2s256::digest(secret.as_bytes()).into();
        bool::from(digest.ct_eq(&grant.digest)).then_some((id, grant))
    }
    pub fn active_employee(&self, id: Id) -> bool {
        self.grants.values().any(|grant| grant.employee == id)
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
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(directory.join(LOCK))?;
        file.lock()?;
        let database = Data {
            calendar,
            audit: VecDeque::new(),
        };
        let access = Access {
            name,
            employees,
            grants: BTreeMap::new(),
            audit: VecDeque::new(),
        };
        save(&directory.join(DATA), &database, DATA_LIMIT)?;
        save(&directory.join(ACCESS), &access, ACCESS_LIMIT)?;
        sync_directory(directory)?;
        Ok(Self(directory.canonicalize()?))
    }
    pub fn open(directory: &Path) -> Fallible<Self> {
        validate_profile_access(directory)?;
        if directory.join("calendar.json").try_exists()? {
            return Err("Нельзя использовать P2P-профиль как хранилище узла.".into());
        }
        for name in [ACCESS, DATA, LOCK] {
            if !fs::symlink_metadata(directory.join(name))?.is_file() {
                return Err("Повреждён каталог узла.".into());
            }
        }
        Ok(Self(directory.canonicalize()?))
    }
    /// Every administrative change and authenticated read/write shares this lock.
    /// In particular an HTTP request cannot pass revocation and then commit a write.
    pub fn lock(&self) -> Fallible<LockedVault<'_>> {
        let path = self.0.join(LOCK);
        check_profile_file(&path)?;
        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        let file = opts.open(path)?;
        file.lock()?;
        Ok(LockedVault {
            directory: &self.0,
            _lock: file,
        })
    }
}
fn sync_directory(directory: &Path) -> Fallible<()> {
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}
impl LockedVault<'_> {
    fn read<T: DeserializeOwned>(&self, name: &str, limit: usize) -> Fallible<T> {
        let path = self.directory.join(name);
        check_profile_file(&path)?;
        let file = File::open(path)?;
        if file.metadata()?.len() > limit as u64 {
            return Err("Хранилище превышает допустимый размер.".into());
        }
        Ok(serde_json::from_reader(std::io::BufReader::new(
            file.take(limit as u64 + 1),
        ))?)
    }
    pub fn access(&self) -> Fallible<Access> {
        self.read(ACCESS, ACCESS_LIMIT)
    }
    pub fn data(&self) -> Fallible<Data> {
        self.read(DATA, DATA_LIMIT)
    }
    pub fn save_data(&self, data: &Data) -> Fallible<()> {
        data.calendar.validate()?;
        save(&self.directory.join(DATA), data, DATA_LIMIT)?;
        sync_directory(self.directory)
    }
    pub fn grant(
        &self,
        name: &str,
        existing: Option<Id>,
        read_only: bool,
    ) -> Fallible<(Id, Id, String)> {
        let mut access = self.access()?;
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
                employee,
                read_only,
                digest: Blake2s256::digest(secret.as_bytes()).into(),
            },
        );
        self.audit_security("grant_requested", Some(id), Some(employee), None)?;
        record(&mut access.audit, "grant", id, employee, None);
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
        save(&self.directory.join(ACCESS), &access, ACCESS_LIMIT)?;
        sync_directory(self.directory)
    }
}
