//! Monthly host audit. The caller holds the vault lock through the operation.
use super::{Id, LockedVault, ProtectionState, sync_directory};
use crate::Fallible;
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use std::{
    fs::OpenOptions,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
};

#[derive(Serialize)]
struct SecurityRecord {
    at: String,
    action: &'static str,
    credential: Option<Id>,
    employee: Option<Id>,
    object: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protection: Option<ProtectionState>,
}

impl LockedVault<'_> {
    pub fn audit_security(
        &self,
        action: &'static str,
        credential: Option<Id>,
        employee: Option<Id>,
        object: Option<Id>,
    ) -> Fallible<()> {
        self.append_security(action, credential, employee, object, None)
    }
    pub(super) fn audit_protection(
        &self,
        action: &'static str,
        change: Id,
        policy: ProtectionState,
    ) -> Fallible<()> {
        self.append_security(action, None, None, Some(change), Some(policy))
    }
    fn append_security(
        &self,
        action: &'static str,
        credential: Option<Id>,
        employee: Option<Id>,
        object: Option<Id>,
        protection: Option<ProtectionState>,
    ) -> Fallible<()> {
        let now = Utc::now();
        let path = self
            .directory
            .join(format!("security-{}.jsonl", now.format("%Y-%m")));
        let mut options = OpenOptions::new();
        options.read(true).append(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        let (mut file, created) = match options.open(&path) {
            Ok(file) => (file, true),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                (options.create_new(false).open(&path)?, false)
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err("Журнал должен быть обычным файлом.".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
                || metadata.uid() != std::fs::metadata(self.directory)?.uid()
            {
                return Err("Небезопасные права или ссылки журнала.".into());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err("Журнал содержит точку повторной обработки.".into());
            }
        }
        if metadata.len() > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0u8];
            file.read_exact(&mut last)?;
            if last != [b'\n'] {
                return Err(
                    "Незавершённая запись журнала. Требуется проверка администратором.".into(),
                );
            }
        }
        let mut entry = serde_json::to_vec(&SecurityRecord {
            at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            action,
            credential,
            employee,
            object,
            protection,
        })?;
        entry.push(b'\n');
        file.write_all(&entry)?;
        file.sync_data()?;
        if created {
            sync_directory(self.directory)?;
        }
        Ok(())
    }
}
