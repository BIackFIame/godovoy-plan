//! Keep each group's key generation in a separate, locked office profile.
use super::*;
use blake2::{
    Blake2sMac256,
    digest::{KeyInit, Mac},
};
use std::collections::HashSet;
use subtle::ConstantTimeEq;

const SUCCESSOR: &str = "active-successor-v1.json";
const GENERATIONS: &str = "access-profiles-v1";

// Bound enrollment metadata independently of the calendar snapshot. A proof for a
// retired generation permits recognizing its descendants without retaining its key.
const MAX_KEY_PROOFS: usize = 128;

impl Workspace {
    pub fn is_revoked(&self, device: Id) -> bool {
        self.revoked_devices.contains(&device)
    }

    pub(crate) fn require_device_access(&self, device: Id) -> Fallible<()> {
        if self.is_revoked(device) {
            return Err("Доступ этого устройства отозван.".into());
        }
        Ok(())
    }

    pub(crate) fn require_manager(&self, device: Id) -> Fallible<()> {
        if let Some(manager) = self.access_manager
            && manager != device
        {
            return Err(format!("Доступом управляет другое устройство: {manager}.").into());
        }
        Ok(())
    }

    pub(crate) fn valid_key_history(&self) -> bool {
        self.previous_key_proofs.len() as u64 == self.key_generation.min(MAX_KEY_PROOFS as u64)
            && (self.key_generation != 0 || self.revoked_devices.is_empty())
            && self.revoked_devices.len() <= 256
            && (self.access_manager.is_none() || self.key_generation > 0)
            && (self.revoked_devices.is_empty() || self.access_manager.is_some())
            && self
                .access_manager
                .is_none_or(|id| self.calendar.members.contains_key(&id) && !self.is_revoked(id))
    }

    fn renewal_proof(&self) -> [u8; 32] {
        let mut mac = <Blake2sMac256 as KeyInit>::new((&self.key).into());
        Mac::update(&mut mac, b"GodovoyPlan/WorkspaceRenewal/v1");
        Mac::update(&mut mac, self.id.as_bytes());
        Mac::update(&mut mac, &self.key_generation.to_be_bytes());
        mac.finalize().into_bytes().into()
    }

    fn rotate_key(&mut self) -> Fallible<()> {
        let generation = self
            .key_generation
            .checked_add(1)
            .ok_or("Исчерпаны номера поколений ключа.")?;
        let key = random_bytes()?;
        let proof = self.renewal_proof();
        if self.previous_key_proofs.len() == MAX_KEY_PROOFS {
            self.previous_key_proofs.remove(0);
        }
        self.previous_key_proofs.push(proof);
        self.key_generation = generation;
        self.key = key;
        Ok(())
    }

    fn descends_from(&self, previous: &Workspace) -> bool {
        if self.id != previous.id || self.key == previous.key || !self.valid_key_history() {
            return false;
        }
        self.key_generation
            .checked_sub(previous.key_generation)
            .and_then(|gap| usize::try_from(gap).ok())
            .and_then(|gap| self.previous_key_proofs.len().checked_sub(gap))
            .and_then(|index| self.previous_key_proofs.get(index))
            .is_some_and(|proof| bool::from(proof.ct_eq(&previous.renewal_proof())))
    }
}

fn generations(directory: &Path) -> PathBuf {
    if directory
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| Id::parse_str(n).is_ok())
        && let Some(parent) = directory.parent()
        && parent.file_name().is_some_and(|n| n == GENERATIONS)
    {
        parent.to_owned()
    } else {
        directory.join(GENERATIONS)
    }
}

fn successor_id(directory: &Path) -> Fallible<Option<Id>> {
    let path = directory.join(SUCCESSOR);
    check_profile_file(&path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(65).read_to_end(&mut bytes)?;
    if bytes.len() > 64 {
        return Err("Указатель активного профиля повреждён.".into());
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub(super) fn ensure_no_successor(directory: &Path) -> Fallible<()> {
    if successor_id(directory)?.is_some() {
        return Err("Доступ уже перевыпущен. Откройте активный профиль.".into());
    }
    Ok(())
}

impl Store {
    pub fn active_directory(initial: &Path) -> Fallible<PathBuf> {
        let mut directory = initial.to_owned();
        let mut seen = HashSet::new();
        loop {
            match fs::symlink_metadata(directory.join(SUCCESSOR)) {
                Err(e) if e.kind() == io::ErrorKind::NotFound && seen.is_empty() => {
                    return Ok(directory);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    check_private_directory(&directory)?;
                    return Ok(directory);
                }
                Err(e) => return Err(e.into()),
                Ok(_) => {}
            }
            validate_profile_access(&directory)?;
            let id = successor_id(&directory)?.ok_or("Указатель активного профиля исчез.")?;
            if !seen.insert(id) {
                return Err("Повреждена цепочка активных профилей.".into());
            }
            let parent = generations(&directory);
            validate_profile_access(&parent)?;
            directory = parent.join(id.to_string());
        }
    }

    pub fn successor_directory(&self) -> Option<PathBuf> {
        self.successor.clone()
    }

    pub(crate) fn ensure_current_generation(&self) -> Fallible<()> {
        if self.successor.is_some() {
            return Err("Доступ перевыпущен. Изменения принимаются в активном профиле.".into());
        }
        Ok(())
    }

    pub(crate) fn check_revocations(&self, devices: &BTreeSet<Id>) -> Fallible<()> {
        self.ensure_current_generation()?;
        let workspace = self
            .state
            .workspace
            .as_ref()
            .ok_or("Календарь недоступен.")?;
        workspace.require_manager(self.state.device)?;
        if devices.contains(&self.state.device) {
            return Err("Нельзя отозвать доступ текущего устройства.".into());
        }
        for id in devices {
            workspace.require_device_access(*id)?;
            if !workspace.calendar.members.contains_key(id) {
                return Err("Устройство отсутствует в этом календаре.".into());
            }
        }
        if workspace.revoked_devices.len() + devices.len() > 256 {
            return Err("Достигнут предел отозванных устройств.".into());
        }
        Ok(())
    }

    pub(crate) fn renew_workspace_key(&mut self, devices: &BTreeSet<Id>) -> Fallible<()> {
        self.check_revocations(devices)?;
        let mut next = (*self.state).clone();
        let manager = next.device;
        let workspace = next.workspace_mut()?;
        workspace.access_manager = Some(manager);
        workspace.rotate_key()?;
        workspace.revoked_devices.extend(devices.iter().copied());
        next.enrollment_grants.retain(|id, _| !devices.contains(id));
        next.peers.clear();
        self.install_generation(next)
    }

    pub(crate) fn accept_renewal(
        &mut self,
        mut workspace: Workspace,
        peer: SocketAddrV4,
    ) -> Fallible<()> {
        let mut next = (*self.state).clone();
        let previous = next
            .workspace
            .as_ref()
            .ok_or("Сначала подключитесь к пространству.")?;
        workspace.require_device_access(next.device)?;
        if !workspace.descends_from(previous)
            || !previous
                .revoked_devices
                .is_subset(&workspace.revoked_devices)
            || previous
                .access_manager
                .is_some_and(|id| workspace.access_manager != Some(id))
        {
            return Err("Нужен новый код после перевыпуска доступа к этому календарю.".into());
        }
        workspace.calendar.merge(previous.calendar.clone());
        workspace.calendar.validate()?;
        next.workspace = Some(Arc::new(workspace));
        next.peers.clear();
        next.peers.insert(peer);
        self.install_generation(next)
    }

    fn install_generation(&mut self, mut next: LocalState) -> Fallible<()> {
        self.ensure_current_generation()?;
        next.sync_paused = false;
        next.last_sync = None;
        next.admitted_this_generation.clear();
        next.api = None;
        let directory = self
            .path
            .parent()
            .ok_or("Не указана папка профиля.")?
            .to_owned();
        let parent = generations(&directory);
        backup::create_folder(&parent)?;
        validate_profile_access(&parent)?;
        let id = Id::new_v4();
        let target = parent.join(id.to_string());
        let mut builder = tempfile::Builder::new();
        builder.prefix(".prepare-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let staging = builder.tempdir_in(&parent)?;
        validate_profile_access(staging.path())?;
        mark_team_initialized(&staging.path().join(TEAM_MARKER))?;
        save_profile(&staging.path().join("calendar.json"), &next)?;
        if let Some(drafts) = self.load_ui_drafts::<serde_json::Value>()? {
            save(
                &staging.path().join("drafts-v1.json"),
                &drafts,
                UI_DRAFT_LIMIT,
            )?;
        }
        save(
            &staging.path().join("reminders-v1.json"),
            &*self.reminder_receipts(),
            REMINDER_LIMIT,
        )?;
        validate_profile_access(staging.path())?;
        sync_directory(staging.path())?;
        fs::rename(staging.path(), &target)?;
        sync_directory(&parent)?;
        // Pause even an older binary that subsequently opens the retired copy.
        self.set_sync_paused(true)?;
        let marker = directory.join(SUCCESSOR);
        let (file, _) = stage(&marker, &id, 64)?;
        file.persist_noclobber(marker)?;
        self.successor = Some(target);
        sync_directory(&directory)?;
        Ok(())
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skipped_keys_are_accepted_only_within_the_same_lineage() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("profile")).unwrap();
        let mut state = (*store.lock().unwrap().snapshot()).clone();
        state
            .create_workspace("Офис".into(), "Анна".into())
            .unwrap();
        let initial = Arc::unwrap_or_clone(state.workspace.unwrap());
        let mut newer = initial.clone();
        let mut fork = initial.clone();
        newer.rotate_key().unwrap();
        fork.rotate_key().unwrap();
        let first = newer.clone();
        newer.rotate_key().unwrap();
        fork.rotate_key().unwrap();
        fork.rotate_key().unwrap();
        assert!(newer.descends_from(&initial));
        assert!(newer.descends_from(&first));
        assert!(!fork.descends_from(&first));
        assert!(!fork.descends_from(&newer));
        assert!(!first.descends_from(&first));
        assert!(!first.descends_from(&newer));
        for _ in 0..MAX_KEY_PROOFS {
            newer.rotate_key().unwrap();
        }
        assert!(newer.valid_key_history());
        assert!(!newer.descends_from(&initial));
    }
}
