//! Operator-prepared space on the ACL volume. All operations hold the vault lock.
use super::{LockedVault, access, sync_directory};
use crate::Fallible;
use std::{
    fs::{File, OpenOptions},
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};

const NAME: &str = "revoke-space-v1.bin";
const MAGIC: &[u8] = b"Godovoy revoke space v1\n";
const SIZE: u64 = 2 * 1024 * 1024;
// APFS can report several MiB available after returning ENOSPC.
const LOW_SPACE: u64 = 8 * 1024 * 1024;

impl LockedVault<'_> {
    // An absent reserve is an ordinary profile. Refuse to modify an unrelated,
    // shared, sparse or unrecognised file even if it occupies the reserved name.
    fn revoke_space_file(&self, options: &mut OpenOptions) -> Fallible<Option<File>> {
        let path = self.directory.join(NAME);
        let (mut file, metadata) = match access::open_regular(&path, options.read(true)) {
            Ok(value) => value,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if metadata.mode() & 0o777 != 0o600
            || metadata.len() > SIZE
            || metadata.dev() != self._lock.metadata()?.dev()
            || metadata.blocks().saturating_mul(512) < metadata.len()
        {
            return Err("Неверный файл резерва отзыва, его права или выделенное место.".into());
        }
        let mut marker = [0; MAGIC.len()];
        let used = metadata.len().min(MAGIC.len() as u64) as usize;
        file.read_exact(&mut marker[..used])?;
        if marker[..used] != MAGIC[..used] {
            return Err("Неверное содержимое файла резерва отзыва.".into());
        }
        Ok(Some(file))
    }

    pub fn revoke_space_ready(&self) -> Fallible<bool> {
        let Some(file) = self.revoke_space_file(&mut OpenOptions::new())? else {
            return Ok(false);
        };
        #[cfg(target_os = "macos")]
        access::check_mac_acl(&[self.directory.join(NAME)], false)?;
        Ok(file.metadata()?.len() == SIZE)
    }

    pub fn prepare_revoke_space(&self) -> Fallible<()> {
        let path = self.directory.join(NAME);
        let old = self.revoke_space_file(OpenOptions::new().write(true))?;
        let previous = old
            .as_ref()
            .map(|f| f.metadata().map(|m| m.len()))
            .transpose()?
            .unwrap_or(0);
        if previous < SIZE {
            let space = rustix::fs::fstatvfs(&self._lock)?;
            if space.f_bavail.saturating_mul(space.f_frsize) < SIZE - previous + LOW_SPACE {
                return Err("Сначала освободите место для подготовки резерва отзыва.".into());
            }
        }
        let mut file = match old {
            Some(file) => file,
            None => {
                let mut options = OpenOptions::new();
                options.read(true).write(true).create_new(true).mode(0o600);
                let (file, _) = access::open_regular(&path, &mut options)?;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                file
            }
        };
        #[cfg(target_os = "macos")]
        access::check_mac_acl(std::slice::from_ref(&path), false)?;
        let mut length = file.seek(SeekFrom::End(0))?;
        if length < MAGIC.len() as u64 {
            file.write_all(&MAGIC[length as usize..])?;
            file.sync_all()?;
            sync_directory(self.directory)?;
            length = MAGIC.len() as u64;
        }
        let result = (|| -> Fallible<()> {
            let mut chunk = [0u8; 32768];
            let mut left = (SIZE - length) as usize;
            while left != 0 {
                let step = left.min(chunk.len());
                getrandom::fill(&mut chunk[..step])?;
                file.write_all(&chunk[..step])?;
                left -= step;
            }
            file.sync_all()?;
            if file.metadata()?.blocks().saturating_mul(512) < SIZE {
                return Err("Файловая система не подтвердила физическое выделение резерва.".into());
            }
            sync_directory(self.directory)
        })();
        if result.is_err() && length < SIZE {
            file.set_len(length)?;
            file.sync_all()?;
        }
        result
    }

    pub(super) fn release_revoke_space(&self, force: bool) {
        // Space reclamation is optional. A failure here must not impede a
        // revocation whose mandatory audit, ACL commit and directory sync succeed.
        let _ = self.try_release_revoke_space(force);
    }

    fn try_release_revoke_space(&self, force: bool) -> Fallible<()> {
        let Some(file) = self.revoke_space_file(&mut OpenOptions::new())? else {
            return Ok(());
        };
        if file.metadata()?.len() == SIZE
            && (force
                || rustix::fs::fstatvfs(&file)
                    .is_ok_and(|space| space.f_bavail.saturating_mul(space.f_frsize) < LOW_SPACE))
        {
            #[cfg(target_os = "macos")]
            if access::check_mac_acl(&[self.directory.join(NAME)], false).is_err() {
                return Ok(());
            }
            // A full APFS volume can reject even truncate(0); unlink can still
            // release the blocks. The vault lock also serializes other preparers.
            let path = self.directory.join(NAME);
            let opened = file.metadata()?;
            let named = std::fs::symlink_metadata(&path)?;
            if named.dev() != opened.dev() || named.ino() != opened.ino() {
                return Ok(());
            }
            std::fs::remove_file(path)?;
            drop(file);
            sync_directory(self.directory)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{Calendar, Id},
        storage::online::OnlineVault,
    };

    #[test]
    fn only_valid_revocation_may_consume_an_opted_in_reserve() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("office");
        let vault = OnlineVault::init(
            &profile,
            "Тест".into(),
            Calendar::default(),
            Default::default(),
        )
        .unwrap();
        let lock = vault.lock().unwrap();
        assert!(!lock.revoke_space_ready().unwrap());
        lock.prepare_revoke_space().unwrap();
        let path = profile.join(NAME);
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert!(metadata.blocks() * 512 >= SIZE);
        lock.prepare_revoke_space().unwrap();
        assert_eq!(metadata.ino(), std::fs::metadata(&path).unwrap().ino());
        assert!(lock.revoke_with_reserve(Id::nil(), true).is_err());
        assert_eq!(lock.revoke_all_with_reserve(true).unwrap(), 0);
        assert!(lock.revoke_space_ready().unwrap());
        let (id, _, _) = lock.grant("Сотрудник", None, false).unwrap();
        assert!(lock.revoke_space_ready().unwrap());
        lock.revoke_with_reserve(id, true).unwrap();
        assert!(!path.exists());
        assert!(!lock.access().unwrap().grants.contains_key(&id));
    }
}
