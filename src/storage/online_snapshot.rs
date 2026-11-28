//! Offline administrator operations. A backup is never a live profile and filesystem paths
//! never come from an HTTP request. Digests detect corruption, not malicious replacement.
use super::{
    ACCESS, ACCESS_LIMIT, DATA, DATA_LIMIT, LOCK, OnlineVault, evidence, save, sync_directory,
    validate_profile_access,
};
use super::super::access::{verify_profile_access, verify_staging_parent};
use crate::Fallible;
use blake2::{Blake2s256, Digest};
use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};
use tempfile::{Builder, TempDir};

const MANIFEST: &str = "backup-manifest-v1.json";
const MANIFEST_LIMIT: usize = 512 * 1024;
const MAX_PINNED_JOURNALS: usize = 64;
use super::{protection::REQUIRED as PROTECTION, standby::MARKER as STANDBY};

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Fingerprint {
    bytes: u64,
    blake2s256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Manifest {
    version: u8,
    created_at: DateTime<Utc>,
    standby_pin: bool,
    files: BTreeMap<String, Fingerprint>,
}

pub(super) fn private_file(path: &Path) -> Fallible<File> {
    let (file, _metadata) = super::open_regular(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // open_regular already checked the owner and link count on this handle.
        if _metadata.mode() & 0o077 != 0 {
            return Err("Файл должен быть закрытым, принадлежать текущему пользователю и не иметь других ссылок.".into());
        }
    }
    Ok(file)
}

// The shared macOS checker validates the profile ACL and legacy profile files.
// The backup has additional payload and journal filenames; check their ACLs in batches, too.
pub(super) fn validate_evidence_directory(dir: &Path) -> Fallible<()> {
    verify_profile_access(dir)?;
    #[cfg(target_os = "macos")]
    {
        let mut paths = Vec::new();
        for item in fs::read_dir(dir)? {
            let item = item?;
            let name = item.file_name();
            if name.to_str().is_some_and(|s| {
                s == MANIFEST
                    || s == evidence::RECEIPT
                    || file_limit(s.strip_prefix("part-").unwrap_or(s)).is_some()
            }) {
                paths.push(item.path());
            }
        }
        super::super::access::check_mac_acl(&paths, false)?;
    }
    Ok(())
}

fn create_file(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn copy_file(src: &Path, dest: &Path, limit: u64) -> Fallible<Fingerprint> {
    let input = private_file(src)?;
    let size = input.metadata()?.len();
    if size > limit {
        return Err("Превышен размер файла резервной копии.".into());
    }
    copy_reader(input, dest, size)
}

fn copy_reader(mut input: impl Read, dest: &Path, size: u64) -> Fallible<Fingerprint> {
    let mut output = create_file(dest)?;
    let mut hash = Blake2s256::new();
    let mut bytes = 0u64;
    let mut buf = [0u8; 32768];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > size {
            return Err("Файл изменился во время копирования.".into());
        }
        hash.update(&buf[..n]);
        output.write_all(&buf[..n])?;
    }
    if bytes != size {
        return Err("Файл изменился во время копирования.".into());
    }
    output.sync_all()?;
    Ok(Fingerprint {
        bytes,
        blake2s256: format!("{:x}", hash.finalize()),
    })
}

fn file_limit(name: &str) -> Option<u64> {
    match name {
        ACCESS => Some(ACCESS_LIMIT as u64),
        DATA => Some(DATA_LIMIT as u64),
        STANDBY => Some(2048),
        PROTECTION => Some(8),
        _ => evidence::audit_month(name).map(|_| u64::MAX),
    }
}

fn staging(parent: &Path) -> Fallible<TempDir> {
    let mut builder = Builder::new();
    builder.prefix(".godovoy-backup-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let dir = builder.tempdir_in(parent)?;
    validate_profile_access(dir.path())?;
    Ok(dir)
}

fn parent_for_new(path: &Path, excluded: &Path) -> Fallible<PathBuf> {
    let path = std::path::absolute(path)?;
    let parent = path.parent().ok_or("Укажите новое имя в частной папке.")?;
    verify_staging_parent(parent)?;
    let parent = parent.canonicalize()?;
    for ancestor in parent.ancestors() {
        if ancestor == excluded
            || ancestor.join(ACCESS).try_exists()?
            || ancestor.join(MANIFEST).try_exists()?
            || ancestor.join("calendar.json").try_exists()?
        {
            return Err(
                "Для результата нужна отдельная папка вне календаря и резервной копии.".into(),
            );
        }
    }
    let dest = parent.join(path.file_name().ok_or("Укажите новое имя папки.")?);
    match fs::symlink_metadata(&dest) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(dest),
        Ok(_) => Err("Папка назначения уже существует.".into()),
        Err(e) => Err(e.into()),
    }
}

fn publish(temp: TempDir, dest: &Path) -> Fallible<()> {
    sync_directory(temp.path())?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        renameat_with(CWD, temp.path(), CWD, dest, RenameFlags::NOREPLACE)?;
    }
    #[cfg(windows)]
    {
        // MoveFileExW cannot replace an existing directory.
        fs::rename(temp.path(), dest)?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    return Err("Публикация копии не поддерживается на этой ОС.".into());
    let _ = temp.keep();
    sync_directory(
        dest.parent()
            .ok_or("У назначения нет родительской папки.")?,
    )
}

fn validate_journal(path: &Path, month: chrono::NaiveDate) -> Fallible<()> {
    let mut reader = BufReader::new(private_file(path)?);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = (&mut reader).take(8193).read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if n > 8192 || line.last() != Some(&b'\n') {
            return Err("Незавершённая или слишком длинная строка журнала.".into());
        }
        #[derive(Deserialize)]
        struct Entry {
            at: DateTime<Utc>,
            action: String,
            #[serde(rename = "credential")]
            _credential: Option<super::Id>,
            #[serde(rename = "employee")]
            _employee: Option<super::Id>,
            #[serde(rename = "object")]
            _object: Option<super::Id>,
            #[serde(rename = "protection")]
            _protection: Option<super::ProtectionState>,
        }
        let entry: Entry = serde_json::from_slice(&line)?;
        if (entry.at.year(), entry.at.month()) != (month.year(), month.month())
            || entry.action.is_empty()
            || entry.action.len() > 128
        {
            return Err("Некорректная запись месячного журнала.".into());
        }
    }
    Ok(())
}

fn read_manifest(backup: &Path) -> Fallible<Manifest> {
    validate_evidence_directory(backup)?;
    let file = private_file(&backup.join(MANIFEST))?;
    if file.metadata()?.len() > MANIFEST_LIMIT as u64 {
        return Err("Слишком большой манифест.".into());
    }
    let manifest: Manifest = super::read_bounded_json(file, MANIFEST_LIMIT)?;
    if manifest.version != 1
        || !manifest.files.contains_key(ACCESS)
        || !manifest.files.contains_key(DATA)
        || (manifest.standby_pin && !manifest.files.contains_key(STANDBY))
    {
        return Err("Неполный или неподдерживаемый манифест.".into());
    }
    let mut files = BTreeSet::from([String::from(MANIFEST)]);
    for (name, digest) in &manifest.files {
        if file_limit(name).is_none_or(|limit| digest.bytes > limit)
            || digest.blake2s256.len() != 64
        {
            return Err("Недопустимая запись манифеста.".into());
        }
        files.insert(format!("part-{name}"));
    }
    for entry in fs::read_dir(backup)? {
        if !entry?
            .file_name()
            .to_str()
            .is_some_and(|name| files.remove(name))
        {
            return Err("В копии обнаружены незаявленные файлы.".into());
        }
    }
    if !files.is_empty() {
        return Err("В копии отсутствуют заявленные файлы.".into());
    }
    Ok(manifest)
}

fn reconstruct(backup: &Path, out: &Path) -> Fallible<Manifest> {
    let manifest = read_manifest(backup)?;
    for (name, fingerprint) in &manifest.files {
        let dest = out.join(name);
        let actual = copy_file(
            &backup.join(format!("part-{name}")),
            &dest,
            fingerprint.bytes,
        )?;
        if &actual != fingerprint {
            return Err("Контрольная сумма файла резервной копии не совпадает.".into());
        }
        if let Some(month) = evidence::audit_month(name) {
            validate_journal(&dest, month)?;
        }
    }
    let pin = create_file(&out.join(LOCK))?;
    pin.set_len(u64::from(manifest.standby_pin))?;
    pin.sync_all()?;
    drop(pin);
    let vault = OnlineVault::open(out)?;
    let lock = vault.lock()?;
    lock.operator_record()?;
    lock.data()?;
    lock.standby_window()?;
    Ok(manifest)
}

struct CapturedBackup {
    manifest: Manifest,
    payloads: Vec<(String, Vec<u8>)>,
    journals: Vec<(String, io::Take<File>)>,
}

impl CapturedBackup {
    fn capture(vault: &OnlineVault, staging: &Path) -> Fallible<Self> {
        let lock = vault.lock()?;
        validate_evidence_directory(lock.directory)?;
        lock.access()?;
        lock.standby_window()?;
        lock.audit_security("backup_requested", None, None, None)?;

        let mut captured = Self {
            manifest: Manifest {
                version: 1,
                created_at: Utc::now(),
                standby_pin: lock._lock.metadata()?.len() == 1,
                files: BTreeMap::new(),
            },
            payloads: Vec::new(),
            journals: Vec::new(),
        };
        for entry in fs::read_dir(lock.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(limit) = file_limit(name) else {
                continue;
            };
            let file = private_file(&entry.path())?;
            let size = file.metadata()?.len();
            if size > limit {
                return Err("Превышен размер файла резервной копии.".into());
            }
            if evidence::audit_month(name).is_some() {
                if captured.journals.len() < MAX_PINNED_JOURNALS {
                    // Writers only append complete records under the same vault lock. Keep
                    // this file identity and stop exactly at the last captured record.
                    captured.journals.push((name.to_owned(), file.take(size)));
                } else {
                    // Retain support for long histories even on systems with few file handles.
                    let digest = copy_reader(file, &staging.join(format!("part-{name}")), size)?;
                    captured.manifest.files.insert(name.to_owned(), digest);
                }
            } else {
                // Release handles to atomically replaced Windows files before unlocking.
                let mut bytes = Vec::with_capacity(usize::try_from(size)?);
                file.take(size + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 != size {
                    return Err("Файл изменился во время копирования.".into());
                }
                captured.payloads.push((name.to_owned(), bytes));
            }
        }
        Ok(captured)
    }

    fn write_to(mut self, staging: &Path) -> Fallible<()> {
        for (name, bytes) in self.payloads {
            let digest = copy_reader(
                bytes.as_slice(),
                &staging.join(format!("part-{name}")),
                bytes.len() as u64,
            )?;
            self.manifest.files.insert(name, digest);
        }
        for (name, prefix) in self.journals {
            let size = prefix.limit();
            let digest = copy_reader(prefix, &staging.join(format!("part-{name}")), size)?;
            self.manifest.files.insert(name, digest);
        }
        save(&staging.join(MANIFEST), &self.manifest, MANIFEST_LIMIT)?;
        Ok(())
    }
}

impl OnlineVault {
    pub fn create_verified_backup(&self, destination: &Path) -> Fallible<Value> {
        let dest = parent_for_new(destination, &self.0)?;
        let parent = dest.parent().ok_or("Отсутствует родительская папка.")?;
        let snapshot = staging(parent)?;
        CapturedBackup::capture(self, snapshot.path())?.write_to(snapshot.path())?;
        let out = staging(parent)?;
        let verified = reconstruct(snapshot.path(), out.path())?;
        drop(out);
        publish(snapshot, &dest)?;
        let at = Utc::now();
        let lock = self.lock()?;
        lock.audit_security("backup_verified", None, None, None)?;
        save(&lock.directory.join(evidence::RECEIPT), &at, 128)?;
        sync_directory(lock.directory)?;
        Ok(json!({"verifiedAt":at,"files":verified.files.len(),"createdAt":verified.created_at}))
    }

    pub fn verify_backup(backup: &Path) -> Fallible<Value> {
        verify_profile_access(backup)?;
        let canonical = backup.canonicalize()?;
        let parent = canonical
            .parent()
            .ok_or("У копии нет родительской папки.")?;
        verify_staging_parent(parent)?;
        let out = staging(parent)?;
        let verified = reconstruct(&canonical, out.path())?;
        Ok(
            json!({"verifiedAt":Utc::now(),"files":verified.files.len(),"createdAt":verified.created_at}),
        )
    }

    pub fn restore_backup(backup: &Path, destination: &Path) -> Fallible<Value> {
        verify_profile_access(backup)?;
        let canonical = backup.canonicalize()?;
        let dest = parent_for_new(destination, &canonical)?;
        let out = staging(dest.parent().ok_or("Отсутствует родительская папка.")?)?;
        let verified = reconstruct(&canonical, out.path())?;
        let vault = OnlineVault::open(out.path())?;
        let lock = vault.lock()?;
        let revoked = lock.revoke_all()?;
        lock.audit_security("backup_restored", None, None, None)?;
        let restored_at = Utc::now();
        save(&lock.directory.join(evidence::RECEIPT), &restored_at, 128)?;
        drop(lock);
        publish(out, &dest)?;
        Ok(json!({"restoredAt":restored_at,"files":verified.files.len(),"revoked":revoked}))
    }
}

#[cfg(test)]
#[path = "online_snapshot_tests.rs"]
mod tests;
