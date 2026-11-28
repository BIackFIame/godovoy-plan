//! Discovery hints are authenticated separately from enrollment and sync sessions.
use std::{
    collections::BTreeMap,
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    sync::{Arc, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use socket2::{Domain, Protocol, Socket, Type};

use super::{Core, DISCOVERY_TTL, MAX_PEERS, Seen, Wait};

use crate::{model::Id, storage::random_bytes};
use blake2::{
    Blake2sMac256,
    digest::{KeyInit, Mac},
};
use chacha20poly1305::{XChaCha20Poly1305, XNonce, aead::AeadInPlace};
use zeroize::Zeroizing;

const HEADER: &[u8; 8] = b"YRDADV02";
const NONCE_END: usize = 32;
const DATA_END: usize = NONCE_END + 16 + 16 + 2 + 8;
pub(super) const LENGTH: usize = DATA_END + 16;

pub(super) struct Key(XChaCha20Poly1305);

impl Key {
    pub(super) fn derive(secret: &[u8; 32], group: bool, namespace: Id) -> Self {
        let mut mac = <Blake2sMac256 as KeyInit>::new(secret.into());
        Mac::update(&mut mac, b"GodovoyPlan/LAN/Discovery/XChaCha20Poly1305/2");
        Mac::update(&mut mac, &[u8::from(group)]);
        Mac::update(&mut mac, namespace.as_bytes());
        let key = Zeroizing::new(<[u8; 32]>::from(mac.finalize().into_bytes()));
        Self(XChaCha20Poly1305::new((&*key).into()))
    }

    pub(super) fn seal(
        &self,
        device: Id,
        workspace: Id,
        port: u16,
        now: i64,
    ) -> Option<[u8; LENGTH]> {
        let nonce = random_bytes::<24>().ok()?;
        let mut packet = [0; LENGTH];
        packet[..8].copy_from_slice(HEADER);
        packet[8..NONCE_END].copy_from_slice(&nonce);
        let body = &mut packet[NONCE_END..DATA_END];
        body[..16].copy_from_slice(device.as_bytes());
        body[16..32].copy_from_slice(workspace.as_bytes());
        body[32..34].copy_from_slice(&port.to_be_bytes());
        body[34..].copy_from_slice(&now.to_be_bytes());
        let tag = self
            .0
            .encrypt_in_place_detached(XNonce::from_slice(&nonce), HEADER, body)
            .ok()?;
        packet[DATA_END..].copy_from_slice(&tag);
        Some(packet)
    }

    pub(super) fn open(&self, packet: &[u8], now: i64) -> Option<(Id, Id, u16)> {
        if packet.len() != LENGTH || !packet.starts_with(HEADER) {
            return None;
        }
        let mut body: [u8; DATA_END - NONCE_END] = packet[NONCE_END..DATA_END].try_into().ok()?;
        self.0
            .decrypt_in_place_detached(
                XNonce::from_slice(&packet[8..NONCE_END]),
                HEADER,
                &mut body,
                chacha20poly1305::Tag::from_slice(&packet[DATA_END..]),
            )
            .ok()?;
        let sent = i64::from_be_bytes(body[34..].try_into().ok()?);
        let port = u16::from_be_bytes(body[32..34].try_into().ok()?);
        if port == 0 || now.abs_diff(sent) > 120 {
            return None;
        }
        Some((
            Id::from_slice(&body[..16]).ok()?,
            Id::from_slice(&body[16..32]).ok()?,
            port,
        ))
    }
}
pub(super) fn socket(ip: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_broadcast(ip.is_unspecified())?;
    socket.bind(&SocketAddrV4::new(ip, port).into())?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

pub(super) fn run(core: Arc<Core>, mut wait: Wait, port: u16) {
    let inbound = core.inbound.get().unwrap();
    let mut announce_at = Instant::now();
    // One extra byte distinguishes oversized/truncated packets.
    let mut packet = [0; LENGTH + 1];
    while !core.stopped.load(Ordering::Relaxed) {
        if !core.active() {
            inbound.wait_for_resume(&core);
            continue;
        }
        if Instant::now() >= announce_at {
            announce_at = Instant::now() + Duration::from_secs(3);
            let state = core.snapshot();
            if let Some(team) = &state.workspace {
                let mut keys = vec![Key::derive(&team.key, true, team.id)];
                let mut invites = core.invites.lock().unwrap();
                let now = Instant::now();
                invites.retain(|_, invite| invite.expires > now);
                keys.extend(
                    invites
                        .iter()
                        .map(|(id, invite)| Key::derive(&invite.key, false, *id)),
                );
                drop(invites);
                for key in keys {
                    if !core.active() {
                        break;
                    }
                    if let Some(message) = key.seal(
                        state.device,
                        team.id,
                        core.advertised_port(),
                        Utc::now().timestamp(),
                    ) {
                        inbound.announce(&message, port);
                    }
                }
            }
        }
        let (registered, result) = inbound.receive(&mut packet);
        let source = match result {
            Ok((LENGTH, SocketAddr::V4(source))) if core.active() => source,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait.until(
                    registered,
                    announce_at.saturating_duration_since(Instant::now()),
                );
                continue;
            }
            Err(_) => {
                thread::park_timeout(Duration::from_millis(40));
                continue;
            }
            _ => continue,
        };
        if !core.active() || !core.permits_ip(*source.ip()) {
            continue;
        }
        let state = core.snapshot();
        let now = Utc::now().timestamp();
        let packet = &packet[..LENGTH];
        let hint = state
            .workspace
            .as_ref()
            .and_then(|team| {
                Key::derive(&team.key, true, team.id)
                    .open(packet, now)
                    .filter(|(_, group, _)| *group == team.id)
            })
            .or_else(|| {
                let pending = core.pending_discovery.lock().unwrap();
                let (host, key) = pending.as_ref()?;
                key.open(packet, now).filter(|(from, _, _)| from == host)
            });
        let Some((from, remote_workspace, listen_port)) = hint else {
            continue;
        };
        if from == state.device {
            continue;
        }
        let address = SocketAddrV4::new(*source.ip(), listen_port);
        let mut seen = core.discovered.lock().unwrap();
        let new_route = seen.get(&address).is_none_or(|old| {
            old.workspace != remote_workspace || old.time.elapsed() >= DISCOVERY_TTL
        });
        remember(
            &mut seen,
            address,
            Seen {
                device: from,
                workspace: remote_workspace,
                time: Instant::now(),
            },
        );
        if new_route && seen.contains_key(&address) {
            core.wake_sync();
        }
    }
}

pub(super) fn remember(
    seen: &mut BTreeMap<SocketAddrV4, Seen>,
    address: SocketAddrV4,
    beacon: Seen,
) {
    seen.retain(|previous, record| {
        let same_device = previous.ip() == address.ip()
            && record.device == beacon.device
            && record.workspace == beacon.workspace;
        !same_device && beacon.time.saturating_duration_since(record.time) < DISCOVERY_TTL
    });
    let other_ports = seen.keys().filter(|key| key.ip() == address.ip()).count();
    if seen.contains_key(&address) || (seen.len() < MAX_PEERS && other_ports < 3) {
        seen.insert(address, beacon);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hints_are_private_authenticated_fresh_and_domain_separated() {
        let group = Id::new_v4();
        let device = Id::new_v4();
        let key = Key::derive(&[1; 32], true, group);
        let mut packet = key.seal(device, group, 45736, 1000).unwrap();
        assert_eq!(key.open(&packet, 1001), Some((device, group, 45736)));
        assert!(
            !packet
                .windows(16)
                .any(|v| v == device.as_bytes() || v == group.as_bytes())
        );
        assert_ne!(packet, key.seal(device, group, 45736, 1000).unwrap());
        assert!(
            Key::derive(&[1; 32], false, group)
                .open(&packet, 1001)
                .is_none()
        );
        assert!(
            Key::derive(&[2; 32], true, group)
                .open(&packet, 1001)
                .is_none()
        );
        assert!(key.open(&packet, 1121).is_none());
        packet[40] ^= 1;
        assert!(key.open(&packet, 1001).is_none());
    }
}
