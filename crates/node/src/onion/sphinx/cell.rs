//! The cell on a loop edge, `cell = χ ‖ y`, as one product with one parser (#834 D6, Hop line 1).
//!
//! ```text
//! parse    : {0,1}^* ⇀ OnionCell,   w ↦ (b, χ, y)     b = |w| (F = 0), |χ| = 2919, |y| = C_b
//! to_bytes : OnionCell → {0,1}^*,   (b, χ, y) ↦ χ ‖ y
//! to_bytes ∘ parse = id on its domain,   parse ∘ to_bytes = Some
//! ```
//!
//! The class `b` is never an argument at a hop: it is the observed length, fixed by [`parse`],
//! and every use downstream (the MAC label of `γ`, the width `C_b` of the slot, the class of the
//! forwarded cell) reads it from the cell. So a relabelled cell (#834 H1) can only arrive as a
//! cell of the other length, whose `γ` fails at the next honest hop. A hop's step is
//!
//! ```text
//!             peel (χ under d_i)
//! OnionCell ──────────────────────▶ OnionPeeledCell
//!
//!                  step, by λ_i's application
//! OnionPeeledCell ─┬─ relay:  Dec⁰ under KDF₄₈(σ_in)  ──▶ Relayed(OnionCell)
//!                  └─ symbol: Dec^τ under KDF₄₈(σ_in) ──▶ Consumed(v, OnionProducer)
//!
//! OnionProducer ── produce(v′), sealed under the keys of σ_out in λ_i ──▶ OnionCell
//! ```
//!
//! The class flows along every arrow unchanged, the role is the layer's application, and the
//! producer's keys come from its own layer: a hop supplies none of the three. The client has one
//! constructor, [`OnionCell::client`], which builds the header and seals the first segment in the
//! class it chooses, and reads a returning cell through [`OnionCell::loop_tag`] and
//! [`OnionCell::open`].
//!
//! Buffers: [`OnionCell::parse`] takes the received `Vec` and splits the slot off it, and
//! [`OnionCell::into_bytes`] re-encodes the forwarded header into the same allocation and appends
//! the slot, so a relay allocates once per cell.
//!
//! [`parse`]: OnionCell::parse

use rand::CryptoRng;
use rand::RngCore;
use rings_aez::KeyError;
use rings_core::delegation::DelegateeKey;
use zeroize::Zeroizing;

use super::carry;
use super::carry::OnionCarry;
use super::carry::OnionOpenError;
use super::carry::OnionValueTooWide;
use super::class::OnionLoopClass;
use super::header::OnionBlindingError;
use super::header::OnionHeader;
use super::header::OnionHeaderRoute;
use super::header::OnionLoopTag;
use super::header::OnionPeelError;
use super::header::ONION_HEADER_BYTES;
use super::layer::OnionLayer;
use super::layer::OnionLayerApplication;
use super::seed::OnionCarryKey;
use super::seed::OnionSegmentKeys;

/// One cell `(b, χ, y)` of a loop edge.
///
/// Invariant: `|χ| + |y| = b`, established by [`OnionCell::parse`] and by every constructor,
/// which takes the class of the cell it continues or of the client's choice.
pub(crate) struct OnionCell {
    /// `b`, the observed or chosen cell length.
    class: OnionLoopClass,
    /// `χ`.
    header: OnionHeader,
    /// `y`, `C_b` bytes.
    carry: OnionCarry,
    /// The allocation the cell is re-encoded into, with capacity for `b` bytes.
    buffer: Vec<u8>,
}

/// A cell whose header this hop has peeled: `λ_i`, `χ_{i+1}` and the inbound slot `y_{i−1}`.
pub(crate) struct OnionPeeledCell {
    /// `b`, the class of the received cell and of the cell this hop forwards.
    class: OnionLoopClass,
    /// `λ_i`.
    layer: OnionLayer,
    /// `χ_{i+1}`.
    next: OnionHeader,
    /// `y_{i−1}`.
    carry: OnionCarry,
    /// The received cell's allocation.
    buffer: Vec<u8>,
}

/// The outcome of a hop's carry step, chosen by its layer's application.
pub(crate) enum OnionStep {
    /// A relay removed one AEZ layer: the cell it forwards.
    Relayed {
        /// `λ_i`.
        layer: OnionLayer,
        /// The forwarded cell, of the received class.
        cell: OnionCell,
    },
    /// A symbol hop received its input: the value and the producer of its output.
    Consumed {
        /// `λ_i`.
        layer: OnionLayer,
        /// `v`, zeroized on drop.
        value: Zeroizing<Vec<u8>>,
        /// The producer of the forwarded cell.
        producer: OnionProducer,
    },
}

/// A symbol hop between consuming its input and producing its output.
pub(crate) struct OnionProducer {
    /// `b`, the class of the received cell.
    class: OnionLoopClass,
    /// `χ_{i+1}`.
    next: OnionHeader,
    /// The segment keys of `σ_out` in `λ_i`.
    keys: OnionSegmentKeys,
    /// The received cell's allocation.
    buffer: Vec<u8>,
}

/// A byte string whose length is no loop class: no cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0} bytes is not the length of any onion loop class")]
pub(crate) struct OnionCellWidth(pub(crate) usize);

/// Why a hop's carry step failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionStepError {
    /// A key named by the layer's seeds is weak.
    #[error(transparent)]
    Key(#[from] KeyError),
    /// A symbol hop rejected its input slot.
    #[error(transparent)]
    Open(#[from] OnionOpenError),
}

/// Why the client did not build a loop's first cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionClientError {
    /// A blinding factor was zero; build again with fresh randomness.
    #[error(transparent)]
    Blinding(#[from] OnionBlindingError),
    /// The value does not fit the class.
    #[error(transparent)]
    ValueTooWide(#[from] OnionValueTooWide),
}

impl OnionCell {
    /// `w ↦ (b, χ, y)` with `b = |w|`: the one parser of a received cell. The slot is split off
    /// the received buffer, which is kept for re-encoding.
    ///
    /// # Errors
    ///
    /// [`OnionCellWidth`] unless `|w|` is the length of a loop class.
    pub(crate) fn parse(mut bytes: Vec<u8>) -> Result<Self, OnionCellWidth> {
        let width = bytes.len();
        OnionLoopClass::from_cell_bytes(width)
            .and_then(|class| {
                let header = OnionHeader::decode(bytes.get(..ONION_HEADER_BYTES)?)?;
                let carry = OnionCarry::new(bytes.split_off(ONION_HEADER_BYTES)).ok()?;
                Some(Self {
                    class,
                    header,
                    carry,
                    buffer: bytes,
                })
            })
            .ok_or(OnionCellWidth(width))
    }

    /// `χ ‖ y`, exactly `b` bytes, in the cell's allocation.
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        let mut bytes = self.buffer;
        bytes.clear();
        self.header.encode_into(&mut bytes);
        bytes.extend_from_slice(self.carry.as_slice());
        bytes
    }

    /// Return the class `b`.
    pub(crate) const fn class(&self) -> OnionLoopClass {
        self.class
    }

    /// The client's first cell of a loop: the header over `route` and `v` sealed for segment 0
    /// under `keys`, in the class the client chooses; returns the cell and the tag `t_⋄` the loop
    /// will return with.
    ///
    /// # Errors
    ///
    /// [`OnionClientError::ValueTooWide`] if `|v| ≥ C₀`, and [`OnionClientError::Blinding`] for a
    /// zero blinding factor (build again).
    pub(crate) fn client(
        route: &OnionHeaderRoute,
        class: OnionLoopClass,
        keys: &OnionSegmentKeys,
        value: &[u8],
        rng: &mut (impl CryptoRng + RngCore),
    ) -> Result<(Self, OnionLoopTag), OnionClientError> {
        let carry = carry::seal(class, keys, value)?;
        let (header, tag) = OnionHeader::build(route, class, rng)?;
        Ok((
            Self {
                class,
                header,
                carry,
                buffer: Vec::with_capacity(class.cell_bytes()),
            },
            tag,
        ))
    }

    /// The client tag `t_⋄` of a cell arriving at the client, position `H + 1`.
    pub(crate) const fn loop_tag(&self) -> OnionLoopTag {
        self.header.loop_tag()
    }

    /// The client's view of a returning cell: the last segment's value under `k_{c_n}`.
    ///
    /// # Errors
    ///
    /// The [`OnionOpenError`] of the consumer's check.
    pub(crate) fn open(self, key: &OnionCarryKey) -> Result<Zeroizing<Vec<u8>>, OnionOpenError> {
        carry::open(key, self.carry)
    }

    /// Peel `χ_i` under the hop's key, with the class the cell arrived in.
    ///
    /// # Errors
    ///
    /// The [`OnionPeelError`] of the header.
    pub(crate) fn peel(self, key: &DelegateeKey) -> Result<OnionPeeledCell, OnionPeelError> {
        let peeled = self.header.peel(self.class, key)?;
        Ok(OnionPeeledCell {
            class: self.class,
            layer: peeled.layer,
            next: peeled.next,
            carry: self.carry,
            buffer: self.buffer,
        })
    }
}

impl OnionPeeledCell {
    /// Return `λ_i`, for the admission and routing decisions made before the carry step.
    pub(crate) const fn layer(&self) -> &OnionLayer {
        &self.layer
    }

    /// The carry step of this position, by `λ_i`'s application:
    ///
    /// ```text
    /// relay     y_i = Dec⁰_{KDF₄₈(σ_in)}(y_{i−1})                   ⇒ Relayed, same class
    /// f ∈ Σ_W   v = pad⁻¹ Dec^τ_{KDF₄₈(σ_in)}(y_{i−1}),  keys(σ_out) ⇒ Consumed
    /// ```
    ///
    /// # Errors
    ///
    /// [`OnionStepError::Key`] for a weak key, and [`OnionStepError::Open`] if a symbol hop
    /// rejects its input.
    pub(crate) fn step(mut self) -> Result<OnionStep, OnionStepError> {
        match self.layer.application {
            OnionLayerApplication::Relay => {
                carry::peel(&self.layer.inbound.key()?, &mut self.carry);
                Ok(OnionStep::Relayed {
                    layer: self.layer,
                    cell: OnionCell {
                        class: self.class,
                        header: self.next,
                        carry: self.carry,
                        buffer: self.buffer,
                    },
                })
            }
            OnionLayerApplication::Apply { .. } => {
                let value = carry::open(&self.layer.inbound.key()?, self.carry)?;
                let keys = self.layer.outbound.keys()?;
                Ok(OnionStep::Consumed {
                    layer: self.layer,
                    value,
                    producer: OnionProducer {
                        class: self.class,
                        next: self.next,
                        keys,
                        buffer: self.buffer,
                    },
                })
            }
        }
    }
}

impl OnionProducer {
    /// A symbol hop's output: `v′` sealed under the keys of `σ_out`, forwarded as a cell of the
    /// received class.
    ///
    /// # Errors
    ///
    /// [`OnionValueTooWide`] if `|v′| ≥ C₀`.
    pub(crate) fn produce(self, value: &[u8]) -> Result<OnionCell, OnionValueTooWide> {
        Ok(OnionCell {
            class: self.class,
            carry: carry::seal(self.class, &self.keys, value)?,
            header: self.next,
            buffer: self.buffer,
        })
    }
}
