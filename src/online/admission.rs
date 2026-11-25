//! Per-listener limits on credential discovery. A remembered fingerprint only skips
//! discovery admission. The vault still authenticates and authorizes each request.
use blake2::{Blake2s256, Digest};
use godovoy_plan::{model::Id, storage::online::GrantPurpose};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

const BURST: u64 = 8;
const MAX_REMEMBERED: usize = 512;
const DENIAL_INTERVAL: Duration = Duration::from_secs(60);

pub(super) type Fingerprint = [u8; 32];

pub(super) fn credential_hint(token: &str, purpose: GrantPurpose) -> Option<(Id, Fingerprint)> {
    let (id, secret) = token.split_once('.')?;
    let id = Id::parse_str(id).ok()?;
    if secret.len() != 43
        || !secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return None;
    }
    let mut hash = Blake2s256::new();
    hash.update([match purpose {
        GrantPurpose::Employee => 0,
        GrantPurpose::Mcp => 1,
    }]);
    hash.update(id.as_bytes());
    hash.update(secret.as_bytes());
    Some((id, hash.finalize().into()))
}

pub(super) enum Admission {
    Known,
    New,
    Limited,
}

pub(super) struct Discovery {
    known: BTreeSet<Fingerprint>,
    credits: u64,
    refill: Instant,
    last_denial: Option<Instant>,
}

impl Discovery {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            known: BTreeSet::new(),
            credits: BURST,
            refill: now,
            last_denial: None,
        }
    }

    pub(super) fn admit(&mut self, key: &Fingerprint, now: Instant) -> Admission {
        if self.known.contains(key) {
            return Admission::Known;
        }
        let elapsed = now.saturating_duration_since(self.refill).as_secs();
        self.credits = self.credits.saturating_add(elapsed).min(BURST);
        self.refill += Duration::from_secs(elapsed);
        if self.credits == 0 {
            return Admission::Limited;
        }
        self.credits -= 1;
        Admission::New
    }

    pub(super) fn remember(&mut self, key: Fingerprint) {
        if !self.known.contains(&key) && self.known.len() >= MAX_REMEMBERED {
            self.known.pop_first();
        }
        self.known.insert(key);
    }

    pub(super) fn reject(&mut self, key: &Fingerprint, now: Instant) -> bool {
        self.known.remove(key);
        if self
            .last_denial
            .is_some_and(|last| now.saturating_duration_since(last) < DENIAL_INTERVAL)
        {
            return false;
        }
        self.last_denial = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_identify_the_credential_and_its_purpose() {
        let id = Id::new_v4();
        let employee_key = format!("{id}.{}", "a".repeat(43));
        let second_key = format!("{id}.{}", "b".repeat(43));
        let (_, hint) = credential_hint(&employee_key, GrantPurpose::Employee).unwrap();
        assert_ne!(
            hint,
            credential_hint(&second_key, GrantPurpose::Employee)
                .unwrap()
                .1
        );
        assert_ne!(
            hint,
            credential_hint(&employee_key, GrantPurpose::Mcp).unwrap().1
        );
        assert!(credential_hint("incomplete", GrantPurpose::Employee).is_none());
    }

    #[test]
    fn new_credentials_share_a_bounded_refilling_budget() {
        let start = Instant::now();
        let mut discovery = Discovery::new(start);
        let key = [17; 32];
        for _ in 0..BURST {
            assert!(matches!(discovery.admit(&key, start), Admission::New));
        }
        assert!(matches!(
            discovery.admit(&key, start + Duration::from_millis(999)),
            Admission::Limited
        ));
        let later = start + Duration::from_secs(1);
        assert!(matches!(discovery.admit(&key, later), Admission::New));
        assert!(matches!(discovery.admit(&key, later), Admission::Limited));
        let later = start + Duration::from_secs(120);
        for _ in 0..BURST {
            assert!(matches!(discovery.admit(&key, later), Admission::New));
        }
        assert!(matches!(discovery.admit(&key, later), Admission::Limited));
    }

    #[test]
    fn rejected_hints_are_forgotten_even_when_their_audit_is_sampled() {
        let now = Instant::now();
        let mut discovery = Discovery::new(now);
        for number in 0..=MAX_REMEMBERED {
            discovery.remember(Blake2s256::digest(number.to_le_bytes()).into());
        }
        assert_eq!(discovery.known.len(), MAX_REMEMBERED);
        let key = [42; 32];
        discovery.remember(key);
        discovery.credits = 0;
        assert!(matches!(discovery.admit(&key, now), Admission::Known));
        assert!(discovery.reject(&[99; 32], now));
        assert!(!discovery.reject(&key, now));
        assert!(matches!(discovery.admit(&key, now), Admission::Limited));
        assert!(!discovery.reject(&key, now + Duration::from_secs(59)));
        assert!(discovery.reject(&key, now + DENIAL_INTERVAL));
    }
}
