//! The fixed-length Sphinx header `χ = (α, β, γ)` (#834 L5′, D6″; paper Def. fixed-length header).
//!
//! Key transport is one blinded secp256k1 element per cell. Hop `i` holds the delegatee key
//! `d_i` with `pk_i = d_i·G`; the client draws `x ∈ Z_n^*` and, over `G ∖ {O}` and `Z_n^*` (so no
//! step can meet `O`, see `rings_core::ecc::prime_order`),
//!
//! ```text
//! x_1 = x,  α_i = x_i·G,  K_i = x(x_i·pk_i) = x(d_i·α_i)     (ECDH; the hop computes d_i·α_i)
//! (ρ-key, μ-key, blind) = HKDF-SHA256(salt = D, ikm = SEC1(α_i) ‖ K_i; "prg", "mac", "blind")
//! z_i = blind mod n ∈ Z_n^*     (the blinding factor; 64-byte wide reduction; 0 rejected)
//! x_{i+1} = x_i·z_i,  α_{i+1} = z_i·α_i
//! ρ_i = ChaCha20_{ρ-key}(0^{(Ĥ+1)ℓ}),   γ_i = HMAC-SHA256_{μ-key}(b ‖ β_i)[0, 16)
//! ```
//!
//! where `b` is the loop class, bound as its one-byte label (#834 H1: a cell relabelled to another
//! class fails `γ` at the next honest hop); `b` denotes the class only. `SEC1(α_i)` in the key
//! schedule binds the sign of `α_i`: `−α_i` has the same `K_i` but other keys. With `β` as `Ĥ`
//! blocks of `ℓ` bytes and `ρ[a, b)` the blocks `a … b − 1`, the routing information is
//!
//! ```text
//! φ_0 = ε,   φ_i = (φ_{i−1} ‖ 0^ℓ) ⊕ ρ_i[Ĥ−i+1, Ĥ+1)                               1 ≤ i < H
//! β_H = ((λ_H ‖ R) ⊕ ρ_H[0, Ĥ−H+1)) ‖ φ_{H−1}                                     R uniform
//! β_i = (λ_i ‖ β_{i+1}[0, Ĥ−1)) ⊕ ρ_i[0, Ĥ)                                       1 ≤ i < H
//! ```
//!
//! and hop `i` peels `(β_i ‖ 0^ℓ) ⊕ ρ_i = λ_i ‖ β_{i+1}`, where `λ_i` carries `γ_{i+1}`. At
//! `i = H` the next header goes to the client, position `H + 1` (D6′): `γ_{H+1} = t_⋄` is the
//! client's loop tag, drawn by the client and forwarded by the guard ([`OnionHeader::loop_tag`]).
//!
//! Laws:
//!
//! - **Correctness** (Prop. header correctness). For `H ≤ Ĥ` and `1 ≤ i ≤ H`,
//!   [`OnionHeader::peel`] of `χ_i` under `d_i` yields `λ_i` and `χ_{i+1}`, every `γ_i` verifies,
//!   and `χ_{H+1}` carries `t_⋄`. By induction down from `H`, the last `i − 1` blocks of `β_i` are
//!   `φ_{i−1}`, so the block that peeling appends, `ρ_i[Ĥ, Ĥ+1)`, is the last block of `φ_i`,
//!   hence of `β_{i+1}`.
//! - **Length** (L5′). `|χ| = 33 + Ĥℓ + 16` for every `H` and `i`: the header type has fixed
//!   width.
//! - **Rejection.** `peel` rejects an `α` that is not a curve point before any ECDH (a uniformly
//!   random 33-byte string, e.g. link cover under F = 0, is rejected there with probability
//!   `≈ 99.6 %`), and verifies `γ` before any layer is decoded. Both are one outcome,
//!   [`OnionPeelError::Invalid`]: the admission step charges a cell rejected at either point
//!   the same `u(b)` (#834), so a flood of invalid `α` is not free.

use chacha20::cipher::KeyIvInit;
use chacha20::cipher::StreamCipher;
use chacha20::ChaCha20;
use hmac::digest::Key;
use hmac::Hmac;
use hmac::Mac;
use rand::CryptoRng;
use rand::RngCore;
use rings_core::delegation::DelegateeKey;
use rings_core::ecc::prime_order::NonIdentityPoint;
use rings_core::ecc::prime_order::NonZeroScalar;
use rings_core::ecc::prime_order::SHARED_SECRET_BYTES;
use rings_core::ecc::prime_order::WIDE_SCALAR_BYTES;
use rings_core::ecc::PublicKey;
use rings_core::ecc::Secp256k1;
use sha2::Sha256;
use subtle::Choice;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use super::class::OnionLoopClass;
use super::hkdf_expand;
use super::hkdf_extract;
use super::layer::OnionLayer;
use super::layer::OnionLayerError;
use super::layer::ONION_LAYER_BYTES;
use super::xor_in_place;
use crate::onion::loop_shape::MAX_ONION_LOOP_HOPS;

/// `|α|`, a compressed SEC1 secp256k1 point.
const ONION_GROUP_ELEMENT_BYTES: usize = 33;

/// `|γ|`, the truncated header MAC.
pub(crate) const ONION_HEADER_MAC_BYTES: usize = 16;

/// `|β| = Ĥ·ℓ`, the routing information: exactly `Ĥ` layer slots whatever the loop's own `H`
/// (L5′).
pub(crate) const ONION_HEADER_ROUTING_BYTES: usize = MAX_ONION_LOOP_HOPS * ONION_LAYER_BYTES;

/// `|χ| = |α| + |β| + |γ|`.
pub(crate) const ONION_HEADER_BYTES: usize =
    ONION_GROUP_ELEMENT_BYTES + ONION_HEADER_ROUTING_BYTES + ONION_HEADER_MAC_BYTES;

/// HKDF salt `D` of the per-hop key schedule.
const HEADER_KDF_SALT: &[u8] = b"rings-node:onion-sphinx-header";

/// HKDF info label of the stream-cipher key.
const STREAM_INFO: &[u8] = b"prg";

/// HKDF info label of the MAC key.
const MAC_INFO: &[u8] = b"mac";

/// HKDF info label of the blinding factor.
const BLIND_INFO: &[u8] = b"blind";

/// Width of the stream-cipher key.
const STREAM_KEY_BYTES: usize = 32;

/// Width of the MAC key: one SHA-256 block, so HMAC uses it without hashing.
const MAC_KEY_BYTES: usize = 64;

/// One `ℓ`-byte block of `β` or `ρ`: one layer slot.
type Block = [u8; ONION_LAYER_BYTES];

/// `β`, `Ĥ` layer slots.
type Routing = [Block; MAX_ONION_LOOP_HOPS];

/// `ρ`, the pad of `β ‖ 0^ℓ`: `Ĥ + 1` blocks.
type Stream = [Block; MAX_ONION_LOOP_HOPS + 1];

/// `γ`, a truncated HMAC-SHA256; compared only in constant time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OnionHeaderMac([u8; ONION_HEADER_MAC_BYTES]);

/// The client's loop tag `t_⋄ = γ_{H+1}` (#834 D6′, H2), uniform, drawn by
/// [`OnionHeader::build`].
///
/// `Eq` and `Hash` are sound here although `γ` compares in constant time: the client must map
/// `t_⋄` to its loop state (D6′), and the only other party that sees `γ_{H+1}` is the guard,
/// which already knows it, so variable-time equality reveals it to nobody.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OnionLoopTag([u8; ONION_HEADER_MAC_BYTES]);

/// The Sphinx header `χ = (α, β, γ)`, exactly `|χ|` bytes whatever the loop length.
#[derive(Clone, Debug)]
pub(crate) struct OnionHeader {
    /// `α`, SEC1 compressed; decoded when peeled.
    alpha: PublicKey<ONION_GROUP_ELEMENT_BYTES>,
    /// `β`.
    routing: Routing,
    /// `γ = MAC(b ‖ β)`.
    mac: OnionHeaderMac,
}

/// One position of a loop as the client builds it: the hop's key and its layer.
pub(crate) struct OnionHeaderHop {
    /// `pk_i`, the delegatee key of the hop at this position.
    pub(crate) public_key: PublicKey<33>,
    /// `λ_i` without `γ_{i+1}`, which the builder computes.
    pub(crate) layer: OnionLayer,
}

/// One validated position: `pk_i ∈ G ∖ {O}` and `λ_i`.
struct OnionRoutePosition {
    /// `pk_i`.
    recipient: NonIdentityPoint<Secp256k1>,
    /// `λ_i` without `γ_{i+1}`.
    layer: OnionLayer,
}

/// The positions `1 … H` of a loop, `1 ≤ H ≤ Ĥ`, every key a point: the outer positions and the
/// last one.
pub(crate) struct OnionHeaderRoute {
    /// Positions `1 … H − 1`, in visiting order.
    outer: Vec<OnionRoutePosition>,
    /// Position `H`, the guard facing the client.
    last: OnionRoutePosition,
}

/// The result of peeling one header: this hop's layer and the header it forwards.
pub(crate) struct OnionPeeledHeader {
    /// `λ_i`.
    pub(crate) layer: OnionLayer,
    /// `χ_{i+1}`.
    pub(crate) next: OnionHeader,
}

/// Why a loop's positions are not a route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionHeaderRouteError {
    /// The loop has no position or more than `Ĥ`.
    #[error("loop of {0} positions is outside 1..={MAX_ONION_LOOP_HOPS}")]
    HopCount(usize),
    /// A hop's public key is not a secp256k1 point.
    #[error("onion hop public key is not a secp256k1 point")]
    PublicKey,
}

/// The blinding factor `z_i` derived for a position is `0 mod n` (probability `2^−256`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("onion header blinding factor is zero")]
pub(crate) struct OnionBlindingError;

/// Why a hop did not peel a header.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionPeelError {
    /// `α` is not a secp256k1 point, or `γ` does not verify: one outcome, charged alike.
    #[error("onion header is invalid")]
    Invalid,
    /// The derived blinding factor is zero.
    #[error(transparent)]
    Blinding(#[from] OnionBlindingError),
    /// The peeled layer is not a layer encoding.
    #[error(transparent)]
    Layer(#[from] OnionLayerError),
}

/// The per-hop secrets derived from `(α_i, K_i)`, zeroized on drop.
struct OnionHopSecrets {
    /// `ρ_i`.
    stream: Zeroizing<Stream>,
    /// The HMAC key of `γ_i`.
    mac_key: Zeroizing<[u8; MAC_KEY_BYTES]>,
    /// `z_i`.
    blinding: NonZeroScalar<Secp256k1>,
}

impl OnionHeaderMac {
    /// Wrap MAC bytes.
    pub(crate) const fn new(bytes: [u8; ONION_HEADER_MAC_BYTES]) -> Self {
        Self(bytes)
    }

    /// Return the MAC bytes.
    pub(crate) const fn to_bytes(self) -> [u8; ONION_HEADER_MAC_BYTES] {
        self.0
    }
}

impl ConstantTimeEq for OnionHeaderMac {
    /// Equality in time independent of the MAC bytes.
    fn ct_eq(&self, other: &Self) -> Choice {
        self.0.ct_eq(&other.0)
    }
}

impl OnionHeaderRoute {
    /// Accept the positions `hops = (pk_i, λ_i)_{i=1…H}` of a loop, validating every key once.
    ///
    /// # Errors
    ///
    /// [`OnionHeaderRouteError::HopCount`] unless `1 ≤ H ≤ Ĥ`, and
    /// [`OnionHeaderRouteError::PublicKey`] for a key that is not a curve point.
    pub(crate) fn new(hops: Vec<OnionHeaderHop>) -> Result<Self, OnionHeaderRouteError> {
        let count = hops.len();
        let mut positions = hops
            .into_iter()
            .map(|hop| {
                NonIdentityPoint::try_from(hop.public_key)
                    .map(|recipient| OnionRoutePosition {
                        recipient,
                        layer: hop.layer,
                    })
                    .map_err(|_| OnionHeaderRouteError::PublicKey)
            })
            .collect::<Result<Vec<_>, _>>()?;
        positions
            .pop()
            .filter(|_| count <= MAX_ONION_LOOP_HOPS)
            .map(|last| Self {
                outer: positions,
                last,
            })
            .ok_or(OnionHeaderRouteError::HopCount(count))
    }
}

impl OnionHopSecrets {
    /// The key schedule of one position from `α_i` (encoded) and `K_i`.
    ///
    /// # Errors
    ///
    /// [`OnionBlindingError`] if the blinding material reduces to `0 mod n`; the client then
    /// builds again with fresh randomness, a hop drops the cell.
    fn derive(
        alpha: &PublicKey<ONION_GROUP_ELEMENT_BYTES>,
        shared: &[u8; SHARED_SECRET_BYTES],
    ) -> Result<Self, OnionBlindingError> {
        let kdf = hkdf_extract(HEADER_KDF_SALT, &[alpha.0.as_slice(), shared.as_slice()]);
        let blinding =
            NonZeroScalar::from_wide_bytes(&hkdf_expand::<WIDE_SCALAR_BYTES>(&kdf, &[BLIND_INFO]))
                .ok_or(OnionBlindingError)?;
        let mut stream = Zeroizing::new([[0; ONION_LAYER_BYTES]; MAX_ONION_LOOP_HOPS + 1]);
        ChaCha20::new(
            &(*hkdf_expand::<STREAM_KEY_BYTES>(&kdf, &[STREAM_INFO])).into(),
            &[0; 12].into(),
        )
        .apply_keystream(stream.as_flattened_mut());
        Ok(Self {
            stream,
            mac_key: hkdf_expand(&kdf, &[MAC_INFO]),
            blinding,
        })
    }

    /// `γ = HMAC-SHA256_μ(b ‖ β)[0, 16)`: truncation keeps the leftmost 16 of the 32 bytes.
    fn mac(&self, class: OnionLoopClass, routing: &Routing) -> OnionHeaderMac {
        let mut key = Zeroizing::new(Key::<Hmac<Sha256>>::default());
        key.iter_mut()
            .zip(self.mac_key.iter())
            .for_each(|(slot, byte)| *slot = *byte);
        let mut mac = <Hmac<Sha256> as Mac>::new(&key);
        mac.update(&class.mac_label());
        mac.update(routing.as_flattened());
        let mut truncated = [0; ONION_HEADER_MAC_BYTES];
        truncated
            .iter_mut()
            .zip(mac.finalize().into_bytes())
            .for_each(|(slot, byte)| *slot = byte);
        OnionHeaderMac(truncated)
    }

    /// `(β ‖ 0^ℓ) ⊕ ρ` for the hop, or `(λ ‖ β′) ⊕ ρ[0, Ĥ)` for the client: `blocks ⊕ ρ` over
    /// the common prefix.
    fn mask(&self, blocks: &mut [Block]) {
        blocks
            .iter_mut()
            .zip(self.stream.iter())
            .for_each(|(block, mask)| xor_in_place(block.iter_mut(), mask.iter()));
    }
}

impl OnionHeader {
    /// The fields `α ‖ β ‖ γ` of an `|χ|`-byte string, `None` for any other width; `α` is
    /// decoded as a point only when the header is peeled. The cell parser is the one caller.
    pub(super) fn decode(bytes: &[u8]) -> Option<Self> {
        let (alpha, rest) = bytes.split_first_chunk()?;
        let (routing, mac) = rest.split_last_chunk()?;
        let (blocks, []) = routing.as_chunks() else {
            return None;
        };
        Some(Self {
            alpha: PublicKey(*alpha),
            routing: Routing::try_from(blocks).ok()?,
            mac: OnionHeaderMac(*mac),
        })
    }

    /// Append the header's encoding `α ‖ β ‖ γ`, exactly `|χ|` bytes, to `bytes`.
    pub(super) fn encode_into(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(self.alpha.0.as_slice());
        bytes.extend_from_slice(self.routing.as_flattened());
        bytes.extend_from_slice(self.mac.0.as_slice());
    }

    /// The header's encoding `α ‖ β ‖ γ`, exactly `|χ|` bytes.
    pub(super) fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(ONION_HEADER_BYTES);
        self.encode_into(&mut bytes);
        bytes
    }

    /// `t_⋄ = γ_{H+1}`: the loop tag, read by the client from the header the guard forwards.
    pub(super) const fn loop_tag(&self) -> OnionLoopTag {
        OnionLoopTag(self.mac.0)
    }

    /// Build `χ_1` for a loop of class `b`, drawing `x`, `R` and the client tag `t_⋄` from
    /// `rng`; returns the header and `t_⋄`.
    ///
    /// ```text
    /// x ← Z_n^*,  t_⋄ ← {0,1}^128
    /// for i = 1 … H:     α_i = x_i·G,  K_i = x(x_i·pk_i),  secrets_i,  x_{i+1} = x_i·z_i
    /// for i = 1 … H−1:   φ_i = (φ_{i−1} ‖ 0^ℓ) ⊕ ρ_i[Ĥ−i+1, Ĥ+1)      (tail-aligned blocks)
    /// β_H = ((λ_H(γ_{H+1} = t_⋄) ‖ R) ⊕ ρ_H[0, Ĥ)), then its last H−1 blocks := φ_{H−1}
    /// γ_H = MAC_H(b ‖ β_H)
    /// for i = H−1 … 1:   β_i = (λ_i(γ_{i+1}) ‖ β_{i+1}[0, Ĥ−1)) ⊕ ρ_i[0, Ĥ),  γ_i = MAC_i(b ‖ β_i)
    /// return ((α_1, β_1, γ_1), t_⋄)
    /// ```
    ///
    /// # Errors
    ///
    /// [`OnionBlindingError`] for a zero blinding factor (probability `2^−256`), after which the
    /// client builds again with fresh randomness.
    pub(super) fn build(
        route: &OnionHeaderRoute,
        class: OnionLoopClass,
        rng: &mut (impl CryptoRng + RngCore),
    ) -> Result<(Self, OnionLoopTag), OnionBlindingError> {
        let mut tag = OnionLoopTag([0; ONION_HEADER_MAC_BYTES]);
        rng.fill_bytes(&mut tag.0);
        let (exponent, outer) = route.outer.iter().try_fold(
            (
                NonZeroScalar::random_with_rng(&mut *rng),
                Vec::with_capacity(route.outer.len()),
            ),
            |(exponent, mut schedule), position| {
                let (alpha, secrets) = Self::position(position, &exponent)?;
                let next = &exponent * &secrets.blinding;
                schedule.push((alpha, secrets));
                Ok::<_, OnionBlindingError>((next, schedule))
            },
        )?;
        let (alpha, secrets) = Self::position(&route.last, &exponent)?;
        let filler = outer
            .iter()
            .fold(Vec::<Block>::new(), |mut filler, (_, secrets)| {
                filler.push([0; ONION_LAYER_BYTES]);
                filler
                    .iter_mut()
                    .rev()
                    .zip(secrets.stream.iter().rev())
                    .for_each(|(block, mask)| xor_in_place(block.iter_mut(), mask.iter()));
                filler
            });
        let mut routing: Routing = [[0; ONION_LAYER_BYTES]; MAX_ONION_LOOP_HOPS];
        rng.fill_bytes(routing.as_flattened_mut());
        let [first, ..] = &mut routing;
        *first = *route.last.layer.encode(&OnionHeaderMac(tag.0));
        secrets.mask(&mut routing);
        routing
            .iter_mut()
            .rev()
            .zip(filler.iter().rev())
            .for_each(|(block, filler)| *block = *filler);
        let innermost = Self {
            alpha,
            mac: secrets.mac(class, &routing),
            routing,
        };
        let header = route.outer.iter().zip(outer.iter()).rev().fold(
            innermost,
            |next, (position, (alpha, secrets))| {
                let mut routing = next.routing;
                routing.rotate_right(1);
                let [first, ..] = &mut routing;
                *first = *position.layer.encode(&next.mac);
                secrets.mask(&mut routing);
                Self {
                    alpha: *alpha,
                    mac: secrets.mac(class, &routing),
                    routing,
                }
            },
        );
        Ok((header, tag))
    }

    /// The client's view of one position under exponent `x_i`: `α_i` and the secrets of
    /// `K_i = x(x_i·pk_i)`.
    fn position(
        position: &OnionRoutePosition,
        exponent: &NonZeroScalar<Secp256k1>,
    ) -> Result<(PublicKey<ONION_GROUP_ELEMENT_BYTES>, OnionHopSecrets), OnionBlindingError> {
        let alpha = PublicKey::from(&NonIdentityPoint::generator_mul(exponent));
        let secrets = OnionHopSecrets::derive(&alpha, &position.recipient.shared_secret(exponent))?;
        Ok((alpha, secrets))
    }

    /// Peel `χ_i` of a class-`b` cell under the hop's delegatee key: `(λ_i, χ_{i+1})`. The class
    /// is the observed cell length, supplied by the cell parser alone.
    ///
    /// ```text
    /// α_i ∈ G ∖ {O} ?                        else Invalid   (decoded before any ECDH)
    /// K_i = x(d_i·α_i);  secrets_i            else Blinding  (z_i ≡ 0, probability 2^−256)
    /// γ_i = MAC_i(b ‖ β_i) ?                 else Invalid   (constant time, before decoding)
    /// λ_i ‖ β_{i+1} = (β_i ‖ 0^ℓ) ⊕ ρ_i;  decode λ_i = (layer, γ_{i+1})   else Layer
    /// α_{i+1} = z_i·α_i                      (∈ G ∖ {O} by type)
    /// ```
    ///
    /// # Errors
    ///
    /// [`OnionPeelError::Invalid`], [`OnionPeelError::Blinding`] and [`OnionPeelError::Layer`],
    /// in that order of checking.
    pub(super) fn peel(
        &self,
        class: OnionLoopClass,
        key: &DelegateeKey,
    ) -> Result<OnionPeeledHeader, OnionPeelError> {
        let alpha = NonIdentityPoint::<Secp256k1>::try_from(self.alpha)
            .map_err(|_| OnionPeelError::Invalid)?;
        let secrets = OnionHopSecrets::derive(&self.alpha, &key.diffie_hellman(&alpha))?;
        if !bool::from(secrets.mac(class, &self.routing).ct_eq(&self.mac)) {
            return Err(OnionPeelError::Invalid);
        }
        let mut blocks = Zeroizing::new([[0; ONION_LAYER_BYTES]; MAX_ONION_LOOP_HOPS + 1]);
        blocks
            .iter_mut()
            .zip(self.routing.iter())
            .for_each(|(block, routing)| *block = *routing);
        secrets.mask(blocks.as_mut_slice());
        let [layer, routing @ ..] = *blocks;
        let (layer, mac) = OnionLayer::decode(Zeroizing::new(layer).as_slice())?;
        Ok(OnionPeeledHeader {
            layer,
            next: Self {
                alpha: PublicKey::from(&(&alpha * &secrets.blinding)),
                routing,
                mac,
            },
        })
    }
}
