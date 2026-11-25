//! Discovery hints are authenticated separately from enrollment and sync sessions.
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
