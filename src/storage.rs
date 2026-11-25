use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Read, Write},
    net::SocketAddrV4,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tempfile::{Builder, NamedTempFile};

use crate::{
    Fallible,
    model::{Calendar, Id, MAX_PLANS, Record, Revision},
    planning::{self, Reminder},
};

mod access;
mod generations;
use access::{check_directory as check_private_directory, validate_profile_access};
mod backup;
pub use backup::{BackupEntry, RestoreResult, RestoredProfile};
#[cfg(feature = "desktop")]
mod import_status;
#[cfg(feature = "desktop")]
pub use import_status::{
    BridgeRunStatus, ImportFailure, ImportResultKind, ImportStatus, ImportSuccess,
};
#[cfg(feature = "desktop")]
pub(crate) use import_status::{ImportLog, valid_source};
mod peer_routes;
pub(crate) use peer_routes::PeerRoutes;

pub type SharedStore = Arc<Mutex<Store>>;
pub const CALENDAR_MESSAGE_LIMIT: usize = 8 * 1024 * 1024;
// Headroom for local credentials, invitations and pause state during restore.
pub const PROFILE_LIMIT: usize = CALENDAR_MESSAGE_LIMIT + 64 * 1024;
const REMINDER_LIMIT: usize = 32 * 1024 * 1024;
const UI_DRAFT_LIMIT: usize = 2 * 1024 * 1024;
const TEAM_MARKER: &str = "team-initialized";
const STAGING_PREFIX: &str = ".godovoy-stage-";
const STAGING_SUFFIX_LEN: usize = 16;

#[derive(Debug, Eq, PartialEq)]
pub enum ProfileOpenIssue {
    Locked,
    InvalidCalendar,
    CalendarTooLarge,
    CalendarMissing,
}

impl std::fmt::Display for ProfileOpenIssue {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Locked => {
                "Календарь уже открыт в другом экземпляре. Закройте его и повторите попытку."
            }
            Self::InvalidCalendar => {
                "Файл календаря не удалось прочитать. Сохраните его отдельно. Если у вас есть резервная копия, восстановите её в папке профиля и повторите попытку."
            }
            Self::CalendarTooLarge => {
                "Файл превышает допустимый размер календаря или профиля. Сохраните его отдельно, восстановите подходящую резервную копию и повторите попытку."
            }
            Self::CalendarMissing => {
                "В папке профиля нет файла calendar.json. Восстановите его из резервной копии и повторите попытку."
            }
        };
        output.write_str(message)
    }
}

impl std::error::Error for ProfileOpenIssue {}

#[derive(Debug, PartialEq, Eq)]
pub enum ProfileSizeLimit {
    File,
    Calendar,
}

impl std::fmt::Display for ProfileSizeLimit {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(match self {
            Self::File => "Файл профиля достиг предельного размера.",
            Self::Calendar => "Календарь достиг предельного размера для обмена.",
        })
    }
}

impl std::error::Error for ProfileSizeLimit {}

#[derive(Debug)]
struct StagingSizeLimit(usize);

impl std::fmt::Display for StagingSizeLimit {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "Запись превысит лимит файла в {} байт.", self.0)
    }
}

impl std::error::Error for StagingSizeLimit {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiSettings {
    pub token: [u8; 32],
    #[serde(default)]
    pub import_token: Option<[u8; 32]>,
    pub port: u16,
    pub enabled: bool,
}

fn is_first_key(generation: &u64) -> bool {
    *generation == 0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Id,
    #[serde(default, skip_serializing_if = "is_first_key")]
    pub key_generation: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    previous_key_proofs: Vec<[u8; 32]>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    revoked_devices: BTreeSet<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_manager: Option<Id>,
    pub name: String,
    pub key: [u8; 32],
    pub calendar: Calendar,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalState {
    pub device: Id,
    pub join_proof: [u8; 32],
    #[serde(default)]
    pub enrollment_grants: BTreeMap<Id, Id>,
    // The manager records an accepted request before replying. This is a local
    // enrollment receipt, not proof that the recipient saved the response.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub admitted_this_generation: BTreeSet<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision_floor: Option<u64>,
    pub workspace: Option<Workspace>,
    pub peers: BTreeSet<SocketAddrV4>,
    pub last_sync: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    sync_paused: bool,
    // Kept for readers of profiles written by releases through 0.4. A system
    // delivery attempt did not count as an acknowledgement.
    #[serde(default, rename = "delivered_reminders", skip_deserializing)]
    legacy_delivery_attempts: [String; 0],
    // Old HTTP bearer keys must never authorize the authenticated-only API.
    #[serde(default, rename = "secure_api_v1")]
    pub api: Option<ApiSettings>,
}

impl Workspace {
    /// Historical owners stay readable; only a new assignment requires current access.
    pub fn check_assignment(&self, previous: Option<Id>, owner: Option<Id>) -> Fallible<()> {
        if let Some(id) = owner
            && owner != previous
        {
            if !self.calendar.members.contains_key(&id) {
                return Err("Указанный ответственный отсутствует в списке участников.".into());
            }
            if self.is_revoked(id) {
                return Err(
                    "Доступ ответственного отозван. Выберите действующего участника.".into(),
                );
            }
        }
        Ok(())
    }
}

impl LocalState {
    pub fn create_workspace(&mut self, name: String, employee: String) -> Fallible<()> {
        if self.workspace.is_some() {
            return Err("На этом устройстве уже есть пространство.".into());
        }
        let mut calendar = Calendar::default();
        calendar
            .members
            .insert(self.device, Record::new(self.revision()?, employee));
        self.workspace = Some(Workspace {
            id: Id::new_v4(),
            key_generation: 0,
            previous_key_proofs: Vec::new(),
            revoked_devices: BTreeSet::new(),
            access_manager: None,
            name,
            key: random_bytes()?,
            calendar,
        });
        Ok(())
    }

    pub fn revision(&self) -> Fallible<Revision> {
        Ok(Revision {
            sequence: self
                .latest_revision()
                .checked_add(1)
                .ok_or("Исчерпан счётчик изменений")?,
            operation: uuid::Builder::from_random_bytes(random_bytes()?).into_uuid(),
        })
    }

    pub(crate) fn latest_revision(&self) -> u64 {
        self.workspace
            .as_ref()
            .map_or(0, |w| w.calendar.latest())
            .max(self.revision_floor.unwrap_or(0))
    }

    pub fn workspace_mut(&mut self) -> Fallible<&mut Workspace> {
        self.workspace
            .as_mut()
            .ok_or_else(|| "Сначала подключитесь к пространству.".into())
    }
}

pub fn random_bytes<const N: usize>() -> Fallible<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReminderFileIssue {
    InvalidData,
    TooLarge,
    Unavailable,
}

impl ReminderFileIssue {
    pub fn can_recover(self) -> bool {
        matches!(self, Self::InvalidData | Self::TooLarge)
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidData => "Файл отметок о прочтении повреждён.",
            Self::TooLarge => "Файл отметок о прочтении превышает допустимый размер.",
            Self::Unavailable => {
                "Файл отметок о прочтении недоступен. Проверьте файл и права профиля."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileUsage {
    pub plans: usize,
    pub plan_limit: usize,
    pub bytes: usize,
    pub byte_limit: usize,
}

impl ProfileUsage {
    fn from_state(state: &LocalState, bytes: usize) -> Self {
        Self {
            plans: state
                .workspace
                .as_ref()
                .map_or(0, |workspace| workspace.calendar.plans.len()),
            plan_limit: MAX_PLANS,
            bytes,
            byte_limit: PROFILE_LIMIT,
        }
    }

    pub fn near_limit(self) -> bool {
        self.plans >= self.plan_limit * 9 / 10 || self.bytes >= self.byte_limit * 9 / 10
    }
}

#[derive(Clone)]
pub struct ProfileSnapshot {
    pub state: Arc<LocalState>,
    pub usage: ProfileUsage,
    pub reminder_receipts: Arc<BTreeSet<Reminder>>,
    pub reminder_issue: Option<ReminderFileIssue>,
}

#[derive(Clone)]
pub struct StoreReader(
    Arc<RwLock<ProfileSnapshot>>,
    #[cfg(feature = "desktop")]
    Arc<RwLock<import_status::View>>,
);

impl StoreReader {
    pub fn snapshot(&self) -> ProfileSnapshot {
        self.0.read().unwrap().clone()
    }

    #[cfg(feature = "desktop")]
    pub fn import_status(&self) -> Arc<[ImportStatus]> {
        self.1.read().unwrap().statuses()
    }

    #[cfg(feature = "desktop")]
    pub fn import_status_warning(&self) -> bool {
        self.1.read().unwrap().has_warning()
    }
}

pub struct Store {
    successor: Option<PathBuf>,
    state: Arc<LocalState>,
    path: PathBuf,
    reader: StoreReader,
    #[cfg(feature = "desktop")]
    import_log: Arc<ImportLog>,
    reminders: Arc<ReminderStore>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReminderRecovery {
    Reloaded,
    Reset,
}

pub struct ReminderStore {
    path: PathBuf,
    reader: StoreReader,
    writer: Mutex<()>,
    // A background repair may outlive the window that started it.
    _profile_lock: ProfileLock,
}

impl Store {
    pub fn open(directory: &Path) -> Fallible<SharedStore> {
        Self::open_profile(directory, true)
    }

    pub fn open_existing(directory: &Path) -> Fallible<SharedStore> {
        Self::open_profile(directory, false)
    }

    fn open_profile(directory: &Path, create: bool) -> Fallible<SharedStore> {
        #[cfg(windows)]
        if create && !directory.try_exists()? {
            access::create_windows_profile(directory)?;
        }
        #[cfg(unix)]
        if create {
            let mut directories = fs::DirBuilder::new();
            directories.recursive(true);
            use std::os::unix::fs::DirBuilderExt;
            directories.mode(0o700).create(directory)?;
        }
        let profile_lock = match open_profile_lock(directory) {
            Err(error)
                if !create
                    && error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                return Err(ProfileOpenIssue::CalendarMissing.into());
            }
            result => result?,
        };
        let path = directory.join("calendar.json");
        let (state, profile_bytes) = match read_calendar_file(&path) {
            Ok(result) => result,
            Err(error)
                if error.downcast_ref() == Some(&ProfileOpenIssue::CalendarMissing)
                    && create
                    && !directory.join(TEAM_MARKER).try_exists()?
                    && !backup::has_origin(directory)? =>
            {
                let state = LocalState {
                    device: Id::new_v4(),
                    join_proof: random_bytes()?,
                    enrollment_grants: BTreeMap::new(),
                    admitted_this_generation: BTreeSet::new(),
                    revision_floor: None,
                    workspace: None,
                    peers: BTreeSet::new(),
                    last_sync: None,
                    sync_paused: false,
                    legacy_delivery_attempts: [],
                    api: None,
                };
                let bytes = save_profile(&path, &state)?;
                (state, bytes)
            }
            Err(error) => return Err(error),
        };
        Self::open_saved_profile(directory, state, profile_bytes, profile_lock)
    }

    fn open_saved_profile(
        directory: &Path,
        state: LocalState,
        profile_bytes: usize,
        profile_lock: ProfileLock,
    ) -> Fallible<SharedStore> {
        if state.workspace.is_some() {
            mark_team_initialized(&directory.join(TEAM_MARKER))?;
        }
        discard_interrupted_writes(directory);
        let backups = Self::backup_folder(directory);
        if check_private_directory(&backups).is_ok() {
            discard_interrupted_writes(&backups);
        }
        let reminder_path = directory.join("reminders-v1.json");
        let (read_reminders, reminder_issue) = match load_reminder_file(&reminder_path) {
            Ok(receipts) => (receipts, None),
            Err(issue) => (BTreeSet::new(), Some(issue)),
        };
        let state = Arc::new(state);
        #[cfg(feature = "desktop")]
        let import_log = Arc::new(ImportLog::open(directory.join("imports-v1.json"), &state));
        let reader = StoreReader(
            Arc::new(RwLock::new(ProfileSnapshot {
                usage: ProfileUsage::from_state(&state, profile_bytes),
                state: Arc::clone(&state),
                reminder_receipts: Arc::new(read_reminders),
                reminder_issue,
            })),
            #[cfg(feature = "desktop")]
            Arc::clone(&import_log.view),
        );
        let reminders = Arc::new(ReminderStore {
            path: reminder_path,
            reader: reader.clone(),
            writer: Mutex::new(()),
            _profile_lock: profile_lock,
        });
        Ok(Arc::new(Mutex::new(Self {
            successor: None,
            state,
            path: directory.join("calendar.json"),
            reader,
            #[cfg(feature = "desktop")]
            import_log,
            reminders,
        })))
    }

    pub fn reader(&self) -> StoreReader {
        self.reader.clone()
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn import_log(&self) -> Arc<ImportLog> {
        Arc::clone(&self.import_log)
    }

    pub fn load_ui_drafts<T: DeserializeOwned>(&self) -> Fallible<Option<T>> {
        let path = self.path.with_file_name("drafts-v1.json");
        check_profile_file(&path)?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if file.metadata()?.len() > UI_DRAFT_LIMIT as u64 {
            return Err("Файл черновиков слишком большой.".into());
        }
        let mut limited = file.take(UI_DRAFT_LIMIT as u64 + 1);
        let drafts = serde_json::from_reader(io::BufReader::new(&mut limited))?;
        if limited.limit() == 0 {
            return Err("Файл черновиков слишком большой.".into());
        }
        Ok(Some(drafts))
    }

    pub fn save_ui_drafts<T: Serialize>(&self, drafts: Option<&T>) -> Fallible<()> {
        let path = self.path.with_file_name("drafts-v1.json");
        check_profile_file(&path)?;
        if let Some(drafts) = drafts {
            save(&path, drafts, UI_DRAFT_LIMIT)?;
        } else if let Err(error) = fs::remove_file(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error.into());
        }
        Ok(())
    }

    pub(crate) fn peer_routes(&self, capacity: usize) -> PeerRoutes {
        PeerRoutes::open(self.path.with_file_name("peer-routes-v1.json"), capacity)
    }

    pub fn state(&self) -> &LocalState {
        &self.state
    }

    pub fn sync_paused(&self) -> bool {
        self.state.sync_paused
    }

    pub fn set_sync_paused(&mut self, paused: bool) -> Fallible<()> {
        self.transaction(|state| {
            state.sync_paused = paused;
            Ok(())
        })
    }

    pub fn snapshot(&self) -> Arc<LocalState> {
        Arc::clone(&self.state)
    }

    pub fn reminder_receipts(&self) -> Arc<BTreeSet<Reminder>> {
        self.reader.snapshot().reminder_receipts
    }

    pub fn reminder_issue(&self) -> Option<ReminderFileIssue> {
        self.reader.snapshot().reminder_issue
    }

    pub fn recover_reminders(&mut self) -> Fallible<()> {
        self.reminders.recover().map(|_| ())
    }

    pub fn reminder_store(&self) -> Arc<ReminderStore> {
        Arc::clone(&self.reminders)
    }

    pub fn acknowledge_reminders(
        &mut self,
        today: chrono::NaiveDate,
        acknowledged: &[Reminder],
    ) -> Fallible<()> {
        self.reminders.acknowledge(today, acknowledged)
    }

    pub fn transaction<T>(
        &mut self,
        change: impl FnOnce(&mut LocalState) -> Fallible<T>,
    ) -> Fallible<T> {
        self.ensure_current_generation()?;
        let mut next = (*self.state).clone();
        let result = change(&mut next)?;
        if next != *self.state {
            if next.enrollment_grants.len() > 256 || next.admitted_this_generation.len() > 256 {
                return Err("Слишком много сохранённых приглашений.".into());
            }
            if let Some(workspace) = &next.workspace {
                workspace.calendar.validate()?;
                if self.state.workspace.is_none() {
                    // Record that this profile belongs to a team before publishing it.
                    mark_team_initialized(&self.path.with_file_name(TEAM_MARKER))?;
                }
            }
            let bytes = save_profile(&self.path, &next)?;
            self.publish_calendar(next, bytes);
        }
        Ok(result)
    }

    fn publish_calendar(&mut self, next: LocalState, bytes: usize) {
        self.state = Arc::new(next);
        let mut view = self.reader.0.write().unwrap();
        view.usage = ProfileUsage::from_state(&self.state, bytes);
        let previous = std::mem::replace(&mut view.state, Arc::clone(&self.state));
        drop(view);
        drop(previous);
    }
}

impl ReminderStore {
    pub fn recover(&self) -> Fallible<ReminderRecovery> {
        self.recover_with(|source, target| fs::hard_link(source, target))
    }

    fn recover_with(
        &self,
        link: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Fallible<ReminderRecovery> {
        let _writer = self.lock_writer()?;
        let (receipts, result) = match load_reminder_file(&self.path) {
            Ok(receipts) => (receipts, ReminderRecovery::Reloaded),
            Err(issue) if issue.can_recover() => {
                let filename = format!(
                    "reminders-damaged-{}.json",
                    uuid::Builder::from_random_bytes(random_bytes()?).into_uuid()
                );
                let backup = self.path.with_file_name(filename);
                // Publish the healthy replacement only after retaining its predecessor.
                preserve_original(&self.path, &backup, REMINDER_LIMIT, link)?;
                let receipts = BTreeSet::<Reminder>::new();
                save(&self.path, &receipts, REMINDER_LIMIT)?;
                (receipts, ReminderRecovery::Reset)
            }
            Err(issue) => return Err(issue.message().into()),
        };
        self.publish(receipts);
        Ok(result)
    }

    pub fn acknowledge(&self, today: chrono::NaiveDate, acknowledged: &[Reminder]) -> Fallible<()> {
        self.acknowledge_with(today, acknowledged, |receipts| {
            save(&self.path, receipts, REMINDER_LIMIT).map(|_| ())
        })
    }

    fn acknowledge_with(
        &self,
        today: chrono::NaiveDate,
        acknowledged: &[Reminder],
        write: impl FnOnce(&BTreeSet<Reminder>) -> Fallible<()>,
    ) -> Fallible<()> {
        let _writer = self.lock_writer()?;
        let snapshot = self.reader.snapshot();
        if let Some(issue) = snapshot.reminder_issue {
            return Err(issue.message().into());
        }
        let state = &snapshot.state;
        let workspace = state.workspace.as_ref().ok_or("Пространство недоступно.")?;
        let events = planning::personal_events(&workspace.calendar, today, state.device);
        let due = planning::due_reminders(&events, today);
        let next: BTreeSet<_> = snapshot
            .reminder_receipts
            .iter()
            .chain(acknowledged)
            .copied()
            .filter(|item| due.binary_search(item).is_ok())
            .collect();
        if next != *snapshot.reminder_receipts {
            write(&next)?;
            self.publish(next);
        }
        Ok(())
    }

    fn lock_writer(&self) -> Fallible<std::sync::MutexGuard<'_, ()>> {
        self.writer.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => {
                "Дождитесь завершения работы с файлом отметок.".into()
            }
            std::sync::TryLockError::Poisoned(_) => ReminderFileIssue::Unavailable.message().into(),
        })
    }

    fn publish(&self, receipts: BTreeSet<Reminder>) {
        let mut view = self.reader.0.write().unwrap();
        let previous = std::mem::replace(&mut view.reminder_receipts, Arc::new(receipts));
        view.reminder_issue = None;
        drop(view);
        drop(previous);
    }
}

fn load_reminder_file(path: &Path) -> Result<BTreeSet<Reminder>, ReminderFileIssue> {
    let unavailable = |_| ReminderFileIssue::Unavailable;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => return Err(ReminderFileIssue::Unavailable),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(_) => return Err(ReminderFileIssue::Unavailable),
    }
    let file = File::open(path).map_err(unavailable)?;
    if file.metadata().map_err(unavailable)?.len() > REMINDER_LIMIT as u64 {
        return Err(ReminderFileIssue::TooLarge);
    }
    let mut limited = file.take(REMINDER_LIMIT as u64 + 1);
    let result = serde_json::from_reader(io::BufReader::new(&mut limited));
    if limited.limit() == 0 {
        return Err(ReminderFileIssue::TooLarge);
    }
    result.map_err(|error| {
        if error.is_io() {
            ReminderFileIssue::Unavailable
        } else {
            ReminderFileIssue::InvalidData
        }
    })
}

struct ProfileLock(File);

impl Drop for ProfileLock {
    fn drop(&mut self) {
        // A concurrent process launch can briefly retain a duplicate of the
        // open file description until exec. End our lock at its last owner.
        #[cfg(unix)]
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
        #[cfg(not(unix))]
        let _ = self.0.unlock();
    }
}

fn open_profile_lock(directory: &Path) -> Fallible<ProfileLock> {
    validate_profile_access(directory)?;
    let lock_path = directory.join("profile.lock");
    check_profile_file(&lock_path)?;
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    // Use the Unix primitive directly on every Unix target. The std wrapper
    // reports Unsupported on Android even on kernels with flock support.
    #[cfg(unix)]
    match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => return Err(ProfileOpenIssue::Locked.into()),
        Err(error) => return Err(io::Error::from(error).into()),
    }
    #[cfg(not(unix))]
    match lock.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Err(ProfileOpenIssue::Locked.into()),
        Err(fs::TryLockError::Error(error)) => return Err(error.into()),
    }
    let lock = ProfileLock(lock);
    generations::ensure_no_successor(directory)?;
    Ok(lock)
}

fn read_calendar_file(path: &Path) -> Fallible<(LocalState, usize)> {
    check_profile_file(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ProfileOpenIssue::CalendarMissing.into());
        }
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() > PROFILE_LIMIT as u64 {
        return Err(ProfileOpenIssue::CalendarTooLarge.into());
    }
    let mut limited = file.take(PROFILE_LIMIT as u64 + 1);
    let parsed = serde_json::from_reader(io::BufReader::new(&mut limited));
    if limited.limit() == 0 {
        return Err(ProfileOpenIssue::CalendarTooLarge.into());
    }
    let state: LocalState = match parsed {
        Ok(state) => state,
        Err(error) if error.is_io() => return Err(error.into()),
        Err(_) => return Err(ProfileOpenIssue::InvalidCalendar.into()),
    };
    if let Some(workspace) = &state.workspace {
        if !workspace.valid_key_history() || workspace.is_revoked(state.device) {
            return Err(ProfileOpenIssue::InvalidCalendar.into());
        }
        workspace
            .calendar
            .validate()
            .map_err(|_| ProfileOpenIssue::InvalidCalendar)?;
    }
    let bytes = PROFILE_LIMIT + 1 - limited.limit() as usize;
    // Older JSON may omit defaulted fields. Measure the network form when
    // opening an existing calendar, even if the original file was compact.
    if let Some(workspace) = &state.workspace {
        crate::network::validate_enrollment_size(state.device, workspace)
            .map_err(|_| ProfileOpenIssue::CalendarTooLarge)?;
    }
    Ok((state, bytes))
}

fn check_profile_file(path: &Path) -> Fallible<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(format!(
            "Файл профиля должен быть обычным файлом: {}",
            path.display()
        )
        .into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn preserve_original(
    source: &Path,
    destination: &Path,
    limit: usize,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Fallible<()> {
    check_profile_file(source)?;
    match link(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Err(error.into()),
        Err(_) => {}
    }
    const TOO_LARGE: &str = "Предыдущий файл слишком велик для отдельного копирования.";
    let input = File::open(source)?;
    if input.metadata()?.len() > limit as u64 {
        return Err(TOO_LARGE.into());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(destination)?;
    let result = (|| -> Fallible<()> {
        if io::copy(&mut input.take(limit as u64 + 1), &mut output)? > limit as u64 {
            return Err(TOO_LARGE.into());
        }
        output.sync_all()?;
        Ok(())
    })();
    drop(output);
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn save(path: &Path, state: &impl Serialize, limit: usize) -> Fallible<usize> {
    let (staging, bytes) = stage(path, state, limit)?;
    staging.persist(path)?;
    Ok(bytes)
}

fn validate_profile_size(state: &LocalState, bytes: usize) -> Fallible<()> {
    // Small profiles already include more framing and local fields than
    // the largest network envelope, the successful enrollment response.
    if bytes > CALENDAR_MESSAGE_LIMIT
        && let Some(workspace) = &state.workspace
    {
        crate::network::validate_enrollment_size(state.device, workspace)
            .map_err(|_| ProfileSizeLimit::Calendar)?;
    }
    Ok(())
}

fn stage_profile(path: &Path, state: &LocalState) -> Fallible<(NamedTempFile, usize)> {
    let (staging, bytes) = stage(path, state, PROFILE_LIMIT).map_err(|error| {
        if error.is::<StagingSizeLimit>() {
            ProfileSizeLimit::File.into()
        } else {
            error
        }
    })?;
    validate_profile_size(state, bytes)?;
    Ok((staging, bytes))
}

fn save_profile(path: &Path, state: &LocalState) -> Fallible<usize> {
    let (staging, bytes) = stage_profile(path, state)?;
    staging.persist(path)?;
    Ok(bytes)
}

fn mark_team_initialized(path: &Path) -> io::Result<()> {
    match File::create_new(path) {
        Ok(marker) => marker.sync_all(),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn discard_interrupted_writes(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let filename = entry.file_name();
        if let Some(suffix) = filename
            .to_str()
            .and_then(|name| name.strip_prefix(STAGING_PREFIX))
            && suffix.len() == STAGING_SUFFIX_LEN
            && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            && entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn stage(path: &Path, state: &impl Serialize, limit: usize) -> Fallible<(NamedTempFile, usize)> {
    let mut staging = Builder::new()
        .prefix(STAGING_PREFIX)
        .rand_bytes(STAGING_SUFFIX_LEN)
        .tempfile_in(path.parent().ok_or("Не указана папка профиля")?)?;
    let bytes;
    {
        let mut writer = BufWriter::new(staging.as_file_mut());
        let mut bounded = LimitedWriter::new(&mut writer, limit);
        let result = serde_json::to_writer(&mut bounded, state);
        if bounded.exceeded {
            return Err(StagingSizeLimit(limit).into());
        }
        result?;
        bytes = limit - bounded.remaining;
        writer.flush()?;
    }
    staging.as_file().sync_all()?;
    Ok((staging, bytes))
}

pub(crate) struct LimitedWriter<W> {
    inner: W,
    remaining: usize,
    exceeded: bool,
}

impl<W> LimitedWriter<W> {
    pub(crate) fn new(inner: W, remaining: usize) -> Self {
        Self {
            inner,
            remaining,
            exceeded: false,
        }
    }
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.len() > self.remaining {
            self.exceeded = true;
            return Err(io::Error::other("Превышен лимит размера данных."));
        }
        let written = self.inner.write(data)?;
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsuitable_filesystem(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    #[test]
    fn reminder_recovery_retains_original_bytes_without_hard_links() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        drop(Store::open(&profile).unwrap());
        let original = b"{ unclear reminder receipts";
        let reminders = profile.join("reminders-v1.json");
        fs::write(&reminders, original).unwrap();

        let store = Store::open(&profile).unwrap();
        let store = store.lock().unwrap();
        let reader = store.reader();
        assert_eq!(
            reader.snapshot().reminder_issue,
            Some(ReminderFileIssue::InvalidData)
        );
        assert_eq!(
            store
                .reminder_store()
                .recover_with(unsuitable_filesystem)
                .unwrap(),
            ReminderRecovery::Reset,
        );
        assert_eq!(reader.snapshot().reminder_issue, None);
        assert!(reader.snapshot().reminder_receipts.is_empty());
        assert_eq!(fs::read(&reminders).unwrap(), b"[]");

        let archive = fs::read_dir(&profile)
            .unwrap()
            .map(|entry| entry.unwrap())
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("reminders-damaged-")
            })
            .unwrap();
        assert_eq!(fs::read(archive.path()).unwrap(), original);
    }

    #[test]
    fn failed_reminder_archive_keeps_the_source_and_visible_issue() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        drop(Store::open(&profile).unwrap());
        let reminders = profile.join("reminders-v1.json");
        let mut source = File::create(&reminders).unwrap();
        source.write_all(b"oversize").unwrap();
        let oversize = REMINDER_LIMIT as u64 + 1;
        source.set_len(oversize).unwrap();
        drop(source);

        let store = Store::open(&profile).unwrap();
        let store = store.lock().unwrap();
        let reader = store.reader();
        assert!(
            store
                .reminder_store()
                .recover_with(unsuitable_filesystem)
                .is_err()
        );
        assert_eq!(
            reader.snapshot().reminder_issue,
            Some(ReminderFileIssue::TooLarge)
        );
        let mut source = File::open(&reminders).unwrap();
        assert_eq!(source.metadata().unwrap().len(), oversize);
        let mut start = [0; 8];
        source.read_exact(&mut start).unwrap();
        assert_eq!(&start, b"oversize");
        assert!(!fs::read_dir(&profile).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("reminders-damaged-")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn last_reminder_owner_unlocks_even_while_a_duplicate_file_description_exists() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        let store = Store::open(&profile).unwrap();
        let reminders = store.lock().unwrap().reminder_store();
        let duplicate = reminders._profile_lock.0.try_clone().unwrap();
        drop(store);
        let busy = Store::open_existing(&profile).err().unwrap();
        assert_eq!(busy.downcast_ref(), Some(&ProfileOpenIssue::Locked));
        drop(reminders);
        let reopened = Store::open_existing(&profile).unwrap();
        drop(duplicate);
        let busy = Store::open_existing(&profile).err().unwrap();
        assert_eq!(busy.downcast_ref(), Some(&ProfileOpenIssue::Locked));
        drop(reopened);
    }

    #[test]
    fn reminder_repair_only_reserves_its_file_and_retains_the_profile_lock() {
        use std::{sync::mpsc::sync_channel, thread, time::Duration};

        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        drop(Store::open(&profile).unwrap());
        fs::write(profile.join("reminders-v1.json"), b"{ malformed").unwrap();
        let store = Store::open(&profile).unwrap();
        let reminders = store.lock().unwrap().reminder_store();
        let (started, ready) = sync_channel(1);
        let (resume, paused) = sync_channel(1);
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let timeout = Duration::from_secs(5);

        thread::scope(|scope| {
            let reminders = Arc::clone(&reminders);
            let repair = scope.spawn(move || {
                reminders.recover_with(|source, target| {
                    started.send(()).unwrap();
                    paused.recv_timeout(timeout).unwrap();
                    unsuitable_filesystem(source, target)
                })
            });
            ready.recv_timeout(timeout).unwrap();
            {
                let mut calendar = store.try_lock().unwrap();
                calendar
                    .transaction(|state| state.create_workspace("Команда".into(), "Ира".into()))
                    .unwrap();
                assert_eq!(
                    calendar.reminder_issue(),
                    Some(ReminderFileIssue::InvalidData)
                );
                assert!(
                    calendar
                        .acknowledge_reminders(today, &[])
                        .unwrap_err()
                        .to_string()
                        .contains("Дождитесь")
                );
            }
            drop(store);
            let error = Store::open(&profile).err().unwrap();
            assert_eq!(error.downcast_ref(), Some(&ProfileOpenIssue::Locked));
            resume.send(()).unwrap();
            assert_eq!(repair.join().unwrap().unwrap(), ReminderRecovery::Reset);
        });

        assert_eq!(reminders.recover().unwrap(), ReminderRecovery::Reloaded);
        drop(reminders);
        let reopened = Store::open(&profile).unwrap();
        let reopened = reopened.lock().unwrap();
        assert_eq!(reopened.state().workspace.as_ref().unwrap().name, "Команда");
        assert_eq!(reopened.reminder_issue(), None);
    }

    #[test]
    fn slow_read_receipt_allows_a_calendar_edit_and_does_not_hide_the_new_deadline() {
        use crate::{
            model::{Category, Event, Task},
            planning::Milestone,
        };
        use std::{sync::mpsc::sync_channel, thread, time::Duration};

        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        let store = Store::open(&profile).unwrap();
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let tomorrow = today.succ_opt().unwrap();
        let event = Id::new_v4();
        let task = Id::new_v4();
        let mut guard = store.lock().unwrap();
        guard
            .transaction(|state| {
                state.create_workspace("Команда".into(), "Ира".into())?;
                let revision = state.revision()?;
                let owner = state.device;
                state
                    .workspace_mut()?
                    .calendar
                    .plans
                    .entry(event)
                    .or_default()
                    .templates
                    .insert(
                        2026,
                        Record::new(
                            revision,
                            Some(
                                Event {
                                    title: "Проверить макет".into(),
                                    description: String::new(),
                                    category: Category::Team,
                                    date: today,
                                    yearly: true,
                                    preparation_days: 1,
                                    owner: Some(owner),
                                    tasks: vec![Task {
                                        id: task,
                                        title: "Согласовать".into(),
                                        days_before: 0,
                                    }],
                                }
                                .into(),
                            ),
                        ),
                    );
                Ok(())
            })
            .unwrap();
        let receipts = guard.reminder_store();
        drop(guard);
        let old = Reminder {
            date: today,
            event,
            year: 2026,
            kind: Milestone::Task(task),
        };
        let new = Reminder {
            date: tomorrow,
            ..old
        };
        let timeout = Duration::from_secs(5);
        let (started, ready) = sync_channel(1);
        let (resume, paused) = sync_channel(1);
        thread::scope(|scope| {
            let receipts = &receipts;
            let write = scope.spawn(move || {
                receipts.acknowledge_with(today, &[old], |next| {
                    started.send(()).unwrap();
                    paused.recv_timeout(timeout).unwrap();
                    save(&receipts.path, next, REMINDER_LIMIT).map(|_| ())
                })
            });
            ready.recv_timeout(timeout).unwrap();
            assert!(!receipts.reader.snapshot().reminder_receipts.contains(&old));
            store
                .try_lock()
                .unwrap()
                .transaction(|state| {
                    let revision = state.revision()?;
                    let plans = &mut state.workspace_mut()?.calendar.plans;
                    let record = plans
                        .get_mut(&event)
                        .unwrap()
                        .templates
                        .get_mut(&2026)
                        .unwrap();
                    record.revision = revision;
                    Arc::make_mut(record.value.as_mut().unwrap()).date = tomorrow;
                    Ok(())
                })
                .unwrap();
            resume.send(()).unwrap();
            write.join().unwrap().unwrap();
        });

        drop(receipts);
        drop(store);
        let reopened = Store::open(&profile).unwrap();
        let reopened = reopened.lock().unwrap();
        let state = reopened.state();
        let events = planning::personal_events(
            &state.workspace.as_ref().unwrap().calendar,
            today,
            state.device,
        );
        assert!(planning::due_reminders(&events, today).contains(&new));
        assert!(reopened.reminder_receipts().contains(&old));
        assert!(!reopened.reminder_receipts().contains(&new));
    }
}
