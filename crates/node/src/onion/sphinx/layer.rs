//! The uniform layer `λ_i` (#834 D6, D6″) and its fixed-width encoding.
//!
//! Every position of a loop, relay or symbol hop, first or last, has one layer shape
//!
//! ```text
//! λ = (f, ā, next, e, x, ν, κ = (σ_in, σ_out), γ_{i+1})
//!
//! ℓ = 1 + 64 + 20 + 16 + 8 + 16 + 64 + 16 = 205 bytes
//!     f   ā    next e    x   ν    κ    γ
//! ```
//!
//! The client tag `t_⋄` of D6′ has no field of its own: it is `γ_{H+1}`, the next-MAC field of
//! `λ_H`, which the guard forwards to the client as the MAC of the header it sends there.
//!
//! `γ_{i+1}` is the MAC of the next header, which only the header builder can compute; it is
//! therefore not a field of [`OnionLayer`] but an argument of [`OnionLayer::encode`] and a result
//! of [`OnionLayer::decode`]:
//!
//! ```text
//! encode : OnionLayer × Γ → {0,1}^{8ℓ}
//! decode : {0,1}^* → (OnionLayer × Γ) + OnionLayerError
//! decode ∘ encode = Right                                  (round trip)
//! decode(w) = Right(λ, γ) ⇒ encode(λ, γ) = w                (canonical: encode is onto its image)
//! ```
//!
//! The field widths are declared once, in [`LayerRecord`]; `ℓ`, the encoder and the decoder all
//! follow from that one table. The second law is why decoding rejects a `relay` layer whose
//! argument field is not `0^A`: `relay` takes `ā = ()`, so it has exactly one encoding.

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
use crate::onion::circuit::OnionForwardNonce;
use crate::onion::signature::OnionSymbol;
use crate::onion::signature::ONION_SIGNATURE;
use crate::onion::OnionExitEpoch;
use crate::onion::OnionServiceName;

/// Width `A` of the argument field `ā`: every application's arguments are encoded into exactly
/// this many bytes by its symbol's argument codec.
pub(crate) const ONION_ARGUMENT_BYTES: usize = 64;

/// Declares the wire record of a layer once, as `field: width` in wire order, and derives from
/// that one table the record type, its width `ℓ` (the sum of the widths), its encoder
/// (concatenation in order) and its decoder (the fields of a string of width exactly `ℓ`).
macro_rules! layer_record {
    ($($(#[doc = $doc:literal])* $field:ident: $width:expr,)*) => {
        /// The raw fields of one layer, in wire order. Zeroized on drop: it holds the seeds.
        #[derive(Zeroize, ZeroizeOnDrop)]
        struct LayerRecord {
            $($(#[doc = $doc])* $field: [u8; $width],)*
        }

        impl LayerRecord {
            /// `ℓ`, the sum of the field widths.
            const WIDTH: usize = 0 $(+ $width)*;

            /// The fields concatenated in wire order: exactly `ℓ` bytes, since `ℓ` is their sum.
            fn encode(&self) -> Zeroizing<[u8; Self::WIDTH]> {
                let mut bytes = Zeroizing::new([0; Self::WIDTH]);
                bytes
                    .iter_mut()
                    .zip(core::iter::empty()$(.chain(self.$field.iter()))*)
                    .for_each(|(slot, byte)| *slot = *byte);
                bytes
            }

            /// The fields of `bytes`, or `None` unless `|bytes| = ℓ`.
            fn decode(mut bytes: &[u8]) -> Option<Self> {
                let record = Self { $($field: take_field(&mut bytes)?,)* };
                bytes.is_empty().then_some(record)
            }
        }
    };
}

layer_record! {
    /// `f`, the symbol's layer code.
    code: 1,
    /// `ā`.
    arguments: ONION_ARGUMENT_BYTES,
    /// `next`, the raw DID.
    next: 20,
    /// `e`, the process epoch.
    epoch: 16,
    /// `x`, big-endian milliseconds.
    expiry: 8,
    /// `ν`.
    nonce: 16,
    /// `σ_in`.
    inbound: ONION_CARRY_SEED_BYTES,
    /// `σ_out`.
    outbound: ONION_CARRY_SEED_BYTES,
    /// `γ_{i+1}`.
    next_mac: ONION_HEADER_MAC_BYTES,
}

/// `ℓ`, the encoded width of one layer.
pub(crate) const ONION_LAYER_BYTES: usize = LayerRecord::WIDTH;

/// `(w[0, N), w[N, |w|))`: the next field of a record and the rest, or `None` past the end.
fn take_field<const N: usize>(bytes: &mut &[u8]) -> Option<[u8; N]> {
    let (field, rest) = bytes.split_first_chunk::<N>()?;
    *bytes = rest;
    Some(*field)
}

/// The client-supplied arguments `ā` of one application, encoded to exactly `A` bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionArguments([u8; ONION_ARGUMENT_BYTES]);

impl OnionArguments {
    /// Wrap arguments already encoded by their symbol's argument codec.
    pub(crate) const fn new(bytes: [u8; ONION_ARGUMENT_BYTES]) -> Self {
        Self(bytes)
    }
}

/// The application `(f, ā)` a layer names, one constructor per shape of `Σ = {relay} ⊎ Σ_W`.
///
/// `relay` takes `ā = ()`, so it has no argument field: a relay with arguments is
/// unrepresentable, and its encoding writes `0^A`.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum OnionLayerApplication {
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
/// Zeroized on drop, since the carry seeds are key material; the seeds compare in constant time.
#[derive(Debug, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub(crate) struct OnionLayer {
    /// The application `(f_i, ā_i)` this position evaluates.
    #[zeroize(skip)]
    pub(crate) application: OnionLayerApplication,
    /// `next_i`, the DID the hop hands its cell to (L6: fixed by the client).
    #[zeroize(skip)]
    pub(crate) next: Did,
    /// `e_i`, the process epoch of the hop this layer is sealed for (#834 D2, L9).
    #[zeroize(skip)]
    pub(crate) epoch: OnionExitEpoch,
    /// `x`, the loop expiry in milliseconds, one per loop (#834 D6).
    pub(crate) expires_at_ms: u64,
    /// `ν_i`, the replay nonce the hop admits at most once (L9).
    #[zeroize(skip)]
    pub(crate) nonce: OnionForwardNonce,
    /// `σ_in`, the seed of the key that removes this hop's inbound carry layer (D7).
    pub(crate) inbound: OnionCarrySeed,
    /// `σ_out`, the segment seed of the carry this hop produces; uniform at a relay (D7).
    pub(crate) outbound: OnionSegmentSeed,
}

/// A byte string outside the image of [`OnionLayer::encode`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionLayerError {
    /// The string is not `ℓ` bytes long.
    #[error("layer of {0} bytes is not {ONION_LAYER_BYTES} bytes long")]
    Width(usize),
    /// The symbol code names no symbol of the closed signature `Σ`.
    #[error("layer symbol code {0} names no symbol of the onion signature")]
    UnknownSymbol(u8),
    /// A `relay` layer whose argument field is not `0^A`.
    #[error("relay layer carries arguments")]
    RelayArguments,
}

impl OnionLayer {
    /// `encode(λ, γ_{i+1})`: the fields of [`LayerRecord`] in wire order, integers big-endian.
    pub(crate) fn encode(&self, next_mac: &OnionHeaderMac) -> Zeroizing<[u8; ONION_LAYER_BYTES]> {
        let (code, arguments) = match &self.application {
            OnionLayerApplication::Relay => (
                ONION_SIGNATURE.code(ONION_SIGNATURE.relay()),
                [0; ONION_ARGUMENT_BYTES],
            ),
            OnionLayerApplication::Apply { symbol, arguments } => {
                (ONION_SIGNATURE.code(symbol.spec()), arguments.0)
            }
        };
        LayerRecord {
            code: [code],
            arguments,
            next: PublicKeyAddress::from(self.next).to_fixed_bytes(),
            epoch: self.epoch.to_bytes(),
            expiry: self.expires_at_ms.to_be_bytes(),
            nonce: self.nonce.to_bytes(),
            inbound: *self.inbound.as_bytes(),
            outbound: *self.outbound.as_bytes(),
            next_mac: next_mac.to_bytes(),
        }
        .encode()
    }

    /// `decode(w)`, the left inverse of [`Self::encode`] on its image.
    ///
    /// # Errors
    ///
    /// [`OnionLayerError::Width`] unless `|w| = ℓ`, [`OnionLayerError::UnknownSymbol`] for a code
    /// outside `Σ`, and [`OnionLayerError::RelayArguments`] for a non-canonical `relay` layer.
    pub(crate) fn decode(bytes: &[u8]) -> Result<(Self, OnionHeaderMac), OnionLayerError> {
        let record = LayerRecord::decode(bytes).ok_or(OnionLayerError::Width(bytes.len()))?;
        let [code] = record.code;
        let application = match ONION_SIGNATURE
            .by_code(code)
            .ok_or(OnionLayerError::UnknownSymbol(code))?
        {
            OnionSymbol::Relay => (record.arguments == [0; ONION_ARGUMENT_BYTES])
                .then_some(OnionLayerApplication::Relay)
                .ok_or(OnionLayerError::RelayArguments)?,
            OnionSymbol::WorldFacing(symbol) => OnionLayerApplication::Apply {
                symbol,
                arguments: OnionArguments(record.arguments),
            },
        };
        Ok((
            Self {
                application,
                next: Did::from(PublicKeyAddress::from(record.next)),
                epoch: OnionExitEpoch::new(record.epoch),
                expires_at_ms: u64::from_be_bytes(record.expiry),
                nonce: OnionForwardNonce::new(record.nonce),
                inbound: OnionCarrySeed::new(record.inbound),
                outbound: OnionSegmentSeed::new(record.outbound),
            },
            OnionHeaderMac::new(record.next_mac),
        ))
    }
}
