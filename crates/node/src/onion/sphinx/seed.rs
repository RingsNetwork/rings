//! Carry seeds and AEZ keys (#834 D7).
//!
//! For segment `k` the client draws one 32-byte segment seed `σ_k` and derives, by
//! HKDF-SHA256 under distinct domain tags,
//!
//! ```text
//!          KDF₃₂(·, relay j)            KDF₄₈
//!   σ_k ─────────────────────▶ σ_{k,j} ─────────▶ k_{r_{k,j}}      j ∈ {1, …, s}
//!    │     KDF₃₂(·, consumer)           KDF₄₈
//!    └───────────────────────▶ σ_{k,c} ─────────▶ k_{c_k}
//!
//!   KDF₃₂(σ, i) = HKDF-SHA256(salt = D_seed, ikm = σ, info = i)[0, 32)
//!   KDF₄₈(σ′)   = HKDF-SHA256(salt = D_key,  ikm = σ′, info = "aez")[0, 48)
//! ```
//!
//! and places `σ_k` in the producer's layer (`σ_out`), `σ_{k,j}` in relay `r_{k,j}`'s and
//! `σ_{k,c}` in the consumer's (`σ_in`). A hop thus holds the 32-byte seed of exactly the key it
//! applies, and the producer, holding `σ_k`, derives every key of its segment. A key exists only
//! as `KDF₄₈` of a seed: [`OnionCarrySeed::key`] is the one constructor of [`OnionCarryKey`].
//!
//! Law (weak keys). A key whose AEZ subkey `I`, `J` or `L` is `0^128` is rejected
//! ([`rings_aez::KeyError`]); [`OnionSegmentSeed::draw`] re-draws `σ_k` while a derived key is
//! weak (each draw fails with probability `≈ (s + 1)·3·2^−128`).

use core::fmt;

use hkdf::Hkdf;
use rand::CryptoRng;
use rand::RngCore;
use rings_aez::Aez;
use rings_aez::KeyError;
use rings_aez::KEY_BYTES;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::Zeroize;
use zeroize::ZeroizeOnDrop;

use super::hkdf_expand;
use super::ONION_SEGMENT_RELAYS;

/// Width of every carry seed, `|σ| = 32`.
pub(crate) const ONION_CARRY_SEED_BYTES: usize = 32;

/// Draws of `σ_k` before [`OnionSegmentSeed::draw`] fails closed: a draw is weak with probability
/// `≈ 9·2^−128 ≈ 2^−124.8`, so four consecutive weak draws occur with probability `≈ 2^−499`
/// unless the RNG is broken.
const SEGMENT_SEED_DRAWS: usize = 4;

/// The relay indices `j = 1 … s`; the array length is checked against `s` at compile time.
const RELAY_INDICES: [u8; ONION_SEGMENT_RELAYS] = [1, 2];

/// HKDF salt `D_seed` of the per-hop seed derivation `KDF₃₂`.
const SEED_SALT: &[u8] = b"rings-node:onion-carry-seed";

/// HKDF salt `D_key` of the key derivation `KDF₄₈`.
const KEY_SALT: &[u8] = b"rings-node:onion-carry-key";

/// HKDF info label of a relay's seed; followed by the one-byte relay index `j`.
const RELAY_INFO: &[u8] = b"relay";

/// HKDF info label of the consumer's seed.
const CONSUMER_INFO: &[u8] = b"consumer";

/// HKDF info label of the AEZ key.
const KEY_INFO: &[u8] = b"aez";

/// `σ_in`: the seed of the one AEZ key its holder applies to an inbound carry.
///
/// Zeroized on drop; compared in constant time.
#[derive(Zeroize, ZeroizeOnDrop)]
pub(crate) struct OnionCarrySeed([u8; ONION_CARRY_SEED_BYTES]);

/// `σ_k`: the seed of one carry segment, held by its producer.
///
/// Zeroized on drop; compared in constant time.
#[derive(Zeroize, ZeroizeOnDrop)]
pub(crate) struct OnionSegmentSeed([u8; ONION_CARRY_SEED_BYTES]);

/// The per-hop seeds of one segment: what the client places in the layers `σ_in` of the relays
/// `r_{k,1} … r_{k,s}` and of the consumer `c_k`.
pub(crate) struct OnionSegmentSeeds {
    /// `σ_{k,j}` for `j = 1 … s`, in the order the relays are visited.
    pub(crate) relays: [OnionCarrySeed; ONION_SEGMENT_RELAYS],
    /// `σ_{k,c}`.
    pub(crate) consumer: OnionCarrySeed,
}

/// The AEZ key `k = KDF₄₈(σ_in)` of one hop; its subkeys are nonzero.
pub(crate) struct OnionCarryKey(Aez);

/// Every key of one segment, derived by its producer from `σ_k`; each is strong.
pub(crate) struct OnionSegmentKeys {
    /// `k_{r_{k,j}}` for `j = 1 … s`, in the order the relays are visited.
    relays: [OnionCarryKey; ONION_SEGMENT_RELAYS],
    /// `k_{c_k}`.
    consumer: OnionCarryKey,
}

impl OnionCarrySeed {
    /// Wrap seed bytes read from a layer.
    pub(crate) const fn new(bytes: [u8; ONION_CARRY_SEED_BYTES]) -> Self {
        Self(bytes)
    }

    /// Return the seed bytes, for the layer encoding.
    pub(crate) const fn as_bytes(&self) -> &[u8; ONION_CARRY_SEED_BYTES] {
        &self.0
    }

    /// `KDF₄₈(σ_in)`, keyed into AEZ: the only way to obtain an [`OnionCarryKey`].
    ///
    /// # Errors
    ///
    /// [`KeyError::ZeroSubkey`] for a weak key; the holder drops the cell.
    pub(crate) fn key(&self) -> Result<OnionCarryKey, KeyError> {
        let key =
            hkdf_expand::<KEY_BYTES>(&Hkdf::<Sha256>::new(Some(KEY_SALT), &self.0), &[KEY_INFO]);
        Aez::new(&key).map(OnionCarryKey)
    }
}

impl OnionSegmentSeed {
    /// Wrap seed bytes read from a layer.
    pub(crate) const fn new(bytes: [u8; ONION_CARRY_SEED_BYTES]) -> Self {
        Self(bytes)
    }

    /// Return the seed bytes, for the layer encoding.
    pub(crate) const fn as_bytes(&self) -> &[u8; ONION_CARRY_SEED_BYTES] {
        &self.0
    }

    /// A uniform seed, as a relay's unused `σ_out` (D7: every layer carries one outbound seed).
    pub(crate) fn random(rng: &mut (impl CryptoRng + RngCore)) -> Self {
        let mut seed = Self([0; ONION_CARRY_SEED_BYTES]);
        rng.fill_bytes(&mut seed.0);
        seed
    }

    /// Draw `σ_k` for a new segment, re-drawing while a derived key is weak: [`first_strong`]
    /// over uniform seeds and [`Self::keys`].
    ///
    /// # Errors
    ///
    /// The last [`KeyError`] when every draw was weak.
    pub(crate) fn draw(
        rng: &mut (impl CryptoRng + RngCore),
    ) -> Result<(Self, OnionSegmentKeys), KeyError> {
        first_strong(|| Self::random(rng), Self::keys)
    }

    /// `(KDF₃₂(σ_k, relay j))_{j=1…s}` and `KDF₃₂(σ_k, consumer)`.
    pub(crate) fn seeds(&self) -> OnionSegmentSeeds {
        let kdf = Hkdf::<Sha256>::new(Some(SEED_SALT), &self.0);
        OnionSegmentSeeds {
            relays: RELAY_INDICES.map(|relay| {
                OnionCarrySeed(*hkdf_expand::<ONION_CARRY_SEED_BYTES>(&kdf, &[
                    RELAY_INFO,
                    &[relay],
                ]))
            }),
            consumer: OnionCarrySeed(*hkdf_expand::<ONION_CARRY_SEED_BYTES>(&kdf, &[
                CONSUMER_INFO,
            ])),
        }
    }

    /// Every key of the segment, as its producer applies them.
    ///
    /// The relay pattern `[first, second]` pins `s = 2` at compile time.
    ///
    /// # Errors
    ///
    /// [`KeyError::ZeroSubkey`] if any of the `s + 1` keys is weak.
    pub(crate) fn keys(&self) -> Result<OnionSegmentKeys, KeyError> {
        let seeds = self.seeds();
        let [first, second] = seeds.relays.each_ref().map(OnionCarrySeed::key);
        Ok(OnionSegmentKeys {
            relays: [first?, second?],
            consumer: seeds.consumer.key()?,
        })
    }
}

/// The first of at most `SEGMENT_SEED_DRAWS` drawn candidates whose derivation succeeds, with
/// its derivation; otherwise the last failure.
///
/// ```text
/// attempt = draw ≫= λc. (c, derive c)
/// first_strong = attempt <|> attempt <|> …   (SEGMENT_SEED_DRAWS times; <|> keeps the first Ok)
/// ```
///
/// Pure in its two arguments, so the fail-closed path is testable with an injected `derive`.
pub(super) fn first_strong<C, K>(
    mut draw: impl FnMut() -> C,
    derive: impl Fn(&C) -> Result<K, KeyError>,
) -> Result<(C, K), KeyError> {
    let mut attempt = || {
        let candidate = draw();
        derive(&candidate).map(|derived| (candidate, derived))
    };
    (1..SEGMENT_SEED_DRAWS).fold(attempt(), |drawn, _| drawn.or_else(|_| attempt()))
}

impl OnionCarryKey {
    /// The keyed cipher.
    pub(super) const fn aez(&self) -> &Aez {
        &self.0
    }
}

impl OnionSegmentKeys {
    /// `k_{r_{k,j}}` for `j = 1 … s`, in visiting order.
    pub(crate) const fn relays(&self) -> &[OnionCarryKey; ONION_SEGMENT_RELAYS] {
        &self.relays
    }

    /// `k_{c_k}`.
    pub(crate) const fn consumer(&self) -> &OnionCarryKey {
        &self.consumer
    }
}

impl PartialEq for OnionCarrySeed {
    /// Equality in time independent of the seed bytes.
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl PartialEq for OnionSegmentSeed {
    /// Equality in time independent of the seed bytes.
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl Eq for OnionCarrySeed {}

impl Eq for OnionSegmentSeed {}

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
