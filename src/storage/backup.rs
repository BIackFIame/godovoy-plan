use super::{
    Fallible, Id, LocalState, PROFILE_LIMIT, ProfileOpenIssue, SharedStore, Store,
    check_private_directory, check_profile_file, open_profile_lock, preserve_original,
    read_calendar_file, stage, stage_profile,
};
use blake2::{Blake2s256, Digest};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

const BACKUP_DIRECTORY: &str = "backups";
const ORIGIN_NAME: &str = "origin.json";
const ORIGIN_LIMIT: usize = 1024;
const VISIBLE_COPIES: usize = 32;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BackupEntry {
    pub created: DateTime<Utc>,
    id: Id,
    restore_point: bool,
}

impl BackupEntry {
    pub fn is_restore_point(&self) -> bool {
        self.restore_point
    }

    fn new(restore_point: bool) -> Fallible<Self> {
        Ok(Self {
            created: DateTime::from_timestamp_millis(Utc::now().timestamp_millis())
                .ok_or("Не удалось определить дату копии.")?,
            id: Id::new_v4(),
            restore_point,
        })
    }

    fn file_name(&self) -> String {
        let prefix = if self.restore_point {
            "restore-point-"
        } else {
            "calendar-"
        };
        format!(
            "{prefix}{}-{}.json",
            self.created.timestamp_millis(),
            self.id
        )
    }

    fn parse(name: &str) -> Option<Self> {
        let (body, restore_point) = if let Some(body) = name.strip_prefix("calendar-") {
            (body, false)
        } else {
            (name.strip_prefix("restore-point-")?, true)
        };
        let body = body.strip_suffix(".json")?;
        let (created, id) = body.split_at_checked(body.len().checked_sub(37)?)?;
        let entry = Self {
            created: DateTime::from_timestamp_millis(created.parse().ok()?)?,
            id: Id::parse_str(id.strip_prefix('-')?).ok()?,
            restore_point,
        };
        // Use a canonical name when recreating the path for a selection.
        (entry.file_name() == name).then_some(entry)
    }
}

#[derive(Debug)]
pub struct RestoreResult {
    pub original: Option<PathBuf>,
    pub working_snapshot: Option<PathBuf>,
    pub restore_point: Option<BackupEntry>,
    pub restore_point_warning: bool,
}

pub struct RestoredProfile {
    pub result: RestoreResult,
    /// The calendar was committed even if opening the profile failed.
    pub store: Fallible<SharedStore>,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct Origin {
    device: Id,
    workspace: Id,
    key_fingerprint: [u8; 32],
}

impl Origin {
    fn of(state: &LocalState) -> Fallible<Self> {
        let workspace = state.workspace.as_ref().ok_or("Календарь не создан.")?;
        Ok(Self {
            device: state.device,
            workspace: workspace.id,
            key_fingerprint: Blake2s256::digest(workspace.key).into(),
        })
    }

    fn require_match(&self, state: &LocalState) -> Fallible<()> {
        if *self != Self::of(state)? {
            return Err("Копия относится к другому профилю или календарю.".into());
        }
        Ok(())
    }
}

impl Store {
    pub fn backup_folder(directory: &Path) -> PathBuf {
        directory.join(BACKUP_DIRECTORY)
    }

    pub fn backup_directory(&self) -> Option<PathBuf> {
        self.path.parent().map(Self::backup_folder)
    }

    pub fn backups(directory: &Path) -> Fallible<Vec<BackupEntry>> {
        Self::list_backups(directory, false)
    }

    pub fn restore_choices(directory: &Path) -> Fallible<Vec<BackupEntry>> {
        Self::list_backups(directory, true)
    }

    fn list_backups(directory: &Path, include_restore_points: bool) -> Fallible<Vec<BackupEntry>> {
        check_private_directory(directory)?;
        let folder = Self::backup_folder(directory);
        match fs::symlink_metadata(&folder) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
            Ok(_) => check_private_directory(&folder)?,
        }
        let mut recent = BTreeSet::new();
        let mut restore_points = BTreeSet::new();
        for item in fs::read_dir(folder)? {
            let item = item?;
            if item.file_type()?.is_file()
                && let Some(entry) = item.file_name().to_str().and_then(BackupEntry::parse)
            {
                let entries = if entry.is_restore_point() {
                    if !include_restore_points {
                        continue;
                    }
                    &mut restore_points
                } else {
                    &mut recent
                };
                entries.insert(entry);
                if entries.len() > VISIBLE_COPIES {
                    entries.pop_first();
                }
            }
        }
        recent.append(&mut restore_points);
        Ok(recent.into_iter().rev().collect())
    }

    pub fn create_backup(&self) -> Fallible<BackupEntry> {
        let directory = self.path.parent().ok_or("Не указана папка профиля.")?;
        Self::write_calendar_backup(directory, &self.state, BackupEntry::new(false)?)
    }

    pub fn create_shared_backup(store: &SharedStore) -> Fallible<BackupEntry> {
        let (directory, state, copy) = {
            let store = store.lock().unwrap();
            let directory = store.path.parent().ok_or("Не указана папка профиля.")?;
            (
                directory.to_path_buf(),
                Arc::clone(&store.state),
                BackupEntry::new(false)?,
            )
        };
        Self::write_calendar_backup(&directory, &state, copy)
    }

    fn write_calendar_backup(
        directory: &Path,
        state: &LocalState,
        copy: BackupEntry,
    ) -> Fallible<BackupEntry> {
        let folder = Self::backup_folder(directory);
        create_folder(&folder)?;
        super::validate_profile_access(&folder)?;
        let origin_path = folder.join(ORIGIN_NAME);
        match read_origin(&origin_path)? {
            Some(saved) => saved.require_match(state)?,
            None => {
                let origin = Origin::of(state)?;
                let (file, _) = stage(&origin_path, &origin, ORIGIN_LIMIT)?;
                if let Err(error) = file.persist_noclobber(&origin_path) {
                    if error.error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                    let saved = read_origin(&origin_path)?
                        .ok_or("Сведения о профиле копии исчезли. Повторите попытку.")?;
                    saved.require_match(state)?;
                }
            }
        }

        write_backup(&folder, state, copy)
    }

    pub fn restore_backup(directory: &Path, backup: &BackupEntry) -> Fallible<RestoreResult> {
        let _lock = open_profile_lock(directory)?;
        let (_, _, result) = Self::replace_calendar(directory, backup, None)?;
        Ok(result)
    }

    pub fn restore_and_open_backup(
        directory: &Path,
        backup: &BackupEntry,
    ) -> Fallible<RestoredProfile> {
        let lock = open_profile_lock(directory)?;
        let (state, bytes, result) = Self::replace_calendar(directory, backup, None)?;
        let store = Self::open_saved_profile(directory, state, bytes, lock);
        Ok(RestoredProfile { result, store })
    }

    pub fn restore_open_backup(&mut self, backup: &BackupEntry) -> Fallible<RestoreResult> {
        self.ensure_current_generation()?;
        let directory = self.path.parent().ok_or("Не указана папка профиля.")?;
        let (restored, bytes, result) =
            Self::replace_calendar(directory, backup, Some(&self.state))?;
        self.publish_calendar(restored, bytes);
        Ok(result)
    }

    fn replace_calendar(
        directory: &Path,
        backup: &BackupEntry,
        live: Option<&LocalState>,
    ) -> Fallible<(LocalState, usize, RestoreResult)> {
        super::validate_profile_access(directory)?;
        let folder = Self::backup_folder(directory);
        check_private_directory(&folder)?;
        let origin = read_origin(&folder.join(ORIGIN_NAME))?
            .ok_or("Сведения о профиле копии отсутствуют.")?;
        let (mut restored, _) = read_calendar_file(&folder.join(backup.file_name()))?;
        origin.require_match(&restored)?;
        restored.api = None;
        restored.enrollment_grants.clear();

        let calendar = directory.join("calendar.json");
        let (disk, has_original) = match read_calendar_file(&calendar) {
            Ok((disk, _)) => {
                origin.require_match(&disk)?;
                (Some(disk), true)
            }
            Err(error) => match error.downcast_ref::<ProfileOpenIssue>() {
                Some(ProfileOpenIssue::CalendarMissing) => (None, false),
                Some(ProfileOpenIssue::InvalidCalendar | ProfileOpenIssue::CalendarTooLarge) => {
                    (None, true)
                }
                _ => return Err(error),
            },
        };
        let current_state = live.or(disk.as_ref());
        if let Some(state) = current_state {
            origin.require_match(state)?;
            let mut current = state.clone();
            current.revision_floor =
                Some(current.latest_revision().max(restored.latest_revision()));
            let workspace = restored.workspace.take().ok_or("Календарь не создан.")?;
            current.workspace_mut()?.calendar = Arc::unwrap_or_clone(workspace).calendar;
            restored = current;
        }
        restored.sync_paused = true;
        let (staged, bytes) = stage_profile(&calendar, &restored)?;
        let working_snapshot =
            if let Some(state) = live.filter(|state| Some(*state) != disk.as_ref()) {
                let name = directory.join(format!(
                    "calendar-open-before-restore-{}.json",
                    Id::new_v4()
                ));
                let (snapshot, _) = stage_profile(&name, state)?;
                snapshot.persist_noclobber(&name)?;
                Some(name)
            } else {
                None
            };
        let original = if has_original {
            let name = directory.join(format!("calendar-before-restore-{}.json", Id::new_v4()));
            preserve_original(&calendar, &name, PROFILE_LIMIT, |source, target| {
                fs::hard_link(source, target)
            })?;
            Some(name)
        } else {
            None
        };
        // The originals above are already preserved. If the backup folder is
        // read-only or this extra file does not fit, repair can still proceed.
        let (restore_point, restore_point_warning) = match current_state {
            Some(state) => {
                match BackupEntry::new(true).and_then(|copy| write_backup(&folder, state, copy)) {
                    Ok(point) => (Some(point), false),
                    Err(_) => (None, true),
                }
            }
            None => (None, false),
        };
        staged.persist(&calendar)?;
        Ok((
            restored,
            bytes,
            RestoreResult {
                original,
                working_snapshot,
                restore_point,
                restore_point_warning,
            },
        ))
    }
}

fn write_backup(folder: &Path, state: &LocalState, copy: BackupEntry) -> Fallible<BackupEntry> {
    let name = folder.join(copy.file_name());
    let mut calendar = state.clone();
    calendar.api = None;
    calendar.enrollment_grants.clear();
    let (file, _) = stage_profile(&name, &calendar)?;
    file.persist_noclobber(&name)?;
    Ok(copy)
}

pub(super) fn has_origin(directory: &Path) -> Fallible<bool> {
    let folder = Store::backup_folder(directory);
    match fs::symlink_metadata(&folder) {
        Ok(_) => check_private_directory(&folder)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    match fs::symlink_metadata(folder.join(ORIGIN_NAME)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn create_folder(folder: &Path) -> Fallible<()> {
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    match builder.create(folder) {
        Ok(()) => check_private_directory(folder),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            check_private_directory(folder)
        }
        Err(error) => Err(error.into()),
    }
}

fn read_origin(path: &Path) -> Fallible<Option<Origin>> {
    check_profile_file(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut content = Vec::new();
    file.take(ORIGIN_LIMIT as u64 + 1)
        .read_to_end(&mut content)?;
    if content.len() > ORIGIN_LIMIT {
        return Err("Сведения о профиле копии повреждены.".into());
    }
    serde_json::from_slice(&content)
        .map(Some)
        .map_err(|_| "Сведения о профиле копии повреждены.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_copy_falls_back_when_links_are_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("calendar.json");
        let archive = directory.path().join("archive.json");
        let bytes = b"{ damaged calendar";
        fs::write(&source, bytes).unwrap();
        let no_links = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::Unsupported));
        preserve_original(&source, &archive, PROFILE_LIMIT, no_links).unwrap();
        assert_eq!(fs::read(&source).unwrap(), bytes);
        assert_eq!(fs::read(&archive).unwrap(), bytes);
        assert!(preserve_original(&source, &archive, PROFILE_LIMIT, no_links).is_err());
        assert_eq!(fs::read(&archive).unwrap(), bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&archive).unwrap().permissions().mode() & 0o077,
                0
            );
        }
    }
}
