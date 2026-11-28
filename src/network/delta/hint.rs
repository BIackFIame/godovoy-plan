//! The surviving peer may offer its volatile revision as a search hint. A hint
//! never establishes a baseline: read_snapshot verifies the entire peer image.
use super::*;

const COLD_MIN_PLANS: usize = PATCH_PLANS * 2;
const COLD_REVISIONS: u64 = 32;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Offer {
    Default,
    HasRemote,
    Ask,
    Unchanged(u64),
    Changes(u64),
}
impl Offer {
    fn send(self, io: &mut impl Write, noise: &mut TransportState) -> Fallible<()> {
        let mut bytes = [0; 9];
        let (tag, revision) = match self {
            Self::Default => (0, None),
            Self::HasRemote => (1, None),
            Self::Ask => (2, None),
            Self::Unchanged(revision) => (3, Some(revision)),
            Self::Changes(revision) => (4, Some(revision)),
        };
        bytes[0] = tag;
        if let Some(revision) = revision {
            bytes[1..].copy_from_slice(&revision.to_be_bytes());
            send_control(io, noise, &bytes)
        } else {
            send_control(io, noise, &bytes[..1])
        }
    }

    fn decode(plain: &[u8]) -> Fallible<Self> {
        match plain {
            [0] => Ok(Self::Default),
            [1] => Ok(Self::HasRemote),
            [2] => Ok(Self::Ask),
            [tag @ (3 | 4), value @ ..] => {
                let revision = u64::from_be_bytes(value.try_into().map_err(|_| INVALID_DELTA)?);
                Ok(if *tag == 3 {
                    Self::Unchanged(revision)
                } else {
                    Self::Changes(revision)
                })
            }
            _ => Err(INVALID_DELTA.into()),
        }
    }
    fn read(io: &mut impl Read, noise: &mut TransportState) -> Fallible<Self> {
        let mut wire = [0; 9 + 16];
        let frame = read_frame_into(io, &mut wire)?;
        let mut plain = [0; 9];
        let len = noise.read_message(frame, &mut plain)?;
        Self::decode(&plain[..len])
    }
}

pub(super) struct Negotiation<'a> {
    pub(super) calendar: &'a Calendar,
    pub(super) confirmed: Option<Baseline>,
    pub(super) local: [u8; 32],
    pub(super) remote: [u8; 32],
    pub(super) initiator: bool,
}
pub(super) struct Route {
    pub(super) sender: bool,
    pub(super) union: Option<BTreeSet<Id>>,
    pub(super) hinted_since: Option<u64>,
}
impl Route {
    fn one_way(sender: bool, hinted_since: Option<u64>) -> Self {
        Self {
            sender,
            union: None,
            hinted_since,
        }
    }
}
impl Negotiation<'_> {
    fn offer(&self) -> (Offer, Option<BTreeSet<Id>>) {
        let floor = match self.confirmed {
            None if !self.initiator => return (Offer::Ask, None),
            // Both processes restarted. On large calendars try recent plans
            // before transferring everything. The receiver verifies the full digest.
            None if self.calendar.plans.len() >= COLD_MIN_PLANS => {
                self.calendar.latest().saturating_sub(COLD_REVISIONS)
            }
            None => return (Offer::Default, None),
            Some(entry) if entry.digest == self.remote => return (Offer::HasRemote, None),
            Some(entry) if entry.digest == self.local => {
                return (Offer::Unchanged(entry.revision), None);
            }
            Some(entry) => entry.revision,
        };
        match mutual::changed_ids(self.calendar, floor) {
            Some(ids) => (Offer::Changes(floor), Some(ids)),
            None => (Offer::Default, None),
        }
    }

    pub(super) fn negotiate(
        &self,
        io: &mut (impl Read + Write),
        noise: &mut TransportState,
        permit: impl Fn() -> Fallible<()>,
    ) -> Fallible<Route> {
        // The responder gets the first offer. Only a responder without a
        // confirmation can ask the initiator to offer instead, exactly once.
        let (offer, offered_here, own) = if self.initiator {
            let remote = Offer::read(io, noise)?;
            if remote == Offer::Ask {
                let (own, ids) = self.offer();
                permit()?;
                own.send(io, noise)?;
                (own, true, ids)
            } else {
                (remote, false, None)
            }
        } else {
            let (own, ids) = self.offer();
            permit()?;
            own.send(io, noise)?;
            if own == Offer::Ask {
                (Offer::read(io, noise)?, false, None)
            } else {
                (own, true, ids)
            }
        };
        match offer {
            Offer::Ask => Err(INVALID_DELTA.into()),
            Offer::Default => Ok(Route::one_way(self.initiator, None)),
            Offer::HasRemote => Ok(Route::one_way(offered_here, None)),
            Offer::Unchanged(revision) => Ok(Route::one_way(!offered_here, Some(revision))),
            Offer::Changes(revision) => {
                let union = if offered_here {
                    let own = own.ok_or(INVALID_DELTA)?;
                    permit()?;
                    mutual::send_ids(io, noise, Some(&own))?;
                    let ids = mutual::read_ids(io, noise)?;
                    if ids.as_ref().is_some_and(|ids| !own.is_subset(ids)) {
                        return Err(INVALID_DELTA.into());
                    }
                    ids
                } else {
                    let mut remote = mutual::read_ids(io, noise)?.ok_or(INVALID_DELTA)?;
                    let selected = mutual::changed_ids(self.calendar, revision).and_then(|own| {
                        remote.extend(own);
                        (remote.len() <= PATCH_PLANS).then_some(remote)
                    });
                    permit()?;
                    mutual::send_ids(io, noise, selected.as_ref())?;
                    selected
                };
                Ok(Route {
                    sender: self.initiator,
                    union,
                    hinted_since: None,
                })
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simultaneous_restart_offers_only_a_bounded_recent_window() {
        use crate::model::{Plan, Revision};
        fn offer(calendar: &Calendar, initiator: bool) -> (Offer, Option<BTreeSet<Id>>) {
            Negotiation {
                calendar,
                confirmed: None,
                local: [1; 32],
                remote: [2; 32],
                initiator,
            }
            .offer()
        }
        let mut calendar = Calendar::default();
        for n in 1..=COLD_MIN_PLANS {
            let id = Id::from_u128(n as u128);
            let mut plan = Plan::default();
            let revision = Revision {
                sequence: n as u64,
                operation: id,
            };
            plan.templates.insert(2026, Record::new(revision, None));
            calendar.plans.insert(id, plan);
            if n < COLD_MIN_PLANS {
                assert_eq!(offer(&calendar, true), (Offer::Default, None));
            }
        }
        assert_eq!(offer(&calendar, false), (Offer::Ask, None));
        let floor = COLD_MIN_PLANS as u64 - COLD_REVISIONS;
        let ids = (floor + 1..=COLD_MIN_PLANS as u64)
            .map(|n| Id::from_u128(n.into()))
            .collect();
        assert_eq!(offer(&calendar, true), (Offer::Changes(floor), Some(ids)));
        // A remembered peer already supplied a real revision. If its range is
        // too large, use the full transfer instead of trying an invented floor.
        let known = Negotiation {
            calendar: &calendar,
            confirmed: Some(Baseline {
                scope: PeerScope {
                    workspace: Id::nil(),
                    generation: 0,
                    device: Id::nil(),
                },
                digest: [3; 32],
                revision: 0,
            }),
            local: [1; 32],
            remote: [2; 32],
            initiator: true,
        };
        assert_eq!(known.offer(), (Offer::Default, None));
        // An imported group can share a single revision.
        for plan in calendar.plans.values_mut() {
            plan.templates.get_mut(&2026).unwrap().revision.sequence = 1;
        }
        assert_eq!(offer(&calendar, true), (Offer::Default, None));
    }
    #[test]
    fn offers_require_an_exact_known_encoding() {
        for invalid in [
            &[][..],
            &[0, 0],
            &[1, 0],
            &[2, 0],
            &[3],
            &[3, 0],
            &[4],
            &[5],
            &[4; 10],
        ] {
            assert!(Offer::decode(invalid).is_err());
        }
        assert_eq!(
            Offer::decode(&[3, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap(),
            Offer::Unchanged(0)
        );
        assert_eq!(
            Offer::decode(&[4, 255, 255, 255, 255, 255, 255, 255, 255]).unwrap(),
            Offer::Changes(u64::MAX)
        );
    }
}
