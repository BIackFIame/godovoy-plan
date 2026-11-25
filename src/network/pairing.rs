use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use curve25519_dalek::montgomery::MontgomeryPoint;
use snow::Builder;
use zeroize::Zeroizing;

use crate::{Fallible, model::Id};

const PAIRING_NOISE: &str = "Noise_KKpsk0_25519_ChaChaPoly_BLAKE2s";
const REQUEST_PREFIX: &str = "device1.";

pub(super) struct PairingKeys {
    private: Zeroizing<Vec<u8>>,
    public: [u8; 32],
}

impl PairingKeys {
    pub(super) fn generate() -> Fallible<Self> {
        let pair = Builder::new(PAIRING_NOISE.parse()?).generate_keypair()?;
        Ok(Self {
            private: Zeroizing::new(pair.private),
            public: pair
                .public
                .try_into()
                .map_err(|_| "Неверная длина публичного ключа.")?,
        })
    }

    pub(super) fn encoded_public(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.public)
    }

    pub(super) fn request(&self, device: Id) -> String {
        format!("{REQUEST_PREFIX}{device}.{}", self.encoded_public())
    }

    pub(super) fn from_request(request: &str) -> Fallible<(Id, [u8; 32])> {
        let encoded = request
            .trim()
            .strip_prefix(REQUEST_PREFIX)
            .ok_or("Вставьте запрос из окна «Присоединиться» на компьютере коллеги.")?;
        let (device, public) = encoded
            .split_once('.')
            .ok_or("Неполный запрос устройства.")?;
        Ok((Id::parse_str(device)?, decode_public(public)?))
    }
}

pub(super) fn decode_public(encoded: &str) -> Fallible<[u8; 32]> {
    let mut key = [0; 32];
    if encoded.len() != 43 || URL_SAFE_NO_PAD.decode_slice(encoded, &mut key).ok() != Some(32) {
        return Err("Некорректный публичный ключ устройства.".into());
    }
    // Ensure the supplied X25519 public key contributes to key agreement.
    if MontgomeryPoint(key).mul_clamped([0; 32]).to_bytes() == [0; 32] {
        return Err("Недопустимый публичный ключ устройства.".into());
    }
    Ok(key)
}

pub(super) enum ChannelKey<'a> {
    Group(&'a [u8; 32]),
    Invitation {
        secret: &'a [u8; 32],
        peer: &'a [u8; 32],
    },
}

impl<'a> ChannelKey<'a> {
    pub(super) fn builder(self, local: &'a PairingKeys, header: &'a [u8]) -> Fallible<Builder<'a>> {
        Ok(match self {
            Self::Group(secret) => super::noise(header, secret)?,
            Self::Invitation { secret, peer } => Builder::new(PAIRING_NOISE.parse()?)
                .local_private_key(&local.private)?
                .remote_public_key(peer)?
                .prologue(header)?
                .psk(0, secret)?,
        })
    }
}
