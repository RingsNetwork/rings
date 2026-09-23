//! Carry seeds and AEZ keys (#834 D7).
//!
//! For segment `k` the client draws one 32-byte segment seed `σ_k` and derives, by
//! HKDF-SHA256 under distinct domain tags,
//!
//! ```text
//!          KDF₃₂(·, relay j)            KDF₄₈
//!   σ_k ─────────────────────▶ σ_{k,j} ─────────▶ k_{r_{k,j}}      1 ≤ j ≤ s
//!    │     KDF₃₂(·, consumer)           KDF₄₈
//!    └───────────────────────▶ σ_{k,c} ─────────▶ k_{c_k}
//!
//!   KDF₃₂(σ, i) = HKDF-SHA256(salt = D_seed, ikm = σ, info = i)[0, 32)
//!   KDF₄₈(σ′)   = HKDF-SHA256(salt = D_key,  ikm = σ′, info = "aez")[0, 48)
//! ```
//!
//! and places `σ_k` in the producer's layer (`σ_out`), `σ_{k,j}` in relay `r_{k,j}`'s and
//! `σ_{k,c}` in the consumer's (`σ_in`). A hop thus holds the 32-byte seed of exactly the key it
//! applies, and the producer, holding `σ_k`, derives every key of its segment.
//!
//! Law (weak keys). A key whose AEZ subkey `I`, `J` or `L` is `0^128` is rejected
//! ([`rings_aez::KeyError`]); [`OnionSegmentSeed::draw`] re-draws `σ_k` until all `s + 1` keys are
//! strong (each draw fails with probability `≈ (s + 1)·3·2^−128`).

use core::fmt;

use hkdf::Hkdf;
use rand::CryptoRng;
use rand::RngCore;
use rings_aez::Aez;
use rings_aez::KeyError;
use rings_aez::KEY_BYTES;
use sha2::Sha256;
use zeroize::Zeroize;
use zeroize::ZeroizeOnDrop;

use super::hkdf_expand;
use super::ONION_SEGMENT_RELAYS;

/// Width of every carry seed, `|σ| = 32`.
pub const ONION_CARRY_SEED_BYTES: usize = 32;

/// HKDF salt `D_seed` of the per-hop seed derivation `KDF₃₂`.
const SEED_SALT: &[u8] = b"rings-node:onion-carry-seed";

/// HKDF salt `D_key` of the key derivation `KDF₄₈`.
const KEY_SALT: &[u8] = b"rings-node:onion-carry-key";

/// HKDF info label of a relay's seed; followed by the relay index `j` as a big-endian `u64`.
const RELAY_INFO: &[u8] = b"relay";

/// HKDF info label of the consumer's seed.
const CONSUMER_INFO: &[u8] = b"consumer";

/// HKDF info label of the AEZ key.
const KEY_INFO: &[u8] = b"aez";

/// `σ_in`: the seed of the one AEZ key its holder applies to an inbound carry.
///
/// Zeroized on drop.
#[derive(Clone, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub struct OnionCarrySeed([u8; ONION_CARRY_SEED_BYTES]);

/// `σ_k`: the seed of one carry segment, held by its producer.
///
/// Zeroized on drop.
#[derive(Clone, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub struct OnionSegmentSeed([u8; ONION_CARRY_SEED_BYTES]);

/// The per-hop seeds of one segment: what the client places in the layers `σ_in` of the relays
/// `r_{k,1} … r_{k,s}` and of the consumer `c_k`.
pub struct OnionSegmentSeeds {
    /// `σ_{k,j}` for `j = 1 … s`, in the order the relays are visited.
    pub relays: [OnionCarrySeed; ONION_SEGMENT_RELAYS],
    /// `σ_{k,c}`.
    pub consumer: OnionCarrySeed,
}

/// The AEZ key `k = KDF₄₈(σ_in)` of one hop, with its subkeys already checked to be nonzero.
pub struct OnionCarryKey(Aez);

/// Every key of one segment, derived by its producer from `σ_k`; each is strong.
///
/// Invariant: `relays` holds exactly `s` keys, established by its only constructor
/// [`OnionSegmentSeed::keys`].
pub struct OnionSegmentKeys {
    /// `k_{r_{k,j}}` for `j = 1 … s`, in the order the relays are visited.
    relays: Vec<OnionCarryKey>,
    /// `k_{c_k}`.
    consumer: OnionCarryKey,
}

impl OnionCarrySeed {
    /// Wrap seed bytes read from a layer.
    pub const fn new(bytes: [u8; ONION_CARRY_SEED_BYTES]) -> Self {
        Self(bytes)
    }

    /// Return the seed bytes, for the layer encoding.
    pub const fn as_bytes(&self) -> &[u8; ONION_CARRY_SEED_BYTES] {
        &self.0
    }

    /// `KDF₄₈(σ_in)`, keyed into AEZ.
    ///
    /// # Errors
    ///
    /// [`KeyError::ZeroSubkey`] for a weak key; the holder drops the cell.
    pub fn key(&self) -> Result<OnionCarryKey, KeyError> {
        let key =
            hkdf_expand::<KEY_BYTES>(&Hkdf::<Sha256>::new(Some(KEY_SALT), &self.0), &[KEY_INFO]);
        OnionCarryKey::new(&key)
    }
}

impl OnionSegmentSeed {
    /// Wrap seed bytes read from a layer.
    pub const fn new(bytes: [u8; ONION_CARRY_SEED_BYTES]) -> Self {
        Self(bytes)
    }

    /// Return the seed bytes, for the layer encoding.
    pub const fn as_bytes(&self) -> &[u8; ONION_CARRY_SEED_BYTES] {
        &self.0
    }

    /// A uniform seed, as a relay's unused `σ_out` (D7: every layer carries one outbound seed).
    pub fn random(rng: &mut (impl CryptoRng + RngCore)) -> Self {
        let mut seed = Self([0; ONION_CARRY_SEED_BYTES]);
        rng.fill_bytes(&mut seed.0);
        seed
    }

    /// Draw `σ_k` for a new segment, re-drawing until every derived key is strong.
    ///
    /// ```text
    /// loop: σ ← U({0,1}^256);  keys(σ) = Ok(K) ⇒ return (σ, K)
    /// ```
    pub fn draw(rng: &mut (impl CryptoRng + RngCore)) -> (Self, OnionSegmentKeys) {
        loop {
            let seed = Self::random(rng);
            if let Ok(keys) = seed.keys() {
                break (seed, keys);
            }
        }
    }

    /// `(KDF₃₂(σ_k, relay j))_{j=1…s}` and `KDF₃₂(σ_k, consumer)`.
    pub fn seeds(&self) -> OnionSegmentSeeds {
        let kdf = Hkdf::<Sha256>::new(Some(SEED_SALT), &self.0);
        OnionSegmentSeeds {
            relays: core::array::from_fn(|index| {
                let relay = (index as u64).saturating_add(1).to_be_bytes();
                OnionCarrySeed(*hkdf_expand::<ONION_CARRY_SEED_BYTES>(&kdf, &[
                    RELAY_INFO,
                    relay.as_slice(),
                ]))
            }),
            consumer: OnionCarrySeed(*hkdf_expand::<ONION_CARRY_SEED_BYTES>(&kdf, &[
                CONSUMER_INFO,
            ])),
        }
    }

    /// Every key of the segment, as its producer applies them.
    ///
    /// # Errors
    ///
    /// [`KeyError::ZeroSubkey`] if any of the `s + 1` keys is weak.
    pub fn keys(&self) -> Result<OnionSegmentKeys, KeyError> {
        let seeds = self.seeds();
        Ok(OnionSegmentKeys {
            relays: seeds
                .relays
                .iter()
                .map(OnionCarrySeed::key)
                .collect::<Result<Vec<_>, _>>()?,
            consumer: seeds.consumer.key()?,
        })
    }
}

impl OnionCarryKey {
    /// Key AEZ with a derived 48-byte key `K = I ‖ J ‖ L`.
    ///
    /// # Errors
    ///
    /// [`KeyError::ZeroSubkey`] if `I`, `J` or `L` is `0^128`: the weak-key check every derived
    /// key passes through.
    pub fn new(key: &[u8; KEY_BYTES]) -> Result<Self, KeyError> {
        Aez::new(key).map(Self)
    }

    /// The keyed cipher.
    pub(super) const fn aez(&self) -> &Aez {
        &self.0
    }
}

impl OnionSegmentKeys {
    /// `k_{r_{k,j}}` for `j = 1 … s`, in visiting order.
    pub fn relays(&self) -> &[OnionCarryKey] {
        self.relays.as_slice()
    }

    /// `k_{c_k}`.
    pub const fn consumer(&self) -> &OnionCarryKey {
        &self.consumer
    }
}

impl fmt::Debug for OnionCarrySeed {
    /// Redacts the seed: it is key material.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OnionCarrySeed(..)")
    }
}

impl fmt::Debug for OnionSegmentSeed {
    /// Redacts the seed: it is key material.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OnionSegmentSeed(..)")
    }
}
