extern crate blake2_rfc;
extern crate byteorder;
#[macro_use]
extern crate lazy_static;
extern crate pairing;
extern crate sapling_crypto;

use blake2_rfc::blake2b::{Blake2b, Blake2bResult};
use byteorder::{ByteOrder, LittleEndian};
use pairing::{bls12_381::Bls12, Field, PrimeField, PrimeFieldRepr};
use sapling_crypto::{
    jubjub::{FixedGenerators, JubjubBls12, JubjubEngine, JubjubParams, ToUniform},
    primitives::ViewingKey,
};

lazy_static! {
    static ref JUBJUB: JubjubBls12 = { JubjubBls12::new() };
}

pub const PRF_EXPAND_PERSONALIZATION: &'static [u8; 16] = b"Zcash_ExpandSeed";
pub const ZIP32_SAPLING_MASTER_PERSONALIZATION: &'static [u8; 16] = b"ZcashIP32Sapling";
pub const ZIP32_SAPLING_FVFP_PERSONALIZATION: &'static [u8; 16] = b"ZcashSaplingFVFP";

// Sapling key components

/// PRF^expand(sk, t) := BLAKE2b-512("Zcash_ExpandSeed", sk || t)
fn prf_expand(sk: &[u8], t: &[u8]) -> Blake2bResult {
    prf_expand_vec(sk, &vec![t])
}

fn prf_expand_vec(sk: &[u8], ts: &[&[u8]]) -> Blake2bResult {
    let mut h = Blake2b::with_params(64, &[], &[], PRF_EXPAND_PERSONALIZATION);
    h.update(sk);
    for t in ts {
        h.update(t);
    }
    h.finalize()
}

/// An outgoing viewing key
#[derive(Clone, Copy, PartialEq)]
struct OutgoingViewingKey([u8; 32]);

impl OutgoingViewingKey {
    fn derive_child(&self, i_l: &[u8]) -> Self {
        let mut ovk = [0u8; 32];
        ovk.copy_from_slice(&prf_expand_vec(i_l, &[&[0x15], &self.0]).as_bytes()[..32]);
        OutgoingViewingKey(ovk)
    }
}

/// A Sapling expanded spending key
#[derive(Clone)]
struct ExpandedSpendingKey<E: JubjubEngine> {
    ask: E::Fs,
    nsk: E::Fs,
    ovk: OutgoingViewingKey,
}

/// A Sapling full viewing key
struct FullViewingKey<E: JubjubEngine> {
    vk: ViewingKey<E>,
    ovk: OutgoingViewingKey,
}

impl<E: JubjubEngine> ExpandedSpendingKey<E> {
    fn from_spending_key(sk: &[u8]) -> Self {
        let ask = E::Fs::to_uniform(prf_expand(sk, &[0x00]).as_bytes());
        let nsk = E::Fs::to_uniform(prf_expand(sk, &[0x01]).as_bytes());
        let mut ovk = OutgoingViewingKey([0u8; 32]);
        ovk.0
            .copy_from_slice(&prf_expand(sk, &[0x02]).as_bytes()[..32]);
        ExpandedSpendingKey { ask, nsk, ovk }
    }

    fn derive_child(&self, i_l: &[u8]) -> Self {
        let mut ask = E::Fs::to_uniform(prf_expand(i_l, &[0x13]).as_bytes());
        let mut nsk = E::Fs::to_uniform(prf_expand(i_l, &[0x14]).as_bytes());
        ask.add_assign(&self.ask);
        nsk.add_assign(&self.nsk);
        let ovk = self.ovk.derive_child(i_l);
        ExpandedSpendingKey { ask, nsk, ovk }
    }

    fn to_bytes(&self) -> [u8; 96] {
        let mut result = [0u8; 96];
        self.ask
            .into_repr()
            .write_le(&mut result[..32])
            .expect("length is 32 bytes");
        self.nsk
            .into_repr()
            .write_le(&mut result[32..64])
            .expect("length is 32 bytes");
        (&mut result[64..]).copy_from_slice(&self.ovk.0);
        result
    }
}

impl<E: JubjubEngine> FullViewingKey<E> {
    fn from_expanded_spending_key(xsk: &ExpandedSpendingKey<E>, params: &E::Params) -> Self {
        FullViewingKey {
            vk: ViewingKey {
                ak: params
                    .generator(FixedGenerators::SpendingKeyGenerator)
                    .mul(xsk.ask, params),
                nk: params
                    .generator(FixedGenerators::ProofGenerationKey)
                    .mul(xsk.nsk, params),
            },
            ovk: xsk.ovk,
        }
    }

    fn derive_child(&self, i_l: &[u8], params: &E::Params) -> Self {
        let i_ask = E::Fs::to_uniform(prf_expand(i_l, &[0x13]).as_bytes());
        let i_nsk = E::Fs::to_uniform(prf_expand(i_l, &[0x14]).as_bytes());
        let ak = params
            .generator(FixedGenerators::SpendingKeyGenerator)
            .mul(i_ask, params)
            .add(&self.vk.ak, params);
        let nk = params
            .generator(FixedGenerators::ProofGenerationKey)
            .mul(i_nsk, params)
            .add(&self.vk.nk, params);

        FullViewingKey {
            vk: ViewingKey { ak, nk },
            ovk: self.ovk.derive_child(i_l),
        }
    }

    fn to_bytes(&self) -> [u8; 96] {
        let mut result = [0u8; 96];
        self.vk
            .ak
            .write(&mut result[..32])
            .expect("length is 32 bytes");
        self.vk
            .nk
            .write(&mut result[32..64])
            .expect("length is 32 bytes");
        (&mut result[64..]).copy_from_slice(&self.ovk.0);
        result
    }
}

// ZIP 32 structures

/// A Sapling full viewing key fingerprint
struct FVKFingerprint([u8; 32]);

impl<'a, E: JubjubEngine> From<&'a FullViewingKey<E>> for FVKFingerprint {
    fn from(fvk: &FullViewingKey<E>) -> Self {
        let mut h = Blake2b::with_params(32, &[], &[], ZIP32_SAPLING_FVFP_PERSONALIZATION);
        h.update(&fvk.to_bytes());
        let mut fvfp = [0u8; 32];
        fvfp.copy_from_slice(h.finalize().as_bytes());
        FVKFingerprint(fvfp)
    }
}

/// A Sapling full viewing key tag
#[derive(Clone, Copy, Debug, PartialEq)]
struct FVKTag([u8; 4]);

impl<'a> From<&'a FVKFingerprint> for FVKTag {
    fn from(fingerprint: &FVKFingerprint) -> Self {
        let mut tag = [0u8; 4];
        tag.copy_from_slice(&fingerprint.0[..4]);
        FVKTag(tag)
    }
}

impl From<FVKFingerprint> for FVKTag {
    fn from(fingerprint: FVKFingerprint) -> Self {
        (&fingerprint).into()
    }
}

impl FVKTag {
    fn master() -> Self {
        FVKTag([0u8; 4])
    }
}

/// A child index for a derived key
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChildIndex {
    NonHardened(u32),
    Hardened(u32), // Hardened(n) == n + (1 << 31) == n' in path notation
}

impl ChildIndex {
    pub fn from_index(i: u32) -> Self {
        match i {
            n if n >= (1 << 31) => ChildIndex::Hardened(n - (1 << 31)),
            n => ChildIndex::NonHardened(n),
        }
    }

    fn master() -> Self {
        ChildIndex::from_index(0)
    }
}

/// A chain code
#[derive(Clone, Copy, Debug, PartialEq)]
struct ChainCode([u8; 32]);

/// A key used to derive diversifiers for a particular child key
#[derive(Clone, Copy, Debug, PartialEq)]
struct DiversifierKey([u8; 32]);

impl DiversifierKey {
    fn master(sk_m: &[u8]) -> Self {
        let mut dk_m = [0u8; 32];
        dk_m.copy_from_slice(&prf_expand(sk_m, &[0x10]).as_bytes()[..32]);
        DiversifierKey(dk_m)
    }

    fn derive_child(&self, i_l: &[u8]) -> Self {
        let mut dk = [0u8; 32];
        dk.copy_from_slice(&prf_expand_vec(i_l, &[&[0x16], &self.0]).as_bytes()[..32]);
        DiversifierKey(dk)
    }
}

/// A Sapling extended spending key
#[derive(Clone)]
pub struct ExtendedSpendingKey {
    depth: u8,
    parent_fvk_tag: FVKTag,
    child_index: ChildIndex,
    chain_code: ChainCode,
    xsk: ExpandedSpendingKey<Bls12>,
    dk: DiversifierKey,
}

// A Sapling extended full viewing key
pub struct ExtendedFullViewingKey {
    depth: u8,
    parent_fvk_tag: FVKTag,
    child_index: ChildIndex,
    chain_code: ChainCode,
    fvk: FullViewingKey<Bls12>,
    dk: DiversifierKey,
}

impl std::cmp::PartialEq for ExtendedSpendingKey {
    fn eq(&self, rhs: &ExtendedSpendingKey) -> bool {
        self.depth == rhs.depth
            && self.parent_fvk_tag == rhs.parent_fvk_tag
            && self.child_index == rhs.child_index
            && self.chain_code == rhs.chain_code
            && self.xsk.ask == rhs.xsk.ask
            && self.xsk.nsk == rhs.xsk.nsk
            && self.xsk.ovk == rhs.xsk.ovk
            && self.dk == rhs.dk
    }
}

impl std::fmt::Debug for ExtendedSpendingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        write!(
            f,
            "ExtendedSpendingKey(d = {}, tag_p = {:?}, i = {:?})",
            self.depth, self.parent_fvk_tag, self.child_index
        )
    }
}

impl std::cmp::PartialEq for ExtendedFullViewingKey {
    fn eq(&self, rhs: &ExtendedFullViewingKey) -> bool {
        self.depth == rhs.depth
            && self.parent_fvk_tag == rhs.parent_fvk_tag
            && self.child_index == rhs.child_index
            && self.chain_code == rhs.chain_code
            && self.fvk.vk.ak == rhs.fvk.vk.ak
            && self.fvk.vk.nk == rhs.fvk.vk.nk
            && self.fvk.ovk == rhs.fvk.ovk
            && self.dk == rhs.dk
    }
}

impl std::fmt::Debug for ExtendedFullViewingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        write!(
            f,
            "ExtendedFullViewingKey(d = {}, tag_p = {:?}, i = {:?})",
            self.depth, self.parent_fvk_tag, self.child_index
        )
    }
}

impl ExtendedSpendingKey {
    pub fn master(seed: &[u8]) -> Self {
        let mut h = Blake2b::with_params(64, &[], &[], ZIP32_SAPLING_MASTER_PERSONALIZATION);
        h.update(seed);
        let i = h.finalize();

        let sk_m = &i.as_bytes()[..32];
        let mut c_m = [0u8; 32];
        c_m.copy_from_slice(&i.as_bytes()[32..]);

        ExtendedSpendingKey {
            depth: 0,
            parent_fvk_tag: FVKTag::master(),
            child_index: ChildIndex::master(),
            chain_code: ChainCode(c_m),
            xsk: ExpandedSpendingKey::from_spending_key(sk_m),
            dk: DiversifierKey::master(sk_m),
        }
    }

    /// Returns the child key corresponding to the path derived from the master key
    pub fn from_path(master: &ExtendedSpendingKey, path: &[ChildIndex]) -> Self {
        let mut xsk = master.clone();
        for &i in path.iter() {
            xsk = xsk.derive_child(i);
        }
        xsk
    }

    pub fn derive_child(&self, i: ChildIndex) -> Self {
        let fvk = FullViewingKey::from_expanded_spending_key(&self.xsk, &JUBJUB);
        let tmp = match i {
            ChildIndex::Hardened(i) => {
                let mut le_i = [0; 4];
                LittleEndian::write_u32(&mut le_i, i + (1 << 31));
                prf_expand_vec(
                    &self.chain_code.0,
                    &[&[0x11], &self.xsk.to_bytes(), &self.dk.0, &le_i],
                )
            }
            ChildIndex::NonHardened(i) => {
                let mut le_i = [0; 4];
                LittleEndian::write_u32(&mut le_i, i);
                prf_expand_vec(
                    &self.chain_code.0,
                    &[&[0x12], &fvk.to_bytes(), &self.dk.0, &le_i],
                )
            }
        };
        let i_l = &tmp.as_bytes()[..32];
        let mut c_i = [0u8; 32];
        c_i.copy_from_slice(&tmp.as_bytes()[32..]);

        ExtendedSpendingKey {
            depth: self.depth + 1,
            parent_fvk_tag: FVKFingerprint::from(&fvk).into(),
            child_index: i,
            chain_code: ChainCode(c_i),
            xsk: self.xsk.derive_child(i_l),
            dk: self.dk.derive_child(i_l),
        }
    }
}

impl<'a> From<&'a ExtendedSpendingKey> for ExtendedFullViewingKey {
    fn from(xsk: &ExtendedSpendingKey) -> Self {
        ExtendedFullViewingKey {
            depth: xsk.depth,
            parent_fvk_tag: xsk.parent_fvk_tag,
            child_index: xsk.child_index,
            chain_code: xsk.chain_code,
            fvk: FullViewingKey::from_expanded_spending_key(&xsk.xsk, &JUBJUB),
            dk: xsk.dk,
        }
    }
}

impl ExtendedFullViewingKey {
    pub fn derive_child(&self, i: ChildIndex) -> Result<Self, ()> {
        let tmp = match i {
            ChildIndex::Hardened(_) => return Err(()),
            ChildIndex::NonHardened(i) => {
                let mut le_i = [0; 4];
                LittleEndian::write_u32(&mut le_i, i);
                prf_expand_vec(
                    &self.chain_code.0,
                    &[&[0x12], &self.fvk.to_bytes(), &self.dk.0, &le_i],
                )
            }
        };
        let i_l = &tmp.as_bytes()[..32];
        let mut c_i = [0u8; 32];
        c_i.copy_from_slice(&tmp.as_bytes()[32..]);

        Ok(ExtendedFullViewingKey {
            depth: self.depth + 1,
            parent_fvk_tag: FVKFingerprint::from(&self.fvk).into(),
            child_index: i,
            chain_code: ChainCode(c_i),
            fvk: self.fvk.derive_child(i_l, &JUBJUB),
            dk: self.dk.derive_child(i_l),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_nonhardened_child() {
        let seed = [0; 32];
        let xsk_m = ExtendedSpendingKey::master(&seed);
        let xfvk_m = ExtendedFullViewingKey::from(&xsk_m);

        let i_5 = ChildIndex::NonHardened(5);
        let xsk_5 = xsk_m.derive_child(i_5);
        let xfvk_5 = xfvk_m.derive_child(i_5);

        assert!(xfvk_5.is_ok());
        assert_eq!(ExtendedFullViewingKey::from(&xsk_5), xfvk_5.unwrap());
    }

    #[test]
    fn derive_hardened_child() {
        let seed = [0; 32];
        let xsk_m = ExtendedSpendingKey::master(&seed);
        let xfvk_m = ExtendedFullViewingKey::from(&xsk_m);

        let i_5h = ChildIndex::Hardened(5);
        let xsk_5h = xsk_m.derive_child(i_5h);
        let xfvk_5h = xfvk_m.derive_child(i_5h);

        // Cannot derive a hardened child from an ExtendedFullViewingKey
        assert!(xfvk_5h.is_err());
        let xfvk_5h = ExtendedFullViewingKey::from(&xsk_5h);

        let i_7 = ChildIndex::NonHardened(7);
        let xsk_5h_7 = xsk_5h.derive_child(i_7);
        let xfvk_5h_7 = xfvk_5h.derive_child(i_7);

        // But we *can* derive a non-hardened child from a hardened parent
        assert!(xfvk_5h_7.is_ok());
        assert_eq!(ExtendedFullViewingKey::from(&xsk_5h_7), xfvk_5h_7.unwrap());
    }

    #[test]
    fn path() {
        let seed = [0; 32];
        let xsk_m = ExtendedSpendingKey::master(&seed);

        let xsk_5h = xsk_m.derive_child(ChildIndex::Hardened(5));
        assert_eq!(
            ExtendedSpendingKey::from_path(&xsk_m, &[ChildIndex::Hardened(5)]),
            xsk_5h
        );

        let xsk_5h_7 = xsk_5h.derive_child(ChildIndex::NonHardened(7));
        assert_eq!(
            ExtendedSpendingKey::from_path(
                &xsk_m,
                &[ChildIndex::Hardened(5), ChildIndex::NonHardened(7)]
            ),
            xsk_5h_7
        );
    }
}
