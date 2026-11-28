//! Optional transfer of changed plans between members of a LAN workspace.
//! The receiver reconstructs and verifies the canonical full snapshot before merge.
//! The revision threshold is only a bandwidth hint; old imported revisions force a full transfer.
use super::*;
use super::wire::{
    CalendarFrames, calendar_digest_with_limit, decode_calendar_with_limit,
    message_length, read_frame_into, send_bytes, send_encrypted,
};
use blake2::{Blake2s256, Digest};
use flate2::{Compression, write::ZlibEncoder};
mod hint;
mod mutual;
use std::collections::{BTreeSet, VecDeque};
const PATCH_PLAIN: usize = 256 * 1024;
const PATCH_WIRE: usize = 64 * 1024;
const PATCH_PLANS: usize = 64;
const INVALID_DELTA: &str = "Не удалось проверить изменения календаря.";

#[derive(Clone, Copy, PartialEq)]
pub(super) struct PeerScope {
    workspace: Id,
    generation: u64,
    device: Id,
}

impl PeerScope {
    pub(super) fn check(&self, state: &LocalState) -> Fallible<()> {
        let current = workspace(state)?;
        if current.id != self.workspace || current.key_generation != self.generation {
            return Err("Ключ рабочего пространства изменился.".into());
        }
        for device in [state.device, self.device] {
            current.require_device_access(device)?;
            if !current.calendar.members.contains_key(&device) {
                return Err(INVALID_DELTA.into());
            }
        }
        Ok(())
    }

    pub(super) fn new(workspace: &Workspace, device: Id) -> Self {
        Self {
            workspace: workspace.id,
            generation: workspace.key_generation,
            device,
        }
    }
}

#[derive(Clone, Copy)]
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
    fn latest_confirmation(&self, scope: PeerScope) -> Option<Baseline> {
        self.entries
            .lock()
            .ok()?
            .iter()
            .find(|entry| entry.scope == scope)
            .copied()
    }
    #[cfg(test)]
    fn known(&self, scope: PeerScope, digest: [u8; 32]) -> Option<u64> {
        self.latest_confirmation(scope)
            .filter(|entry| entry.digest == digest)
            .map(|entry| entry.revision)
    }

    fn confirm(&self, workspace: &Workspace, device: Id, digest: [u8; 32]) {
        let scope = PeerScope::new(workspace, device);
        self.confirm_with(scope, digest, || workspace.calendar.latest());
    }

    fn confirm_with(&self, scope: PeerScope, digest: [u8; 32], revision: impl FnOnce() -> u64) {
        // A canonical digest identifies the same revision for every recipient.
        // Only the peer that confirmed it receives a new baseline entry below.
        let mut confirmed_revision = None;
        if let Ok(entries) = self.entries.lock() {
            for entry in entries.iter().rev() {
                if entry.digest == digest
                    && entry.scope.workspace == scope.workspace
                    && entry.scope.generation == scope.generation
                {
                    if entry.scope.device == scope.device {
                        return;
                    }
                    confirmed_revision = Some(entry.revision);
                }
            }
        }
        // Scan immutable data outside the lock on the first confirmation.
        let revision = confirmed_revision.unwrap_or_else(revision);
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
struct ChangedPlans<'a, P> {
    members: &'a BTreeMap<Id, Record<String>>,
    plans: P,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    device_people: &'a BTreeMap<Id, Record<Id>>,
}

fn candidate(calendar: &Calendar, since: u64) -> Option<Vec<u8>> {
    let mut newer = calendar
        .plans
        .iter()
        .filter(|(_, plan)| plan.latest() > since)
        .peekable();
    // Two devices may independently allocate the same sequence. After an office
    // relays such a merge, its last confirmed peer can differ only at that floor.
    // Keep the usual selection for normal edits and roster-only changes.
    if newer.peek().is_some()
        || calendar
            .members
            .values()
            .any(|r| r.revision.sequence > since)
        || calendar
            .device_people
            .values()
            .any(|r| r.revision.sequence > since)
    {
        return encode_plans(calendar, newer);
    }
    let tied = calendar
        .plans
        .iter()
        .filter(|(_, plan)| plan.latest() == since);
    encode_plans(calendar, tied).or_else(|| encode_plans(calendar, std::iter::empty()))
}

fn encode_plans<'a>(
    calendar: &'a Calendar,
    plans: impl Iterator<Item = (&'a Id, &'a crate::model::Plan)>,
) -> Option<Vec<u8>> {
    let mut changes = ChangedPlans {
        members: &calendar.members,
        device_people: &calendar.device_people,
        plans: BTreeMap::new(),
    };
    for (id, plan) in plans {
        if changes.plans.len() == PATCH_PLANS {
            return None;
        }
        changes.plans.insert(*id, plan);
    }
    let mut plain = Vec::new();
    serde_json::to_writer(LimitedWriter::new(&mut plain, PATCH_PLAIN), &changes).ok()?;
    Some(plain)
}

fn send_snapshot(
    io: &mut (impl Read + Write),
    noise: &mut TransportState,
    calendar: &Calendar,
    patch: Option<Vec<u8>>,
    permit: impl Fn() -> Fallible<()>,
) -> Fallible<()> {
    permit()?;
    if let Some(plain) = patch {
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
                [0] => {
                    permit()?;
                    return send_calendar(io, noise, calendar);
                }
                _ => return Err(INVALID_DELTA.into()),
            }
        }
    }
    send_control(io, noise, &[0])?;
    send_calendar(io, noise, calendar)
}

enum ReceivedSnapshot {
    Full(Calendar),
    OneWayPatch(Calendar, PatchFingerprint),
}

// The full overlay has already matched the remote canonical digest. Its patch
// checksum (verified while decoding) covers the complete roster and aliases and
// every field in every replacement plan. No other plans participate in this merge.
struct PatchFingerprint {
    image_digest: [u8; 32],
    plans: Vec<Id>,
    checksum: [u8; 32],
}

impl PatchFingerprint {
    fn matches(&self, merged: &Calendar) -> bool {
        let Calendar {
            members,
            plans,
            device_people,
        } = merged;
        let Some(plans) = self
            .plans
            .iter()
            .map(|id| plans.get(id).map(|plan| (*id, plan)))
            .collect::<Option<BTreeMap<_, _>>>()
        else {
            return false;
        };
        let subset = ChangedPlans {
            members,
            plans,
            device_people,
        };
        // A semantically valid, noncanonical writer can miss this optimization.
        // The caller then computes the full canonical digest as before.
        calendar_digest_with_limit(&subset, PATCH_PLAIN).is_ok_and(|digest| digest == self.checksum)
    }
}

struct OverlayPlans<'a> {
    base: &'a BTreeMap<Id, crate::model::Plan>,
    patch: &'a BTreeMap<Id, crate::model::Plan>,
}

impl Serialize for OverlayPlans<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        use std::ops::Bound::{Excluded, Unbounded};

        // Both maps are ordered by ID. Stream unchanged ranges between replacements.
        let mut map = serializer.serialize_map(None)?;
        let mut start = Unbounded;
        for (changed, plan) in self.patch {
            for (id, unchanged) in self.base.range((start, Excluded(*changed))) {
                map.serialize_entry(id, unchanged)?;
            }
            map.serialize_entry(changed, plan)?;
            start = Excluded(*changed);
        }
        for (id, unchanged) in self.base.range((start, Unbounded)) {
            map.serialize_entry(id, unchanged)?;
        }
        map.end()
    }
}

fn borrowed_overlay<'a>(base: &'a Calendar, partial: &'a Calendar) -> ChangedPlans<'a, OverlayPlans<'a>> {
    ChangedPlans {
        members: &partial.members,
        plans: OverlayPlans { base: &base.plans, patch: &partial.plans },
        device_people: &partial.device_people,
    }
}

pub(super) fn restore_plans(
    base: &Calendar,
    mut partial: Calendar,
    replace: Option<&BTreeSet<Id>>,
) -> Calendar {
    let mut plans = base.plans.clone();
    if let Some(ids) = replace {
        for id in ids {
            plans.remove(id);
        }
    }
    plans.append(&mut partial.plans);
    partial.plans = plans;
    partial
}

fn read_snapshot(
    io: &mut (impl Read + Write),
    noise: &mut TransportState,
    base: &Calendar,
    remote: SyncHello,
    replace: Option<&BTreeSet<Id>>,
) -> Fallible<ReceivedSnapshot> {
    match read_control::<1>(io, noise)? {
        [0] => return read_calendar(io, noise, remote.digest).map(ReceivedSnapshot::Full),
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
    if let Some(ids) = replace
        && !partial.plans.keys().all(|id| ids.contains(id))
    {
        return Err(INVALID_DELTA.into());
    }
    let removed = replace.map_or(0, |ids| {
        ids.iter()
            .filter(|id| base.plans.contains_key(*id) && !partial.plans.contains_key(*id))
            .count()
    });
    let new = partial
        .plans
        .keys()
        .filter(|id| !base.plans.contains_key(id))
        .count();
    if base.plans.len() - removed + new <= MAX_PLANS {
        if replace.is_none() {
            // Plans omitted by a one-way patch are exactly the valid base plans
            // once the complete canonical digest has been verified.
            let overlay = borrowed_overlay(base, &partial);
            if calendar_digest(&overlay).is_ok_and(|d| d == remote.digest) {
                send_control(io, noise, &[1])?;
                let fingerprint = PatchFingerprint {
                    image_digest: remote.digest,
                    plans: partial.plans.keys().copied().collect(),
                    checksum: tag,
                };
                return Ok(ReceivedSnapshot::OneWayPatch(partial, fingerprint));
            }
        } else {
            let restored = restore_plans(base, partial, replace);
            if calendar_digest(&restored).is_ok_and(|d| d == remote.digest) {
                send_control(io, noise, &[1])?;
                return Ok(ReceivedSnapshot::Full(restored));
            }
        }
    }
    send_control(io, noise, &[0])?;
    read_calendar(io, noise, remote.digest).map(ReceivedSnapshot::Full)
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
    capabilities: Capabilities,
) -> Fallible<bool> {
    let initiator = role == Role::Initiator;
    let generation = core.generation.load(Ordering::Acquire);
    let mut confirmed_inbound = None;
    let local = SyncHello::new(core, state)?;
    let ws = workspace(state)?;
    if initiator {
        local.send(io, noise)?;
    }
    let remote = SyncHello::read(io, noise, local.device)?;
    let scope = PeerScope::new(ws, remote.device);
    let permit = || {
        core.require_available()?;
        scope.check(&core.snapshot())
    };
    permit()?;
    let peer = peer_address(ip, remote.port)?;
    // A modern inbound transfer can carry changes in either direction.
    // Its final guard wakes the scheduler, including after a failed exchange.
    let _receiving = (!initiator
        && remote.digest != local.digest
        && capabilities.contains(Capabilities::REVISION_HINT))
    .then(|| core.receive_delta(peer));
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
        let confirmed = core
            .delta_baselines
            .latest_confirmation(PeerScope::new(ws, remote.device));
        let since = confirmed
            .filter(|entry| entry.digest == remote.digest)
            .map(|entry| entry.revision);
        let mutual = capabilities.contains(Capabilities::MUTUAL_DELTA);
        let (sender, union, hinted_since) =
            if mutual && capabilities.contains(Capabilities::REVISION_HINT) {
                let session = hint::Negotiation {
                    calendar: &ws.calendar,
                    confirmed,
                    local: local.digest,
                    remote: remote.digest,
                    initiator,
                };
                let route = session.negotiate(io, noise, permit)?;
                (route.sender, route.union, route.hinted_since)
            } else {
                let (sender, union) = if initiator {
                    mutual::outgoing_direction(io, noise, &ws.calendar, confirmed, mutual)?
                } else {
                    mutual::incoming_direction(
                        io,
                        noise,
                        &ws.calendar,
                        local.digest,
                        remote.digest,
                        confirmed,
                        mutual,
                    )?
                };
                (sender, union, None)
            };
        let selected_patch = || match &union {
            Some(ids) => encode_plans(
                &ws.calendar,
                ids.iter()
                    .filter_map(|id| ws.calendar.plans.get_key_value(id)),
            ),
            None => since
                .or(hinted_since)
                .and_then(|n| candidate(&ws.calendar, n)),
        };
        let merge = |snapshot| {
            let (calendar, fingerprint) = match snapshot {
                ReceivedSnapshot::Full(calendar) => (calendar, None),
                ReceivedSnapshot::OneWayPatch(calendar, fingerprint) => {
                    (calendar, Some(fingerprint))
                }
            };
            let committed = core.merge_received(
                SyncPacket {
                    device: remote.device,
                    workspace: ws.id,
                    port: remote.port,
                    calendar,
                },
                Some(scope),
                fingerprint.as_ref().map(|_| &ws.calendar),
            )?;
            Ok(match (committed, fingerprint) {
                (Some(ws), Some(check)) if check.matches(&ws.calendar) => Some(CachedDigest {
                    workspace: Arc::downgrade(&ws),
                    digest: check.image_digest,
                }),
                _ => None,
            })
        };
        if sender {
            send_snapshot(io, noise, &ws.calendar, selected_patch(), permit)?;
            match Receipt::read(io, noise)? {
                Receipt::Current => core
                    .delta_baselines
                    .confirm(ws, remote.device, local.digest),
                Receipt::Rejected => return finish_remote_rejection(core, peer),
                Receipt::SnapshotFollows => {
                    let read = if union.is_some() {
                        read_snapshot(io, noise, &ws.calendar, remote, union.as_ref())
                    } else {
                        read_calendar(io, noise, remote.digest).map(ReceivedSnapshot::Full)
                    }
                    .and_then(merge);
                    reject_failed_snapshot(core, io, noise, peer, read)?;
                    Receipt::Current.send(io, noise)?;
                }
            }
        } else {
            let read =
                read_snapshot(io, noise, &ws.calendar, remote, union.as_ref()).and_then(merge);
            let verified = reject_failed_snapshot(core, io, noise, peer, read)?;
            let merged = core.snapshot();
            let original_patch = verified.as_ref().is_some_and(|proof| {
                merged
                    .workspace
                    .as_ref()
                    .is_some_and(|live| std::ptr::eq(Arc::as_ptr(live), proof.workspace.as_ptr()))
            });
            if core.calendar_digest_with(&merged, verified)? == remote.digest {
                Receipt::Current.send(io, noise)?;
                core.delta_baselines
                    .confirm(workspace(&merged)?, remote.device, remote.digest);
                if original_patch
                    && matches!(role, Role::Responder { edit_wake: true })
                    && capabilities.contains(Capabilities::REVISION_HINT)
                    && let Some(workspace) = &merged.workspace
                {
                    confirmed_inbound = Some(ConfirmedInbound {
                        workspace: Arc::downgrade(workspace),
                        generation,
                    });
                }
            } else {
                Receipt::SnapshotFollows.send(io, noise)?;
                if union.is_some() {
                    send_snapshot(io, noise, &ws.calendar, selected_patch(), permit)?;
                } else {
                    permit()?;
                    send_calendar(io, noise, &ws.calendar)?;
                }
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
        core.inbound_exchange(local.device, remote.device, peer, grace, confirmed_inbound);
    }
    Ok(remote.digest == local.digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::wire::encode_calendar;

    #[derive(Default)]
    struct SnapshotStream {
        incoming: io::Cursor<Vec<u8>>,
        replies: Vec<u8>,
    }

    impl Read for SnapshotStream {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.incoming.read(bytes)
        }
    }

    impl Write for SnapshotStream {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.replies.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn receive_one_way(
        base: &Calendar,
        patch: &Calendar,
        remote_image: &Calendar,
        source: Id,
        with_fallback: bool,
    ) -> (Fallible<ReceivedSnapshot>, Option<[u8; 1]>) {
        let preamble = header(Operation::DigestSync, Id::from_u128(44));
        let mut sender = noise(&preamble, &[17; 32])
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut receiver = noise(&preamble, &[17; 32])
            .unwrap()
            .build_responder()
            .unwrap();
        let (mut wire, mut plain) = ([0; 256], [0; 256]);
        let first = sender.write_message(&[], &mut wire).unwrap();
        receiver.read_message(&wire[..first], &mut plain).unwrap();
        let second = receiver.write_message(&[], &mut wire).unwrap();
        sender.read_message(&wire[..second], &mut plain).unwrap();
        let mut sender = sender.into_transport_mode().unwrap();
        let mut receiver = receiver.into_transport_mode().unwrap();
        let mut input = Vec::new();
        send_control(&mut input, &mut sender, &[1]).unwrap();
        send_control(&mut input, &mut sender, &calendar_digest(patch).unwrap()).unwrap();
        send_calendar(&mut input, &mut sender, patch).unwrap();
        if with_fallback {
            send_calendar(&mut input, &mut sender, remote_image).unwrap();
        }
        let mut stream = SnapshotStream {
            incoming: io::Cursor::new(input),
            ..SnapshotStream::default()
        };
        let result = read_snapshot(
            &mut stream,
            &mut receiver,
            base,
            SyncHello {
                device: source,
                port: 47001,
                digest: calendar_digest(remote_image).unwrap(),
            },
            None,
        );
        let ack = (!stream.replies.is_empty())
            .then(|| read_control::<1>(&mut stream.replies.as_slice(), &mut sender).unwrap());
        (result, ack)
    }

    #[test]
    fn received_one_way_patch_uses_peer_roster_and_retains_omitted_plans() {
        use crate::model::{ExternalSource, Plan, Revision};

        let id = Id::from_u128;
        let revision = Revision {
            sequence: 1,
            operation: id(43),
        };
        let mut base = Calendar::default();
        base.members
            .insert(id(1), Record::new(revision, "Анна".into()));
        base.members
            .insert(id(2), Record::new(revision, "Борис".into()));
        base.device_people
            .insert(id(1), Record::new(revision, id(2)));
        base.plans.insert(
            id(11),
            Plan {
                source: Some(ExternalSource {
                    name: "Office".into(),
                    external_id: "annual".into(),
                }),
                ..Plan::default()
            },
        );
        let mut patch = Calendar::default();
        patch
            .members
            .insert(id(2), Record::new(revision, "Борис".into()));
        patch
            .members
            .insert(id(3), Record::new(revision, "Виктор".into()));
        patch
            .device_people
            .insert(id(3), Record::new(revision, id(2)));
        patch.plans.insert(id(12), Plan::default());
        let image = restore_plans(&base, patch.clone(), None);
        image.validate().unwrap();
        let (result, ack) = receive_one_way(&base, &patch, &image, id(2), false);
        assert_eq!(ack, Some([1]));
        let ReceivedSnapshot::OneWayPatch(verified, _) = result.unwrap() else {
            panic!("verified patch was reconstructed before merge");
        };
        let mut expected = base.clone();
        expected.merge(image);
        let mut applied = base.clone();
        applied.merge(verified);
        assert_eq!(applied, expected);
        assert_eq!(applied.plans[&id(11)], base.plans[&id(11)]);
        assert_eq!(applied.person_for(id(3)), id(2));
        applied.validate().unwrap();

        // A base member does not make an incomplete peer roster valid.
        patch
            .device_people
            .insert(id(3), Record::new(revision, id(1)));
        let image = restore_plans(&base, patch.clone(), None);
        let (result, ack) = receive_one_way(&base, &patch, &image, id(2), false);
        assert!(result.is_err());
        assert_eq!(ack, None);
    }

    #[test]
    fn received_one_way_patch_over_plan_limit_requires_full_fallback() {
        use crate::model::{Plan, Revision};
        let source = Id::from_u128(1);
        let revision = Revision {
            sequence: 1,
            operation: source,
        };
        let mut base = Calendar::default();
        base.members
            .insert(source, Record::new(revision, "Анна".into()));
        for index in 0..MAX_PLANS {
            base.plans
                .insert(Id::from_u128(index as u128), Plan::default());
        }
        let replacement = Id::from_u128(MAX_PLANS as u128);
        let mut patch = Calendar {
            members: base.members.clone(),
            ..Calendar::default()
        };
        patch.plans.insert(replacement, Plan::default());
        let mut image = restore_plans(&base, patch.clone(), None);
        image.plans.remove(&Id::nil());
        image.validate().unwrap();
        let (result, ack) = receive_one_way(&base, &patch, &image, source, true);
        assert_eq!(ack, Some([0]));
        let ReceivedSnapshot::Full(restored) = result.unwrap() else {
            panic!("oversized overlay accepted");
        };
        assert_eq!(restored, image);
    }

    #[test]
    fn overlay_streams_insertions_and_replacements_in_canonical_order() {
        let mut base = Calendar::default();
        for id in [1, 3, 5] {
            base.plans.insert(Id::from_u128(id), crate::model::Plan::default());
        }
        for ids in [&[][..], &[1, 3, 5], &[0, 2, 4, 6], &[0, 1, 4, 5, u128::MAX]] {
            let mut patch = Calendar::default();
            for &id in ids {
                let mut updated = crate::model::Plan::default();
                updated.years.entry(2028).or_default();
                patch.plans.insert(Id::from_u128(id), updated);
            }
            let expected = restore_plans(&base, patch.clone(), None);
            assert_eq!(
                serde_json::to_vec(&borrowed_overlay(&base, &patch)).unwrap(),
                serde_json::to_vec(&expected).unwrap()
            );
        }
    }

    #[test]
    fn verified_subset_rejects_local_winners_in_plans_roster_and_aliases() {
        use crate::model::{Plan, Revision};
        let id = Id::from_u128;
        let revision = Revision {
            sequence: 4,
            operation: id(1),
        };
        let mut base = Calendar::default();
        base.members
            .insert(id(1), Record::new(revision, "Анна".into()));
        base.members
            .insert(id(2), Record::new(revision, "Борис".into()));
        base.plans.insert(id(11), Plan::default());
        let mut patch = Calendar {
            members: base.members.clone(),
            ..Calendar::default()
        };
        patch
            .device_people
            .insert(id(2), Record::new(revision, id(1)));
        patch.plans.insert(id(12), Plan::default());
        let image = restore_plans(&base, patch.clone(), None);
        let (received, ack) = receive_one_way(&base, &patch, &image, id(2), false);
        assert_eq!(ack, Some([1]));
        let ReceivedSnapshot::OneWayPatch(_, fingerprint) = received.unwrap() else {
            panic!("full fallback");
        };
        assert!(fingerprint.matches(&image));
        // These local fields would all survive CRDT merge despite a valid peer image.
        for field in ["plan", "member", "alias"] {
            let mut local = base.clone();
            match field {
                "plan" => {
                    local
                        .plans
                        .entry(id(12))
                        .or_default()
                        .years
                        .entry(2027)
                        .or_default()
                        .note = Some(Record::new(revision, "Только в локальном календаре".into()))
                }
                "member" => {
                    local
                        .members
                        .insert(id(3), Record::new(revision, "Инна".into()));
                }
                _ => {
                    local
                        .device_people
                        .insert(id(1), Record::new(revision, id(2)));
                }
            }
            local.merge(patch.clone());
            assert_ne!(local, image);
            assert!(!fingerprint.matches(&local));
        }
    }

    #[test]
    fn one_way_overlay_matches_the_canonical_image_and_full_merge() {
        use crate::model::{Plan, Revision};

        let id = Id::from_u128;
        let stamp = |sequence| Revision {
            sequence,
            operation: id(42),
        };
        let plan = |revision, year, text: &str| {
            let mut plan = Plan::default();
            plan.years.entry(year).or_default().note =
                Some(Record::new(stamp(revision), text.to_owned()));
            plan
        };
        let mut base = Calendar::default();
        base.members
            .insert(id(1), Record::new(stamp(1), "Анна".to_owned()));
        base.members
            .insert(id(2), Record::new(stamp(2), "Мария".to_owned()));
        base.plans.insert(id(10), plan(3, 2026, "Нет изменений"));
        base.plans.insert(id(20), plan(5, 2026, "Локальная запись"));
        let mut patch = Calendar {
            members: base.members.clone(),
            ..Calendar::default()
        };
        patch
            .device_people
            .insert(id(2), Record::new(stamp(6), id(1)));
        // The remote payload can omit a local field in a changed plan.
        patch
            .plans
            .insert(id(20), plan(7, 2027, "Удалённая запись"));
        patch.plans.insert(id(15), plan(8, 2026, "Новое событие"));
        for aliases in [patch.device_people.clone(), BTreeMap::new()] {
            patch.device_people = aliases;
            let image = restore_plans(&base, patch.clone(), None);
            let overlay = borrowed_overlay(&base, &patch);
            assert_eq!(
                serde_json::to_vec(&image).unwrap(),
                serde_json::to_vec(&overlay).unwrap()
            );
            assert_eq!(
                calendar_digest(&image).unwrap(),
                calendar_digest(&overlay).unwrap()
            );
            let mut full = base.clone();
            full.merge(image);
            let mut direct = base.clone();
            direct.merge(patch.clone());
            assert_eq!(full, direct);
            direct.validate().unwrap();
        }
    }

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
    fn tied_sequences_are_bounded_and_only_considered_for_an_empty_selection() {
        use crate::model::{Plan, Revision};
        let stamp = |sequence| Revision {
            sequence,
            operation: Id::from_u128(7),
        };
        let mut calendar = Calendar::default();
        for n in 1..=(PATCH_PLANS + 1) {
            let mut plan = Plan::default();
            plan.years.entry(2026).or_default().note =
                Some(Record::new(stamp(7), "Сверка".to_owned()));
            calendar.plans.insert(Id::from_u128(n as u128), plan);
        }
        let selected = |book: &Calendar| {
            serde_json::from_slice::<Calendar>(&candidate(book, 7).unwrap())
                .unwrap()
                .plans
        };
        assert!(selected(&calendar).is_empty());
        calendar.plans.remove(&Id::from_u128(1));
        assert_eq!(selected(&calendar).len(), PATCH_PLANS);
        calendar
            .members
            .insert(Id::from_u128(12), Record::new(stamp(8), "Анна".to_owned()));
        assert!(selected(&calendar).is_empty());
        calendar.members.clear();
        calendar
            .device_people
            .insert(Id::from_u128(12), Record::new(stamp(8), Id::from_u128(2)));
        assert!(selected(&calendar).is_empty());
        calendar.device_people.clear();
        let id = Id::from_u128(2);
        calendar
            .plans
            .get_mut(&id)
            .unwrap()
            .years
            .get_mut(&2026)
            .unwrap()
            .note
            .as_mut()
            .unwrap()
            .revision = stamp(8);
        assert_eq!(
            selected(&calendar).keys().copied().collect::<Vec<_>>(),
            vec![id]
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
    fn revisions_are_shared_only_within_the_same_generation_after_confirmation() {
        let cache = Baselines::default();
        let first = PeerScope {
            workspace: Id::new_v4(),
            generation: 0,
            device: Id::new_v4(),
        };
        let second = PeerScope {
            device: Id::new_v4(),
            ..first
        };
        let digest = [9; 32];
        cache.confirm_with(first, digest, || 7);
        assert_eq!(cache.known(second, digest), None);
        cache.confirm_with(second, digest, || panic!("same digest was scanned again"));
        assert_eq!(cache.known(second, digest), Some(7));
        let next_generation = PeerScope {
            generation: 1,
            ..second
        };
        cache.confirm_with(next_generation, digest, || 11);
        assert_eq!(cache.known(next_generation, digest), Some(11));
        let other_workspace = PeerScope {
            workspace: Id::new_v4(),
            ..first
        };
        cache.confirm_with(other_workspace, digest, || 17);
        assert_eq!(cache.known(other_workspace, digest), Some(17));
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
