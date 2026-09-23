//! The uniform layer `λ_i` (#834 D6, D6″) and its fixed-width encoding.
//!
//! Every position of a loop, relay or symbol hop, first or last, has one layer shape
//!
//! ```text
//! λ = (f, ā, next, e, x, ν, κ = (σ_in, σ_out), γ_{i+1}, t)
//!
//! ℓ = 1 + 64 + 20 + 16 + 8 + 16 + 64 + 16 + 16 = 221 bytes
//!     f   ā    next e    x   ν    κ    γ    t
//! ```
//!
//! `γ_{i+1}` is the MAC of the next header, which only the header builder can compute; it is
//! therefore not a field of [`OnionLayer`] but an argument of [`OnionLayer::encode`] and a result
//! of [`OnionLayer::decode`]:
//!
//! ```text
//! encode : OnionLayer × Γ → {0,1}^{8ℓ}
//! decode : {0,1}^{8ℓ} → (OnionLayer × Γ) + OnionLayerError
//! decode ∘ encode = Right                                  (round trip)
//! decode(w) = Right(λ, γ) ⇒ encode(λ, γ) = w                (canonical: encode is onto its image)
//! ```
//!
//! The second law is why decoding rejects a `relay` layer whose argument field is not `0^A`:
//! `relay` takes `ā = ()`, so it has exactly one encoding.

use rings_core::dht::Did;
use rings_core::ecc::PublicKeyAddress;
use zeroize::Zeroize;
use zeroize::ZeroizeOnDrop;
use zeroize::Zeroizing;

use super::header::OnionHeaderMac;
use super::header::ONION_HEADER_MAC_BYTES;
use super::seed::OnionCarrySeed;
use super::seed::OnionSegmentSeed;
use super::seed::ONION_CARRY_SEED_BYTES;
use super::take_array;
use crate::onion::circuit::OnionForwardNonce;
use crate::onion::signature::ONION_SIGNATURE;
use crate::onion::OnionExitEpoch;
use crate::onion::OnionServiceName;

/// Width `A` of the argument field `ā`: every application's arguments are encoded into exactly
/// this many bytes by its symbol's argument codec.
pub const ONION_ARGUMENT_BYTES: usize = 64;

/// Width of the symbol code `f` (see the code law of [`crate::onion::signature`]).
const SYMBOL_BYTES: usize = 1;

/// Width of the raw next-hop DID.
const DID_BYTES: usize = 20;

/// Width of the process epoch `e`.
const EPOCH_BYTES: usize = 16;

/// Width of the loop expiry `x`, big-endian milliseconds.
const EXPIRY_BYTES: usize = size_of::<u64>();

/// Width of the replay nonce `ν`.
const NONCE_BYTES: usize = 16;

/// Width of the client tag field `t` (#834 D6′).
pub const ONION_LOOP_TAG_BYTES: usize = 16;

/// `ℓ`, the encoded width of one layer, as the sum of its field widths.
pub const ONION_LAYER_BYTES: usize = SYMBOL_BYTES
    + ONION_ARGUMENT_BYTES
    + DID_BYTES
    + EPOCH_BYTES
    + EXPIRY_BYTES
    + NONCE_BYTES
    + 2 * ONION_CARRY_SEED_BYTES
    + ONION_HEADER_MAC_BYTES
    + ONION_LOOP_TAG_BYTES;

/// The client-supplied arguments `ā` of one application, encoded to exactly `A` bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnionArguments([u8; ONION_ARGUMENT_BYTES]);

impl OnionArguments {
    /// Wrap arguments already encoded by their symbol's argument codec.
    pub const fn new(bytes: [u8; ONION_ARGUMENT_BYTES]) -> Self {
        Self(bytes)
    }

    /// Return the encoded arguments.
    pub const fn to_bytes(self) -> [u8; ONION_ARGUMENT_BYTES] {
        self.0
    }
}

/// The client tag field `t` (#834 D6′): the loop tag `t_⋄` in the last layer, uniform elsewhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnionLoopTag([u8; ONION_LOOP_TAG_BYTES]);

impl OnionLoopTag {
    /// Wrap tag bytes.
    pub const fn new(bytes: [u8; ONION_LOOP_TAG_BYTES]) -> Self {
        Self(bytes)
    }

    /// Return the tag bytes.
    pub const fn to_bytes(self) -> [u8; ONION_LOOP_TAG_BYTES] {
        self.0
    }
}

/// The application `(f, ā)` a layer names, one constructor per shape of `Σ = {relay} ⊎ Σ_W`.
///
/// `relay` takes `ā = ()`, so it has no argument field: a relay with arguments is
/// unrepresentable, and its encoding writes `0^A`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OnionLayerApplication {
    /// The identity symbol `relay` with `ā = ()`.
    Relay,
    /// A symbol of `Σ_W` with its encoded arguments.
    Apply {
        /// The applied symbol.
        symbol: OnionServiceName,
        /// Its arguments `ā`.
        arguments: OnionArguments,
    },
}

/// The uniform layer `λ_i` without its header MAC `γ_{i+1}` (see the module documentation).
///
/// Zeroized on drop: the carry seeds are key material.
#[derive(Clone, Debug, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub struct OnionLayer {
    /// The application `(f_i, ā_i)` this position evaluates.
    #[zeroize(skip)]
    pub application: OnionLayerApplication,
    /// `next_i`, the DID the hop hands its cell to (L6: fixed by the client).
    #[zeroize(skip)]
    pub next: Did,
    /// `e_i`, the process epoch of the hop this layer is sealed for (#834 D2, L9).
    #[zeroize(skip)]
    pub epoch: OnionExitEpoch,
    /// `x`, the loop expiry in milliseconds, one per loop (#834 D6).
    pub expires_at_ms: u64,
    /// `ν_i`, the replay nonce the hop admits at most once (L9).
    #[zeroize(skip)]
    pub nonce: OnionForwardNonce,
    /// `σ_in`, the seed of the key that removes this hop's inbound carry layer (D7).
    pub inbound: OnionCarrySeed,
    /// `σ_out`, the segment seed of the carry this hop produces; uniform at a relay (D7).
    pub outbound: OnionSegmentSeed,
    /// `t_i`, the client tag field (#834 D6′).
    #[zeroize(skip)]
    pub tag: OnionLoopTag,
}

/// A layer string outside the image of [`OnionLayer::encode`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OnionLayerError {
    /// The symbol code names no symbol of the closed signature `Σ`.
    #[error("layer symbol code {0} names no symbol of the onion signature")]
    UnknownSymbol(u8),
    /// A `relay` layer whose argument field is not `0^A`.
    #[error("relay layer carries arguments")]
    RelayArguments,
}

impl OnionLayer {
    /// `encode(λ, γ_{i+1})`: the fields in declaration order, integers big-endian.
    ///
    /// Zeroized on drop, since the string holds the carry seeds.
    pub fn encode(&self, next_mac: OnionHeaderMac) -> Zeroizing<[u8; ONION_LAYER_BYTES]> {
        let (code, arguments) = match &self.application {
            OnionLayerApplication::Relay => {
                (ONION_SIGNATURE.relay().code(), [0; ONION_ARGUMENT_BYTES])
            }
            OnionLayerApplication::Apply { symbol, arguments } => {
                (symbol.spec().code(), arguments.to_bytes())
            }
        };
        let fields = core::iter::once(code)
            .chain(arguments)
            .chain(PublicKeyAddress::from(self.next).to_fixed_bytes())
            .chain(self.epoch.to_bytes())
            .chain(self.expires_at_ms.to_be_bytes())
            .chain(self.nonce.to_bytes())
            .chain(self.inbound.as_bytes().iter().copied())
            .chain(self.outbound.as_bytes().iter().copied())
            .chain(next_mac.to_bytes())
            .chain(self.tag.to_bytes());
        Zeroizing::new(take_array(&mut fields.into_iter()))
    }

    /// `decode(w)`, the left inverse of [`Self::encode`] on its image.
    ///
    /// # Errors
    ///
    /// [`OnionLayerError::UnknownSymbol`] for a code outside `Σ`, and
    /// [`OnionLayerError::RelayArguments`] for a non-canonical `relay` layer.
    pub fn decode(
        bytes: &[u8; ONION_LAYER_BYTES],
    ) -> Result<(Self, OnionHeaderMac), OnionLayerError> {
        let mut fields = bytes.iter().copied();
        let [code] = take_array::<SYMBOL_BYTES>(&mut fields);
        let arguments = take_array::<ONION_ARGUMENT_BYTES>(&mut fields);
        let application = if code == ONION_SIGNATURE.relay().code() {
            (arguments == [0; ONION_ARGUMENT_BYTES])
                .then_some(OnionLayerApplication::Relay)
                .ok_or(OnionLayerError::RelayArguments)?
        } else {
            ONION_SIGNATURE
                .world_facing()
                .find(|symbol| symbol.spec().code() == code)
                .map(|symbol| OnionLayerApplication::Apply {
                    symbol,
                    arguments: OnionArguments::new(arguments),
                })
                .ok_or(OnionLayerError::UnknownSymbol(code))?
        };
        let next = Did::from(PublicKeyAddress::from(take_array::<DID_BYTES>(&mut fields)));
        let epoch = OnionExitEpoch::new(take_array::<EPOCH_BYTES>(&mut fields));
        let expires_at_ms = u64::from_be_bytes(take_array::<EXPIRY_BYTES>(&mut fields));
        let nonce = OnionForwardNonce::new(take_array::<NONCE_BYTES>(&mut fields));
        let inbound = OnionCarrySeed::new(take_array::<ONION_CARRY_SEED_BYTES>(&mut fields));
        let outbound = OnionSegmentSeed::new(take_array::<ONION_CARRY_SEED_BYTES>(&mut fields));
        let next_mac = OnionHeaderMac::new(take_array::<ONION_HEADER_MAC_BYTES>(&mut fields));
        let tag = OnionLoopTag::new(take_array::<ONION_LOOP_TAG_BYTES>(&mut fields));
        let layer = Self {
            application,
            next,
            epoch,
            expires_at_ms,
            nonce,
            inbound,
            outbound,
            tag,
        };
        Ok((layer, next_mac))
    }
}
