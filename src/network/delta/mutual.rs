//! Reconstruct both authenticated snapshots from a bounded union of plan IDs.
//! Revisions only choose candidates: each direction must still verify the full digest.
use super::*;

pub(super) fn changed_ids(calendar: &Calendar, since: u64) -> Option<BTreeSet<Id>> {
    let mut ids = BTreeSet::new();
    for (id, plan) in &calendar.plans {
        if plan.latest() > since {
            if ids.len() == PATCH_PLANS {
                return None;
            }
            ids.insert(*id);
        }
    }
    Some(ids)
}

// An empty set is a valid proposal after an imported low-revision change.
// A separate marker declines the proposal and retains the existing full exchange.
pub(super) fn send_ids(
    io: &mut impl Write,
    noise: &mut TransportState,
    ids: Option<&BTreeSet<Id>>,
) -> Fallible<()> {
    let mut plain = [0u8; 1 + 16 * PATCH_PLANS];
    let mut len = 1;
    if let Some(ids) = ids {
        if ids.len() > PATCH_PLANS {
            return Err(INVALID_DELTA.into());
        }
        plain[0] = 1;
        for id in ids {
            plain[len..len + 16].copy_from_slice(id.as_bytes());
            len += 16;
        }
    }
    let mut wire = [0; 1 + 16 * PATCH_PLANS + 18];
    send_encrypted(io, noise, &plain[..len], &mut wire)
}
pub(super) fn read_ids(
    io: &mut impl Read,
    noise: &mut TransportState,
) -> Fallible<Option<BTreeSet<Id>>> {
    let mut wire = [0; 1 + 16 * PATCH_PLANS + 16];
    let frame = read_frame_into(io, &mut wire)?;
    let mut plain = [0; 1 + 16 * PATCH_PLANS];
    let len = noise.read_message(frame, &mut plain)?;
    decode_ids(&plain[..len])
}

fn decode_ids(plain: &[u8]) -> Fallible<Option<BTreeSet<Id>>> {
    match plain {
        [0] => Ok(None),
        [1, ids @ ..] if ids.len() <= PATCH_PLANS * 16 && ids.len() % 16 == 0 => {
            let mut result = BTreeSet::new();
            for chunk in ids.chunks_exact(16) {
                if !result.insert(Id::from_slice(chunk)?) {
                    return Err(INVALID_DELTA.into());
                }
            }
            Ok(Some(result))
        }
        _ => Err(INVALID_DELTA.into()),
    }
}

pub(super) fn outgoing_direction(
    io: &mut (impl Read + Write),
    noise: &mut TransportState,
    calendar: &Calendar,
    confirmed: Option<Baseline>,
    mutual: bool,
) -> Fallible<(bool, Option<BTreeSet<Id>>)> {
    match read_control::<1>(io, noise)? {
        [0] => Ok((true, None)),
        [1] => Ok((false, None)),
        [2] if mutual => {
            let digest = read_control::<32>(io, noise)?;
            let mut ids = read_ids(io, noise)?.ok_or(INVALID_DELTA)?;
            let selected = confirmed
                .filter(|e| e.digest == digest)
                .and_then(|e| changed_ids(calendar, e.revision))
                .and_then(|local| {
                    ids.extend(local);
                    (ids.len() <= PATCH_PLANS).then_some(ids)
                });
            send_ids(io, noise, selected.as_ref())?;
            Ok((true, selected))
        }
        _ => Err(INVALID_DELTA.into()),
    }
}

pub(super) fn incoming_direction(
    io: &mut (impl Read + Write),
    noise: &mut TransportState,
    calendar: &Calendar,
    local: [u8; 32],
    remote: [u8; 32],
    confirmed: Option<Baseline>,
    mutual: bool,
) -> Fallible<(bool, Option<BTreeSet<Id>>)> {
    if let Some(entry) = confirmed {
        if entry.digest == remote {
            send_control(io, noise, &[1])?;
            return Ok((true, None));
        }
        if mutual
            && entry.digest != local
            && let Some(own) = changed_ids(calendar, entry.revision)
        {
            send_control(io, noise, &[2])?;
            send_control(io, noise, &entry.digest)?;
            send_ids(io, noise, Some(&own))?;
            let asked = read_ids(io, noise)?;
            if asked.as_ref().is_some_and(|ids| !own.is_subset(ids)) {
                return Err(INVALID_DELTA.into());
            }
            return Ok((false, asked));
        }
    }
    send_control(io, noise, &[0])?;
    Ok((false, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_distinct_bounded_and_distinguish_empty_from_declined() {
        assert_eq!(decode_ids(&[0]).unwrap(), None);
        assert_eq!(decode_ids(&[1]).unwrap(), Some(BTreeSet::new()));
        let mut input = vec![1];
        for index in 0..PATCH_PLANS {
            input.extend_from_slice(Id::from_u128(index as u128).as_bytes());
        }
        assert_eq!(decode_ids(&input).unwrap().unwrap().len(), PATCH_PLANS);
        input.extend_from_slice(Id::from_u128(1000).as_bytes());
        assert!(decode_ids(&input).is_err());
        let id = Id::from_u128(42);
        let mut duplicate = vec![1];
        duplicate.extend_from_slice(id.as_bytes());
        duplicate.extend_from_slice(id.as_bytes());
        assert!(decode_ids(&duplicate).is_err());
        for invalid in [&[][..], &[2], &[0, 1], &[1, 0]] {
            assert!(decode_ids(invalid).is_err());
        }
    }
}
