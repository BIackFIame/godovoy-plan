//! Protect local profile data with the logged-in user's filesystem permissions.
//! The profile contains a shared team secret. An exchange file has a different format.
#[cfg(unix)]
use std::io;
use std::{fs, path::Path};

use super::Fallible;

pub(super) fn check_directory(path: &Path) -> Fallible<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(format!("Нужна обычная папка профиля: {}", path.display()).into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
            return Err(format!("Папка должна принадлежать текущему пользователю и быть закрыта для остальных (0700): {}", path.display()).into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("Нужна локальная папка профиля без точек повторной обработки.".into());
        }
    }
    Ok(())
}

/// Inspect the handle that will actually be used. Never follow the final
/// component or wait for a special file while deciding whether it is regular.
pub(super) fn open_regular(
    path: &Path,
    options: &mut fs::OpenOptions,
) -> Fallible<(fs::File, fs::Metadata)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err("Ожидался обычный файл хранилища.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 { // FILE_ATTRIBUTE_REPARSE_POINT
            return Err("Файл содержит точку повторной обработки.".into());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err("Файл должен принадлежать текущему пользователю и не иметь других ссылок.".into());
        }
    }
    Ok((file, metadata))
}

pub(super) fn open_lock(path: &Path, options: &mut fs::OpenOptions) -> Fallible<fs::File> {
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Readers and writers may wait on the same lock. Pin its name while open.
        options.share_mode(0x1 | 0x2); // FILE_SHARE_READ | FILE_SHARE_WRITE
    }
    let (file, _metadata) = open_regular(path, options)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if _metadata.mode() & 0o077 != 0 {
            return Err("Файл блокировки должен иметь закрытые права (0600).".into());
        }
    }
    Ok(file)
}

#[cfg(unix)]
fn managed(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "calendar.json"
            | "online-access-v1.json"
            | "online-calendar-v1.json"
            | "online-protection-required-v1.json"
            | "online-standby-v1.json"
            | "online-backup-check-v1.json"
            | "profile.lock"
            | "team-initialized"
            | "reminders-v1.json"
            | "drafts-v1.json"
            | "imports-v1.json"
            | "bridge-runs-v1.json"
            | "peer-routes-v1.json"
            | "origin.json"
            | "active-successor-v1.json"
    ) || name.starts_with(".godovoy-stage-")
        || (name.ends_with(".json")
            && ["calendar-", "restore-point-", "reminders-damaged-"]
                .iter()
                .any(|prefix| name.starts_with(prefix)))
}

#[cfg(unix)]
fn inspect_file(path: &Path, repair: bool) -> Fallible<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let found = fs::symlink_metadata(path)?;
    if !found.is_file() || found.uid() != rustix::process::geteuid().as_raw() {
        return Err(format!(
            "Файл профиля должен принадлежать текущему пользователю и быть обычным файлом: {}",
            path.display()
        )
        .into());
    }
    if found.mode() & 0o077 == 0 {
        return Ok(());
    }
    if !repair {
        return Err(format!("Файл должен иметь закрытые права (0600): {}", path.display()).into());
    }
    // Narrow permissions on old, user-owned single-link files only. An
    // external hard link might designate data outside the profile.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.nlink() != 1
        || meta.mode() & 0o022 != 0
    {
        return Err(format!("Проверьте владельца, жёсткие ссылки и права файла профиля; нужны закрытые права (0600): {}", path.display()).into());
    }
    file.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o700))?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn check_mac_acl(paths: &[std::path::PathBuf], ancestors: bool) -> Fallible<()> {
    use std::process::Command;
    for batch in paths.chunks(32) {
        // Numeric IDs, escaped filenames, ACL entries on following lines.
        // Reject extended ACLs (including inherited ACEs). Do not strip them.
        let out = Command::new("/bin/ls")
            .args(["-ldbne", "--"])
            .args(batch)
            .env_clear()
            .env("LC_ALL", "C")
            .output()?;
        if !out.status.success() || !out.stderr.is_empty() {
            return Err("Не удалось проверить ACL файлов профиля в macOS.".into());
        }
        if out.stdout.split(|b| *b == b'\n').any(|line| {
            let trimmed = line.trim_ascii_start();
            let Some(colon) = trimmed.iter().position(|b| *b == b':') else {
                return false;
            };
            if colon == 0 || !trimmed[..colon].iter().all(u8::is_ascii_digit) {
                return false;
            }
            // macOS prints numeric ACLs as "index: principal [inherited]
            // allow|deny rights". A deny-only entry on an ancestor is safe.
            let effect = line.rsplit(|byte| byte.is_ascii_whitespace()).nth(1);
            !ancestors || effect != Some(&b"deny"[..])
        }) {
            return Err(format!("Используйте закрытую папку профиля без дополнительных ACL в macOS. Проверьте права в {}.", batch[0].parent().unwrap_or(&batch[0]).display()).into());
        }
    }
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "android", target_os = "ios"))))]
fn validate_unix_ancestors(directory: &Path, absolute: &Path) -> Fallible<()> {
    use std::os::unix::fs::MetadataExt;
    let lexical = std::path::absolute(directory)?;
    let mut ancestors = std::collections::BTreeSet::new();
    for ancestor in lexical
        .ancestors()
        .skip(1)
        .chain(absolute.ancestors().skip(1))
    {
        if !ancestors.insert(ancestor.to_owned()) {
            continue;
        }
        let meta = fs::symlink_metadata(ancestor)?;
        if meta.is_symlink() && meta.uid() == 0 {
            // Standard macOS aliases such as /var and /tmp are root-owned.
            continue;
        }
        if (meta.uid() != rustix::process::geteuid().as_raw() && meta.uid() != 0)
            || (meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0)
        {
            return Err(format!(
                "Путь к профилю проходит через доступную для подмены папку: {}",
                ancestor.display()
            )
            .into());
        }
    }
    #[cfg(target_os = "macos")]
    check_mac_acl(&ancestors.into_iter().collect::<Vec<_>>(), true)?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn validate_profile_access(directory: &Path) -> Fallible<()> {
    inspect_profile(directory, true)
}

#[cfg(all(unix, feature = "server"))]
pub(super) fn verify_profile_access(directory: &Path) -> Fallible<()> {
    inspect_profile(directory, false)
}

#[cfg(unix)]
fn private_directory_path(directory: &Path) -> Fallible<std::path::PathBuf> {
    check_directory(directory)?;
    let absolute = fs::canonicalize(directory)?;
    // The native mobile shells supply their platform-owned private app container.
    // Its ancestors belong to Android/iOS system users and are managed by the OS sandbox.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    validate_unix_ancestors(directory, &absolute)?;
    Ok(absolute)
}

#[cfg(all(unix, feature = "server"))]
pub(super) fn verify_staging_parent(directory: &Path) -> Fallible<()> {
    let _absolute = private_directory_path(directory)?;
    #[cfg(target_os = "macos")]
    check_mac_acl(&[_absolute], false)?;
    Ok(())
}

#[cfg(unix)]
fn inspect_profile(directory: &Path, repair: bool) -> Fallible<()> {
    let mut paths = vec![private_directory_path(directory)?];
    let mut files = Vec::new();
    let backup = directory.join("backups");
    match fs::symlink_metadata(&backup) {
        Ok(_) => {
            check_directory(&backup)?;
            paths.push(std::path::absolute(&backup)?);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    for folder in &paths {
        for entry in fs::read_dir(folder)? {
            let entry = entry?;
            if !managed(&entry.file_name()) {
                continue;
            }
            let ty = entry.file_type()?;
            if ty.is_dir() {
                // A failed local journal may deliberately be left unavailable.
                // Its consumer handles this condition without following it.
                continue;
            }
            files.push(entry.path());
        }
    }
    #[cfg(target_os = "macos")]
    {
        paths.extend(files.iter().cloned());
        check_mac_acl(&paths, false)?;
    }
    for file in files {
        inspect_file(&file, repair)?;
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn validate_profile_access(directory: &Path) -> Fallible<()> {
    check_directory(directory)?;
    run_windows_acl(directory, false)
}

#[cfg(all(windows, feature = "server"))]
pub(super) use validate_profile_access as verify_profile_access;
#[cfg(all(windows, feature = "server"))]
pub(super) use validate_profile_access as verify_staging_parent;

#[cfg(windows)]
pub(super) fn create_windows_profile(directory: &Path) -> Fallible<()> {
    run_windows_acl(directory, true)
}

#[cfg(windows)]
fn run_windows_acl(directory: &Path, create: bool) -> Fallible<()> {
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };
    use winreg::{
        RegKey,
        enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY},
    };
    let os = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        KEY_READ | KEY_WOW64_64KEY,
    )?;
    let system_root: String = os.get_value("SystemRoot")?;
    let windows = Path::new(&system_root);
    if !windows.is_absolute() {
        return Err("Не удалось определить системную папку Windows.".into());
    }
    let shell_dir = windows.join(r"System32\WindowsPowerShell\v1.0");
    let mut process = Command::new(shell_dir.join("powershell.exe"));
    process
        .env_clear()
        .env("SystemRoot", windows)
        .env("WINDIR", windows)
        .env("PSModulePath", shell_dir.join("Modules"))
        .env("GODOVOY_PROFILE_ACL", std::path::absolute(directory)?)
        .env("GODOVOY_CREATE_PROFILE", if create { "1" } else { "0" })
        .current_dir(&shell_dir)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            include_str!("verify-profile-acl.ps1"),
        ])
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = process.spawn().map_err(
        |_| "Не удалось запустить системную проверку ACL Windows. Нужен Windows PowerShell.",
    )?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait()? {
            return match status.code() {
                Some(0) => Ok(()),
                Some(21) => Err(format!("Папка, файл профиля или резервная копия доступны посторонним. Ограничьте права текущим пользователем, SYSTEM и администраторами, включая наследование: {}", directory.display()).into()),
                Some(22) => Err("Профилю нужен локальный NTFS-диск и путь без точек повторной обработки.".into()),
                Some(23) => Err("Родительская папка позволяет другим пользователям подменить путь к профилю. Выберите закрытую папку в LocalAppData.".into()),
                _ => Err(format!("Windows не подтвердила права доступа к профилю. Проверьте доступность Windows PowerShell и ACL: {}", directory.display()).into()),
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Проверка прав Windows не завершилась вовремя, профиль не открыт.".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod lock_files {
    use super::*;

    #[test]
    fn opens_normal_lock_and_rejects_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = tmp.path().join("profile.lock");
        let created = open_lock(&lock, fs::OpenOptions::new().create_new(true)).unwrap();
        drop(created);
        assert!(open_lock(&lock, &mut fs::OpenOptions::new()).is_ok());
        assert!(open_lock(tmp.path(), &mut fs::OpenOptions::new()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn lock_requires_private_permissions_and_a_single_name() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let lock = tmp.path().join("profile.lock");
        let alias = tmp.path().join("other.lock");
        let created = open_lock(&lock, fs::OpenOptions::new().create_new(true)).unwrap();
        drop(created);
        fs::hard_link(&lock, &alias).unwrap();
        assert!(open_lock(&lock, &mut fs::OpenOptions::new()).is_err());
        fs::remove_file(alias).unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_lock(&lock, &mut fs::OpenOptions::new()).is_err());
        assert_eq!(fs::metadata(lock).unwrap().permissions().mode() & 0o777, 0o644);
    }
}
