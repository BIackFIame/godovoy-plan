//! Optional transfer of changed plans between members of a LAN workspace.
//! The receiver reconstructs and verifies the canonical full snapshot before merge.
//! The revision threshold is only a bandwidth hint; old imported revisions force a full transfer.
use super::*;
use std::collections::VecDeque;
const PATCH_PLAIN: usize = 256 * 1024;
const PATCH_WIRE: usize = 64 * 1024;
const PATCH_PLANS: usize = 64;
const INVALID_DELTA: &str = "Не удалось проверить изменения календаря.";

#[derive(Clone, Copy, PartialEq)]
struct PeerScope {
    workspace: Id,
    generation: u64,
    device: Id,
}

impl PeerScope {
    fn new(workspace: &Workspace, device: Id) -> Self {
        Self {
            workspace: workspace.id,
            generation: workspace.key_generation,
            device,
        }
    }
}

struct Baseline {
    scope: PeerScope,
    digest: [u8; 32],
    revision: u64,
}

#[derive(Default)]
pub(super) struct Baselines {
    entries: Mutex<VecDeque<Baseline>>,
}

impl Baselines {
    #[cfg(test)]
    pub(super) fn clear_for_test(&self) {
        self.entries.lock().unwrap().clear();
    }
    fn known(&self, scope: PeerScope, digest: [u8; 32]) -> Option<u64> {
        self.entries
            .lock()
            .ok()?
            .iter()
            .find(|entry| entry.scope == scope && entry.digest == digest)
            .map(|entry| entry.revision)
    }

    fn confirm(&self, workspace: &Workspace, device: Id, digest: [u8; 32]) {
        let scope = PeerScope::new(workspace, device);
        self.confirm_with(scope, digest, || workspace.calendar.latest());
    }

    fn confirm_with(&self, scope: PeerScope, digest: [u8; 32], revision: impl FnOnce() -> u64) {
        if self.known(scope, digest).is_some() {
            return;
        }
        // Hashes and revision scans use immutable snapshots outside the cache lock.
        // Racing confirmations can only cause the next exchange to use a full snapshot.
        let revision = revision();
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|entry| entry.scope != scope);
            if entries.len() == MAX_PEERS {
                entries.pop_front();
            }
            entries.push_back(Baseline {
                scope,
                digest,
                revision,
            });
        }
    }
}

#[derive(Serialize)]
struct ChangedPlans<'a> {
    members: &'a BTreeMap<Id, Record<String>>,
    plans: BTreeMap<Id, &'a crate::model::Plan>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    device_people: &'a BTreeMap<Id, Record<Id>>,
}

fn candidate(calendar: &Calendar, since: u64) -> Option<Vec<u8>> {
    let mut changes = ChangedPlans {
        members: &calendar.members,
        device_people: &calendar.device_people,
        plans: BTreeMap::new(),
    };
    for (id, plan) in &calendar.plans {
        if plan.latest() > since {
            if changes.plans.len() == PATCH_PLANS {
                return None;
            }
            changes.plans.insert(*id, plan);
        }
    }
    let mut plain = Vec::new();
    serde_json::to_writer(LimitedWriter::new(&mut plain, PATCH_PLAIN), &changes).ok()?;
    Some(plain)
}

fn send_snapshot(
    io: &mut (impl Read + Write),
    noise: &mut TransportState,
    calendar: &Calendar,
    since: Option<u64>,
) -> Fallible<()> {
    if let Some(plain) = since.and_then(|n| candidate(calendar, n)) {
        let mut bytes = Vec::new();
        let mut compressor = ZlibEncoder::new(
            LimitedWriter::new(&mut bytes, PATCH_WIRE),
            Compression::fast(),
        );
        if compressor.write_all(&plain).is_ok() && compressor.finish().is_ok() {
            send_control(io, noise, &[1])?;
            send_control(io, noise, &Blake2s256::digest(&plain))?;
            send_bytes(io, noise, &bytes)?;
            match read_control::<1>(io, noise)? {
                [1] => return Ok(()),
                [0] => return send_calendar(io, noise, calendar),
                _ => return Err(INVALID_DELTA.into()),
            }
        }
    }
    send_control(io, noise, &[0])?;
    send_calendar(io, noise, calendar)
}

fn read_snapshot(
    io: &mut (impl Read + Write),
    noise: &mut TransportState,
    base: &Calendar,
    remote: SyncHello,
) -> Fallible<Calendar> {
    match read_control::<1>(io, noise)? {
        [0] => return read_calendar(io, noise, remote.digest),
        [1] => {}
        _ => return Err(INVALID_DELTA.into()),
    }
    let tag = read_control::<32>(io, noise)?;
    let length = message_length(io, noise)?;
    if length > PATCH_WIRE {
        return Err(INVALID_DELTA.into());
    }
    let partial = decode_calendar_with_limit(
        CalendarFrames::new(io, noise, length),
        length,
        tag,
        PATCH_PLAIN,
    )?;
    if partial.plans.len() > PATCH_PLANS {
        return Err(INVALID_DELTA.into());
    }
    validate_incoming_calendar(&partial, remote.device)?;
    let new = partial
        .plans
        .keys()
        .filter(|id| !base.plans.contains_key(id))
        .count();
    if base.plans.len() + new <= MAX_PLANS {
        let mut restored = Calendar {
            plans: base.plans.clone(),
            members: partial.members,
            device_people: partial.device_people,
        };
        restored.plans.extend(partial.plans);
        if calendar_digest(&restored).is_ok_and(|d| d == remote.digest) {
            send_control(io, noise, &[1])?;
            return Ok(restored);
        }
    }
    send_control(io, noise, &[0])?;
    read_calendar(io, noise, remote.digest)
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Role {
    Initiator,
    Responder { edit_wake: bool },
}

pub(super) fn exchange(
    core: &Core,
    io: &mut (impl Read + Write),
    noise: &mut TransportState,
    state: &Arc<LocalState>,
    ip: Ipv4Addr,
    role: Role,
) -> Fallible<bool> {
    let initiator = role == Role::Initiator;
    let local = SyncHello::new(core, state)?;
    let ws = workspace(state)?;
    if initiator {
        local.send(io, noise)?;
    }
    let remote = SyncHello::read(io, noise, local.device)?;
    ws.require_device_access(remote.device)?;
    if !ws.calendar.members.contains_key(&remote.device) {
        return Err(INVALID_DELTA.into());
    }
    let peer = peer_address(ip, remote.port)?;
    if !initiator {
        local.send(io, noise)?;
    }
    if remote.digest == local.digest {
        if initiator {
            if Receipt::read(io, noise)? != Receipt::Current {
                return Err(INVALID_DELTA.into());
            }
        } else {
            Receipt::Current.send(io, noise)?;
        }
        core.delta_baselines
            .confirm(ws, remote.device, local.digest);
    } else {
        let since = core
            .delta_baselines
            .known(PeerScope::new(ws, remote.device), remote.digest);
        let sender = if initiator {
            match read_control::<1>(io, noise)? {
                [0] => true,
                [1] => false,
                _ => return Err(INVALID_DELTA.into()),
            }
        } else {
            send_control(io, noise, &[since.is_some() as u8])?;
            since.is_some()
        };
        let merge = |calendar| {
            core.merge(SyncPacket {
                device: remote.device,
                workspace: ws.id,
                port: remote.port,
                calendar,
            })
        };
        if sender {
            send_snapshot(io, noise, &ws.calendar, since)?;
            match Receipt::read(io, noise)? {
                Receipt::Current => core
                    .delta_baselines
                    .confirm(ws, remote.device, local.digest),
                Receipt::Rejected => return finish_remote_rejection(core, peer),
                Receipt::SnapshotFollows => {
                    accept_calendar(core, io, noise, remote, ws.id, peer)?;
                    Receipt::Current.send(io, noise)?;
                }
            }
        } else {
            let read = read_snapshot(io, noise, &ws.calendar, remote).and_then(merge);
            if let Err(err) = read {
                core.record_merge_error(peer, &err.to_string());
                let _ = Receipt::Rejected.send(io, noise);
                return Err(err);
            }
            let merged = core.snapshot();
            if core.calendar_digest(&merged)? == remote.digest {
                Receipt::Current.send(io, noise)?;
                core.delta_baselines
                    .confirm(workspace(&merged)?, remote.device, remote.digest);
            } else {
                Receipt::SnapshotFollows.send(io, noise)?;
                send_calendar(io, noise, &ws.calendar)?;
                if Receipt::read(io, noise)? != Receipt::Current {
                    return Err(INVALID_DELTA.into());
                }
            }
        }
    }
    if initiator {
        core.exchanged(remote.device, peer);
    } else {
        let grace = if matches!(role, Role::Responder { edit_wake: true })
            && remote.digest == local.digest
        {
            QUIET_INBOUND
        } else {
            RECENT_INBOUND
        };
        core.inbound_exchange(local.device, remote.device, peer, grace);
    }
    Ok(remote.digest == local.digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_selection_limits_plan_count_and_uncompressed_size() {
        use crate::model::{Plan, Revision};
        let revision = Revision {
            sequence: 1,
            operation: Id::from_u128(1),
        };
        let mut plan = Plan::default();
        plan.years.entry(2026).or_default().note = Some(Record::new(revision, "Заметка".into()));
        let mut calendar = Calendar::default();
        for i in 0..PATCH_PLANS {
            calendar
                .plans
                .insert(Id::from_u128(i as u128), plan.clone());
        }
        assert!(candidate(&calendar, 0).is_some());
        calendar
            .plans
            .insert(Id::from_u128(PATCH_PLANS as u128), plan.clone());
        assert!(candidate(&calendar, 0).is_none());
        calendar.plans.clear();
        for year in 2026..2106 {
            plan.years.entry(year).or_default().note =
                Some(Record::new(revision, "я".repeat(4000)));
        }
        calendar.plans.insert(Id::from_u128(1), plan);
        calendar.validate().unwrap();
        assert!(candidate(&calendar, 0).is_none());
        let bytes = encode_calendar(&calendar).unwrap();
        assert!(bytes.len() < PATCH_WIRE);
        let digest = calendar_digest(&calendar).unwrap();
        assert!(
            decode_calendar_with_limit(bytes.as_slice(), bytes.len(), digest, PATCH_PLAIN).is_err()
        );
    }

    #[test]
    fn confirmed_digest_skips_revision_scan_and_cache_is_unlocked_during_scan() {
        let scope = PeerScope {
            workspace: Id::new_v4(),
            generation: 0,
            device: Id::new_v4(),
        };
        let cache = Baselines::default();
        cache.confirm_with(scope, [1; 32], || {
            assert!(cache.entries.try_lock().is_ok());
            7
        });
        cache.confirm_with(scope, [1; 32], || panic!("identical digest rescanned"));
        assert_eq!(cache.known(scope, [1; 32]), Some(7));
    }

    #[test]
    fn peer_cache_evicts_old_entries_and_scopes_each_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = Store::open(&tmp.path().join("team")).unwrap();
        let mut state = profile.lock().unwrap().snapshot().as_ref().clone();
        state
            .create_workspace("Тест".into(), "Анна".into())
            .unwrap();
        let ws = state.workspace_mut().unwrap();
        let cache = Baselines::default();
        for i in 1..=MAX_PEERS + 1 {
            cache.confirm(ws, Id::from_u128(i as u128), [9; 32]);
        }
        assert_eq!(cache.entries.lock().unwrap().len(), MAX_PEERS);
        let hello = |n| SyncHello {
            device: Id::from_u128(n),
            port: 4820,
            digest: [9; 32],
        };
        assert_eq!(
            cache.known(PeerScope::new(ws, hello(1).device), hello(1).digest),
            None
        );
        assert!(
            cache
                .known(PeerScope::new(ws, hello(2).device), hello(2).digest)
                .is_some()
        );
        ws.key_generation += 1;
        assert_eq!(
            cache.known(PeerScope::new(ws, hello(2).device), hello(2).digest),
            None
        );
        cache.confirm(ws, hello(2).device, [7; 32]);
        assert_eq!(
            cache.known(PeerScope::new(ws, hello(2).device), hello(2).digest),
            None
        );
    }
}
