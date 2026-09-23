//! The fixed-length Sphinx header `χ = (α, β, γ)` (#834 L5′, D6″; paper Def. fixed-length header).
//!
//! Key transport is one blinded secp256k1 element per cell. Hop `i` holds the delegatee key
//! `d_i` with `pk_i = d_i·G`; the client draws `x ← Z_n^*` and
//!
//! ```text
//! x_1 = x,  α_i = x_i·G,  K_i = x_i·pk_i = d_i·α_i          (ECDH; the hop computes d_i·α_i)
//! (ρ-key, μ-key, blind) = HKDF-SHA256(salt = D, ikm = SEC1(α_i) ‖ x(K_i); "prg", "mac", "blind")
//! b_i = (blind mod n) ∈ Z_n^*    (64-byte wide reduction, zero rejected)
//! x_{i+1} = x_i·b_i,  α_{i+1} = b_i·α_i
//! ρ_i = ChaCha20_{ρ-key}(0^{(Ĥ+1)ℓ}),   MAC_{K_i} = HMAC-SHA256_{μ-key} truncated to 16 bytes
//! ```
//!
//! With `ρ[a, b)` the byte range `[aℓ, bℓ)`, the routing information `β` of `Ĥ·ℓ` bytes is
//!
//! ```text
//! φ_0 = ε,   φ_i = (φ_{i−1} ‖ 0^ℓ) ⊕ ρ_i[Ĥ−i+1, Ĥ+1)                               1 ≤ i < H
//! β_H = ((λ_H ‖ R) ⊕ ρ_H[0, Ĥ−H+1)) ‖ φ_{H−1}                                     R uniform
//! β_i = (λ_i ‖ β_{i+1}[0, Ĥ−1)) ⊕ ρ_i[0, Ĥ),   γ_i = MAC_{K_i}(β_i)               1 ≤ i ≤ H
//! ```
//!
//! and hop `i` peels `(β_i ‖ 0^ℓ) ⊕ ρ_i = λ_i ‖ β_{i+1}`, where `λ_i` carries `γ_{i+1}` (at `i = H`,
//! the client position `H + 1` of D6′, `γ_{H+1}` is uniform and ignored).
//!
//! Laws:
//!
//! - **Correctness** (Prop. header correctness). For `H ≤ Ĥ` and `1 ≤ i ≤ H`, [`OnionHeader::peel`]
//!   of `χ_i` under `d_i` yields `λ_i` and `χ_{i+1}`, and every `γ_i` verifies. By induction down
//!   from `H`, the last `i − 1` blocks of `β_i` are `φ_{i−1}`, so the block that peeling appends,
//!   `ρ_i[Ĥ, Ĥ+1)`, is the last block of `φ_i`, hence of `β_{i+1}`.
//! - **Length** (L5′). `|χ| = 33 + Ĥℓ + 16` for every `H` and `i`; a header type of fixed width
//!   makes this hold by construction.
//! - **Rejection order.** `peel` rejects an `α` that is not a curve point or is the identity before
//!   any ECDH, and verifies `γ` before any layer is decoded, so a random cell (link cover, F = 0)
//!   costs one ECDH and is dropped before admission.

use chacha20::cipher::KeyIvInit;
use chacha20::cipher::StreamCipher;
use chacha20::ChaCha20;
use hkdf::HkdfExtract;
use hmac::digest::Key;
use hmac::Hmac;
use hmac::Mac;
use k256::elliptic_curve::bigint::Encoding;
use k256::elliptic_curve::bigint::U512;
use k256::elliptic_curve::group::Group;
use k256::elliptic_curve::ops::MulByGenerator;
use k256::elliptic_curve::ops::Reduce;
use k256::elliptic_curve::point::AffineCoordinates;
use k256::elliptic_curve::sec1::FromEncodedPoint;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::AffinePoint;
use k256::EncodedPoint;
use k256::NonZeroScalar;
use k256::ProjectivePoint;
use k256::Scalar;
use rand::CryptoRng;
use rand::RngCore;
use rings_core::delegation::DelegateeKey;
use rings_core::ecc::Point;
use rings_core::ecc::PublicKey;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use super::hkdf_expand;
use super::layer::OnionLayer;
use super::layer::OnionLayerError;
use super::layer::ONION_LAYER_BYTES;
use super::take_array;
use super::xor_in_place;
use super::MAX_ONION_LOOP_HOPS;

/// `|α|`, a compressed SEC1 secp256k1 point.
pub const ONION_GROUP_ELEMENT_BYTES: usize = 33;

/// `|γ|`, the truncated header MAC.
pub const ONION_HEADER_MAC_BYTES: usize = 16;

/// `|β| = Ĥ·ℓ`, the routing information.
pub const ONION_HEADER_ROUTING_BYTES: usize = MAX_ONION_LOOP_HOPS * ONION_LAYER_BYTES;

/// `|χ| = |α| + |β| + |γ|`.
pub const ONION_HEADER_BYTES: usize =
    ONION_GROUP_ELEMENT_BYTES + ONION_HEADER_ROUTING_BYTES + ONION_HEADER_MAC_BYTES;

/// `|ρ| = (Ĥ + 1)·ℓ`: the pad for `β ‖ 0^ℓ`.
const ONION_HEADER_STREAM_BYTES: usize = ONION_HEADER_ROUTING_BYTES + ONION_LAYER_BYTES;

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

/// Width of the blinding material, reduced modulo `n` without bias.
const BLIND_BYTES: usize = 64;

/// `γ`, a truncated HMAC-SHA256 over `β`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnionHeaderMac([u8; ONION_HEADER_MAC_BYTES]);

/// The Sphinx header `χ = (α, β, γ)`, exactly `|χ|` bytes whatever the loop length.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionHeader {
    /// `α`, compressed SEC1; validated when peeled.
    alpha: [u8; ONION_GROUP_ELEMENT_BYTES],
    /// `β`.
    routing: [u8; ONION_HEADER_ROUTING_BYTES],
    /// `γ = MAC(β)`.
    mac: OnionHeaderMac,
}

/// One position of a loop as the client builds it: the hop's key and its layer.
pub struct OnionHeaderHop {
    /// `pk_i`, the delegatee key of the hop at this position.
    pub public_key: PublicKey<33>,
    /// `λ_i` without `γ_{i+1}`, which the builder computes.
    pub layer: OnionLayer,
}

/// The result of peeling one header: this hop's layer and the header it forwards.
pub struct OnionPeeledHeader {
    /// `λ_i`.
    pub layer: OnionLayer,
    /// `χ_{i+1}`.
    pub next: OnionHeader,
}

/// Why a header was not built or not peeled.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OnionHeaderError {
    /// The loop has no position or more than `Ĥ`.
    #[error("loop of {0} positions is outside 1..={MAX_ONION_LOOP_HOPS}")]
    HopCount(usize),
    /// A hop's public key is not a secp256k1 point.
    #[error("onion hop public key is not a secp256k1 point")]
    PublicKey,
    /// `α` is not a curve point, or is the identity.
    #[error("onion header group element is not a non-identity secp256k1 point")]
    GroupElement,
    /// The derived blinding factor is `0 mod n`.
    #[error("onion header blinding factor is zero")]
    Blinding,
    /// `γ` does not verify.
    #[error("onion header MAC does not verify")]
    Mac,
    /// The peeled layer is not a layer encoding.
    #[error(transparent)]
    Layer(#[from] OnionLayerError),
}

/// The per-hop secrets derived from `(α_i, K_i)`.
struct OnionHopSecrets {
    /// The ChaCha20 key of `ρ_i`.
    stream_key: Zeroizing<[u8; STREAM_KEY_BYTES]>,
    /// The HMAC key of `γ_i`.
    mac_key: Zeroizing<[u8; MAC_KEY_BYTES]>,
    /// `b_i`.
    blinding: NonZeroScalar,
}

impl OnionHeaderMac {
    /// Wrap MAC bytes.
    pub const fn new(bytes: [u8; ONION_HEADER_MAC_BYTES]) -> Self {
        Self(bytes)
    }

    /// Return the MAC bytes.
    pub const fn to_bytes(self) -> [u8; ONION_HEADER_MAC_BYTES] {
        self.0
    }
}

impl OnionHopSecrets {
    /// The key schedule of one position from `α_i` (encoded) and `K_i`.
    ///
    /// # Errors
    ///
    /// [`OnionHeaderError::Blinding`] if the blinding material reduces to `0 mod n`
    /// (probability `2^−256`); the client then re-draws `x`, a hop drops the cell.
    fn derive(
        alpha: &[u8; ONION_GROUP_ELEMENT_BYTES],
        shared: &AffinePoint,
    ) -> Result<Self, OnionHeaderError> {
        let mut extract = HkdfExtract::<Sha256>::new(Some(HEADER_KDF_SALT));
        extract.input_ikm(alpha.as_slice());
        extract.input_ikm(shared.x().as_slice());
        let (_, kdf) = extract.finalize();
        let blind = hkdf_expand::<BLIND_BYTES>(&kdf, &[BLIND_INFO]);
        let blinding = NonZeroScalar::new(<Scalar as Reduce<U512>>::reduce(U512::from_be_bytes(
            *blind,
        )))
        .into_option()
        .ok_or(OnionHeaderError::Blinding)?;
        Ok(Self {
            stream_key: hkdf_expand(&kdf, &[STREAM_INFO]),
            mac_key: hkdf_expand(&kdf, &[MAC_INFO]),
            blinding,
        })
    }

    /// `ρ_i`, the ChaCha20 keystream of `(Ĥ + 1)·ℓ` bytes under the zero nonce (one key, one
    /// stream).
    fn stream(&self) -> Zeroizing<[u8; ONION_HEADER_STREAM_BYTES]> {
        let mut stream = Zeroizing::new([0_u8; ONION_HEADER_STREAM_BYTES]);
        ChaCha20::new(&(*self.stream_key).into(), &[0_u8; 12].into())
            .apply_keystream(stream.as_mut_slice());
        stream
    }

    /// `MAC_{K_i}(β)`.
    fn mac(&self, routing: &[u8; ONION_HEADER_ROUTING_BYTES]) -> OnionHeaderMac {
        let mut key = Key::<Hmac<Sha256>>::default();
        key.iter_mut()
            .zip(self.mac_key.iter())
            .for_each(|(slot, byte)| *slot = *byte);
        let mut mac = <Hmac<Sha256> as Mac>::new(&key);
        mac.update(routing.as_slice());
        OnionHeaderMac(take_array(&mut mac.finalize().into_bytes().into_iter()))
    }
}

/// `SEC1(P)`, compressed; `None` for the identity, which has no 33-byte encoding.
fn encode_element(point: &AffinePoint) -> Option<[u8; ONION_GROUP_ELEMENT_BYTES]> {
    let encoded = point.to_encoded_point(true);
    (encoded.len() == ONION_GROUP_ELEMENT_BYTES)
        .then(|| take_array(&mut encoded.as_bytes().iter().copied()))
}

impl OnionHeader {
    /// Decode a header from its `|χ|` bytes; `α` is validated when the header is peeled.
    pub fn from_bytes(bytes: &[u8; ONION_HEADER_BYTES]) -> Self {
        let mut fields = bytes.iter().copied();
        Self {
            alpha: take_array(&mut fields),
            routing: take_array(&mut fields),
            mac: OnionHeaderMac(take_array(&mut fields)),
        }
    }

    /// Encode the header as `α ‖ β ‖ γ`.
    pub fn to_bytes(&self) -> [u8; ONION_HEADER_BYTES] {
        take_array(
            &mut self
                .alpha
                .iter()
                .chain(self.routing.iter())
                .chain(self.mac.0.iter())
                .copied(),
        )
    }

    /// Build `χ_1` for the loop `hops = (pk_i, λ_i)_{i=1…H}`, drawing `x`, `R` and `γ_{H+1}`
    /// from `rng`.
    ///
    /// ```text
    /// H ≤ Ĥ ?                                       else HopCount   (H = 0: no innermost layer)
    /// x ← Z_n^*
    /// for i = 1 … H:     α_i = x_i·G,  K_i = x_i·pk_i,  secrets_i,  x_{i+1} = x_i·b_i
    /// for i = 1 … H−1:   φ_i = (φ_{i−1} ‖ 0^ℓ) ⊕ ρ_i[Ĥ−i+1, Ĥ+1)
    /// β_H = ((λ_H(γ_{H+1} uniform) ‖ R) ⊕ ρ_H[0, Ĥ−H+1)) ‖ φ_{H−1},   γ_H = MAC_H(β_H)
    /// for i = H−1 … 1:   β_i = (λ_i(γ_{i+1}) ‖ β_{i+1}[0, Ĥ−1)) ⊕ ρ_i[0, Ĥ),  γ_i = MAC_i(β_i)
    /// return (α_1, β_1, γ_1)
    /// ```
    ///
    /// # Errors
    ///
    /// [`OnionHeaderError::HopCount`] outside `1 ≤ H ≤ Ĥ` (`H = 0` leaves no innermost layer), [`OnionHeaderError::PublicKey`] for a
    /// key that is not a curve point, and [`OnionHeaderError::Blinding`] for a zero blinding
    /// factor, after which the client builds again with fresh randomness.
    pub fn build(
        hops: &[OnionHeaderHop],
        rng: &mut (impl CryptoRng + RngCore),
    ) -> Result<Self, OnionHeaderError> {
        if hops.len() > MAX_ONION_LOOP_HOPS {
            return Err(OnionHeaderError::HopCount(hops.len()));
        }
        let (_, schedule) = hops.iter().try_fold(
            (
                NonZeroScalar::random(&mut *rng),
                Vec::with_capacity(hops.len()),
            ),
            |(exponent, mut schedule), hop| {
                let recipient = AffinePoint::try_from(hop.public_key)
                    .map_err(|_| OnionHeaderError::PublicKey)?;
                let alpha = encode_element(
                    &ProjectivePoint::mul_by_generator(exponent.as_ref()).to_affine(),
                )
                .ok_or(OnionHeaderError::GroupElement)?;
                let shared = (ProjectivePoint::from(recipient) * exponent.as_ref()).to_affine();
                let secrets = OnionHopSecrets::derive(&alpha, &shared)?;
                let next_exponent = exponent * secrets.blinding;
                schedule.push((alpha, secrets));
                Ok::<_, OnionHeaderError>((next_exponent, schedule))
            },
        )?;
        let filler = schedule.iter().take(hops.len().saturating_sub(1)).fold(
            Vec::new(),
            |mut filler, (_, secrets)| {
                filler.extend([0_u8; ONION_LAYER_BYTES]);
                let offset = ONION_HEADER_STREAM_BYTES.saturating_sub(filler.len());
                xor_in_place(filler.iter_mut(), secrets.stream().iter().skip(offset));
                filler
            },
        );
        let mut positions = hops.iter().zip(schedule.iter()).rev();
        let innermost = positions.next().map(|(hop, (alpha, secrets))| {
            let mut last_mac = [0_u8; ONION_HEADER_MAC_BYTES];
            rng.fill_bytes(&mut last_mac);
            let mut routing = [0_u8; ONION_HEADER_ROUTING_BYTES];
            rng.fill_bytes(&mut routing);
            let masked = ONION_HEADER_ROUTING_BYTES.saturating_sub(filler.len());
            routing
                .iter_mut()
                .zip(hop.layer.encode(OnionHeaderMac(last_mac)).iter())
                .for_each(|(slot, byte)| *slot = *byte);
            xor_in_place(routing.iter_mut().take(masked), secrets.stream().iter());
            routing
                .iter_mut()
                .skip(masked)
                .zip(filler.iter())
                .for_each(|(slot, byte)| *slot = *byte);
            let mac = secrets.mac(&routing);
            Self {
                alpha: *alpha,
                routing,
                mac,
            }
        });
        let outermost = positions.fold(innermost, |next, (hop, (alpha, secrets))| {
            next.map(|next| {
                let layer = hop.layer.encode(next.mac);
                let mut routing = take_array::<ONION_HEADER_ROUTING_BYTES>(
                    &mut layer.iter().chain(next.routing.iter()).copied(),
                );
                xor_in_place(routing.iter_mut(), secrets.stream().iter());
                let mac = secrets.mac(&routing);
                Self {
                    alpha: *alpha,
                    routing,
                    mac,
                }
            })
        });
        outermost.ok_or(OnionHeaderError::HopCount(hops.len()))
    }

    /// Peel `χ_i` under the hop's delegatee key: `(λ_i, χ_{i+1})`.
    ///
    /// ```text
    /// α_i ∈ E(F_p) \ {O} ?                   else GroupElement      (before any ECDH)
    /// K_i = d_i·α_i;  secrets_i
    /// γ_i = MAC_i(β_i) ?                     else Mac               (before any decoding)
    /// λ_i ‖ β_{i+1} = (β_i ‖ 0^ℓ) ⊕ ρ_i;  decode λ_i = (layer, γ_{i+1})
    /// α_{i+1} = b_i·α_i ≠ O ?                else GroupElement
    /// return (layer, (α_{i+1}, β_{i+1}, γ_{i+1}))
    /// ```
    ///
    /// # Errors
    ///
    /// [`OnionHeaderError::GroupElement`], [`OnionHeaderError::Blinding`],
    /// [`OnionHeaderError::Mac`] and [`OnionHeaderError::Layer`], in that order of checking.
    pub fn peel(&self, key: &DelegateeKey) -> Result<OnionPeeledHeader, OnionHeaderError> {
        let alpha = EncodedPoint::from_bytes(self.alpha)
            .ok()
            .and_then(|encoded| AffinePoint::from_encoded_point(&encoded).into_option())
            .map(ProjectivePoint::from)
            .filter(|point| !bool::from(point.is_identity()))
            .ok_or(OnionHeaderError::GroupElement)?;
        let shared = key
            .diffie_hellman(Point::new(alpha))
            .into_inner()
            .to_affine();
        let secrets = OnionHopSecrets::derive(&self.alpha, &shared)?;
        if !bool::from(secrets.mac(&self.routing).0.ct_eq(self.mac.0.as_slice())) {
            return Err(OnionHeaderError::Mac);
        }
        let stream = secrets.stream();
        let mut plaintext = self
            .routing
            .iter()
            .copied()
            .chain(core::iter::repeat_n(0, ONION_LAYER_BYTES))
            .zip(stream.iter())
            .map(|(byte, mask)| byte ^ mask);
        let layer = Zeroizing::new(take_array::<ONION_LAYER_BYTES>(&mut plaintext));
        let routing = take_array::<ONION_HEADER_ROUTING_BYTES>(&mut plaintext);
        let (layer, next_mac) = OnionLayer::decode(&layer)?;
        let next_alpha = encode_element(&(alpha * secrets.blinding.as_ref()).to_affine())
            .ok_or(OnionHeaderError::GroupElement)?;
        Ok(OnionPeeledHeader {
            layer,
            next: Self {
                alpha: next_alpha,
                routing,
                mac: next_mac,
            },
        })
    }
}
