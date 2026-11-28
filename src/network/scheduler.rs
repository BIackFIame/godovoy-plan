//! Schedules exchanges with discovered and saved peers.
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddrV4,
    sync::{Arc, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::Fallible;

use super::{Core, DISCOVERY_TTL, InboundGrace, SyncPace, sync_peer};

const QUIET_SYNC_INTERVAL: Duration = Duration::from_secs(9);

#[derive(Clone, Copy, Default)]
pub(super) enum PeerProtocol {
    #[default]
    Unknown,
    LegacyUntil(Instant),
    Digest,
}

pub(super) struct Backoff {
    next: Instant,
    last_attempt: Instant,
    protocol: PeerProtocol,
    failures: u8,
    quiet_matches: u8,
}

impl Backoff {
    fn record_exchange(&mut self, result: Fallible<SyncPace>, now: Instant) {
        self.quiet_matches = if matches!(result, Ok(SyncPace::QuietMatch)) {
            self.quiet_matches.saturating_add(1)
        } else {
            0
        };
        self.failures = if result.is_ok() {
            0
        } else {
            (self.failures + 1).min(3)
        };
        let interval = if self.quiet_matches >= 2 {
            QUIET_SYNC_INTERVAL
        } else {
            Duration::from_secs(3 << self.failures.saturating_sub(1))
        };
        self.last_attempt = now;
        self.next = self.last_attempt + interval;
    }
}

pub(super) fn next_peer(
    peers: impl Iterator<Item = SocketAddrV4>,
    preferred: &BTreeSet<SocketAddrV4>,
    attempts: &BTreeMap<SocketAddrV4, Backoff>,
    inbound_until: &BTreeMap<SocketAddrV4, InboundGrace>,
    now: Instant,
) -> Option<SocketAddrV4> {
    peers
        .filter(|peer| {
            inbound_until
                .get(peer)
                .is_none_or(|entry| entry.until <= now)
        })
        .filter(|peer| attempts.get(peer).is_none_or(|retry| retry.next <= now))
        .min_by_key(|peer| {
            (
                attempts.get(peer).map(|retry| retry.next),
                !preferred.contains(peer),
            )
        })
}

pub(super) fn sync_forever(core: Arc<Core>) {
    let _ = core
        .inbound
        .get()
        .unwrap()
        .sync_thread
        .set(thread::current());
    let inbound = core.inbound.get().unwrap();
    let calendar_wake = core.store.lock().unwrap().subscribe_calendar();
    let mut attempts = BTreeMap::<SocketAddrV4, Backoff>::new();
    let mut generation = core.generation.load(Ordering::Acquire);
    while !core.stopped.load(Ordering::Relaxed) {
        inbound.retry(&core);
        let current_generation = core.generation.load(Ordering::Acquire);
        if generation != current_generation {
            let now = Instant::now();
            for attempt in attempts.values_mut() {
                // Recheck reachability on resume without forgetting a known
                // digest peer and allowing legacy fallback on its address.
                attempt.last_attempt = now;
                attempt.next = now;
                attempt.failures = 0;
                attempt.quiet_matches = 0;
            }
            core.inbound_until.lock().unwrap().clear();
            generation = current_generation;
        }
        let mut next_dial = None;
        if core.active()
            && let Ok((_, workspace, _)) = core.sync_context()
        {
            let mut preferred: BTreeSet<_> = core.routes.lock().unwrap().addresses().collect();
            preferred.extend(core.discovered.lock().unwrap().iter().filter_map(
                |(&address, seen)| {
                    (seen.workspace == workspace && seen.time.elapsed() < DISCOVERY_TTL)
                        .then_some(address)
                },
            ));
            let peers: BTreeSet<_> = core
                .snapshot()
                .peers
                .union(&preferred)
                .filter(|peer| core.permits_ip(*peer.ip()))
                .copied()
                .collect();
            attempts.retain(|address, _| peers.contains(address));
            let now = Instant::now();
            for address in &peers {
                attempts.entry(*address).or_insert(Backoff {
                    // A newly admitted peer must finish saving its workspace
                    // before it can authenticate a separate sync connection.
                    next: now + Duration::from_millis(300),
                    last_attempt: now,
                    failures: 0,
                    quiet_matches: 0,
                    protocol: PeerProtocol::Unknown,
                });
            }
            if calendar_wake.take_change() {
                // A real edit can use either direction. Preserve the failure backoff
                // and coalesce rapid writes to a healthy peer into one exchange.
                // An authenticated sender already has this published version.
                // Drop ordinary grace and stale confirmations before choosing a peer.
                core.inbound_until
                    .lock()
                    .unwrap()
                    .retain(|_, entry| entry.confirmed.is_some());
                for attempt in attempts
                    .values_mut()
                    .filter(|attempt| attempt.failures == 0)
                {
                    attempt.quiet_matches = 0;
                    attempt.next = attempt
                        .next
                        .min(attempt.last_attempt + Duration::from_millis(300));
                }
            }
            let address = {
                let receiving = core.receiving_deltas.lock().unwrap();
                let incoming = core.inbound_schedule(now);
                next_peer(
                    peers
                        .iter()
                        .copied()
                        .filter(|peer| receiving.get(peer).is_none_or(|p| now >= p.defer_until)),
                    &preferred,
                    &attempts,
                    &incoming,
                    now,
                )
            };
            if let Some(address) = address {
                let retry = attempts.get_mut(&address).expect("known peer");
                let result = sync_peer(&core, address, &mut retry.protocol);
                retry.record_exchange(result, Instant::now());
            }
            // Respect the existing 300 ms pacing between distinct ready peers.
            // With nothing due, sleep until the earliest possible connection.
            let now = Instant::now();
            let receiving = core.receiving_deltas.lock().unwrap();
            let incoming = core.inbound_schedule(now);
            if let Some(earliest) = peers
                .iter()
                .map(|peer| {
                    let retry = attempts.get(peer).map_or(now, |entry| entry.next);
                    retry
                        .max(incoming.get(peer).map_or(now, |entry| entry.until))
                        .max(receiving.get(peer).map_or(now, |p| p.defer_until))
                })
                .min()
            {
                next_dial = Some(
                    earliest
                        .saturating_duration_since(now)
                        .max(Duration::from_millis(300)),
                );
            }
        }
        let retry = inbound
            .retry_at()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        // Publication, discovery, resume and shutdown each unpark this worker.
        let dial = next_dial.filter(|_| core.active());
        match dial.into_iter().chain(retry).min() {
            Some(delay) => thread::park_timeout(delay),
            None => thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_polling_requires_two_edit_aware_matches_and_resets_on_changes_or_errors() {
        let now = Instant::now();
        let mut peer = Backoff {
            next: now,
            last_attempt: now,
            failures: 0,
            quiet_matches: 0,
            protocol: PeerProtocol::Digest,
        };
        for (result, seconds) in [
            (Ok(SyncPace::QuietMatch), 3),
            (Ok(SyncPace::QuietMatch), 9),
            (Err("offline".into()), 3),
            (Err("offline".into()), 6),
            (Ok(SyncPace::QuietMatch), 3),
            (Ok(SyncPace::QuietMatch), 9),
            (Ok(SyncPace::Regular), 3),
            (Ok(SyncPace::Regular), 3),
            (Ok(SyncPace::QuietMatch), 3),
        ] {
            peer.record_exchange(result, now);
            assert_eq!(peer.next - now, Duration::from_secs(seconds));
        }
    }
}
