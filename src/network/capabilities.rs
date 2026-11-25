//! Optional payload inside the authenticated digest-sync Noise handshake.
//! Exact length prevents an unsupported revision from being interpreted as v1.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Capabilities(u16);

impl Capabilities {
    pub(super) const NONE: Self = Self(0);
    pub(super) const DELTA: Self = Self(1 << 0);
    pub(super) const EDIT_WAKE: Self = Self(1 << 1);

    pub(super) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(super) fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }

    pub(super) fn offer(self) -> [u8; 9] {
        let [hi, lo] = self.0.to_be_bytes();
        [b'G', b'P', b'-', b'C', b'A', b'P', 1, hi, lo]
    }

    pub(super) fn negotiate(self, payload: &[u8]) -> Self {
        match payload {
            [b'G', b'P', b'-', b'C', b'A', b'P', 1, hi, lo] => {
                let known = Self::DELTA.union(Self::EDIT_WAKE);
                Self(self.0 & known.0 & u16::from_be_bytes([*hi, *lo]))
            }
            _ => Self::NONE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Capabilities as Caps;

    #[test]
    fn optional_features_are_intersected_independently() {
        let both = Caps::EDIT_WAKE.union(Caps::DELTA);
        assert_eq!(both.offer(), *b"GP-CAP\x01\x00\x03");
        for local in [Caps::NONE, Caps::DELTA, Caps::EDIT_WAKE, both] {
            for remote in [Caps::NONE, Caps::DELTA, Caps::EDIT_WAKE, both] {
                let agreed = local.negotiate(&remote.offer());
                assert_eq!(agreed, remote.negotiate(&local.offer()));
                assert_eq!(agreed, local.negotiate(&agreed.offer()));
                for flag in [Caps::DELTA, Caps::EDIT_WAKE] {
                    assert_eq!(
                        agreed.contains(flag),
                        local.contains(flag) && remote.contains(flag)
                    );
                }
            }
        }
        assert_eq!(both.negotiate(b"GP-CAP\x01\x80\x02"), Caps::EDIT_WAKE);
        assert_eq!(both.negotiate(b"GP-CAP\x01\x80\x00"), Caps::NONE);
        for unknown in [
            &[][..],
            b"GP-CAP",
            b"GP-CAP\x02\x00\x03",
            b"GP-CAP\x01\x00\x03\x00",
            b"GP-???\x01\x00\x03",
        ] {
            assert_eq!(both.negotiate(unknown), Caps::NONE);
        }
    }
}
