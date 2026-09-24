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
//!             peel (χ under d_i)                   relay (Dec⁰ under KDF₄₈(σ_in))
//! OnionCell ─────────────────────▶ OnionPeeledCell ──────────────────────────────▶ OnionCell
//!                                        │ consume (Dec^τ under KDF₄₈(σ_in))
//!                                        ▼
//!                                 (v, OnionProducer) ── produce (seal, σ_out) ──▶ OnionCell
//! ```
//!
//! and the class flows along every arrow unchanged: a hop cannot change it.
//!
//! [`parse`]: OnionCell::parse

use rings_aez::Expanded;
use rings_aez::KeyError;
use rings_core::delegation::DelegateeKey;

use super::carry::OnionCarry;
use super::carry::OnionCarryError;
use super::class::OnionLoopClass;
use super::header::OnionHeader;
use super::header::OnionHeaderError;
use super::header::OnionLoopTag;
use super::header::ONION_HEADER_BYTES;
use super::layer::OnionLayer;
use super::seed::OnionCarryKey;
use super::seed::OnionSegmentKeys;

/// One cell `(b, χ, y)` of a loop edge.
///
/// Invariant: `|χ ‖ y| = b`, established by [`OnionCell::parse`] and by every constructor that
/// takes the class of an earlier cell or of the client's choice.
pub(crate) struct OnionCell {
    /// `b`, the observed cell length.
    class: OnionLoopClass,
    /// `χ`.
    header: OnionHeader,
    /// `y`, `C_b` bytes.
    carry: OnionCarry,
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
}

/// A symbol hop between consuming its input and producing its output: the class and header of
/// the cell it forwards.
pub(crate) struct OnionProducer {
    /// `b`.
    class: OnionLoopClass,
    /// `χ_{i+1}`.
    next: OnionHeader,
}

/// Why a cell was not accepted or not processed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionCellError {
    /// The string's length is no loop class, so it is no cell.
    #[error("{0} bytes is not the length of any onion loop class")]
    Width(usize),
    /// The header was not peeled.
    #[error(transparent)]
    Header(#[from] OnionHeaderError),
    /// The key named by the layer's seed is weak.
    #[error(transparent)]
    Key(#[from] KeyError),
    /// The carry was not opened or not produced.
    #[error(transparent)]
    Carry(#[from] OnionCarryError),
}

impl OnionCell {
    /// `w ↦ (b, χ, y)` with `b = |w|`: the one parser of a received cell.
    ///
    /// # Errors
    ///
    /// [`OnionCellError::Width`] unless `|w|` is the length of a loop class.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, OnionCellError> {
        OnionLoopClass::from_cell_bytes(bytes.len())
            .zip(bytes.split_first_chunk::<ONION_HEADER_BYTES>())
            .and_then(|(class, (header, carry))| {
                Some(Self {
                    class,
                    header: OnionHeader::decode(header)?,
                    carry: OnionCarry::from_slot(Expanded::new(carry.to_vec()).ok()?),
                })
            })
            .ok_or(OnionCellError::Width(bytes.len()))
    }

    /// `χ ‖ y`, exactly `b` bytes.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = self.header.to_bytes();
        bytes.extend_from_slice(self.carry.as_bytes());
        bytes
    }

    /// Return the class `b`.
    pub(crate) const fn class(&self) -> OnionLoopClass {
        self.class
    }

    /// The client's first cell of a loop of its chosen class: `χ_1` and the value sealed for
    /// segment 0.
    ///
    /// # Errors
    ///
    /// [`OnionCarryError::ValueTooWide`] if `|v| ≥ C₀`.
    pub(crate) fn seal(
        class: OnionLoopClass,
        header: OnionHeader,
        keys: &OnionSegmentKeys,
        value: &[u8],
    ) -> Result<Self, OnionCarryError> {
        OnionCarry::seal(class, keys, value).map(|carry| Self {
            class,
            header,
            carry,
        })
    }

    /// The client tag `t_⋄` of a cell arriving at the client, position `H + 1`.
    pub(crate) const fn loop_tag(&self) -> OnionLoopTag {
        self.header.loop_tag()
    }

    /// The client's view of a returning cell: the last segment's value under `k_{c_n}`.
    ///
    /// # Errors
    ///
    /// [`OnionCarryError::Inauthentic`] or [`OnionCarryError::Padding`].
    pub(crate) fn open(self, key: &OnionCarryKey) -> Result<Vec<u8>, OnionCarryError> {
        self.carry.open(key)
    }

    /// Peel `χ_i` under the hop's key, with the class the cell arrived in.
    ///
    /// # Errors
    ///
    /// The [`OnionHeaderError`] of [`OnionHeader::peel`].
    pub(crate) fn peel(self, key: &DelegateeKey) -> Result<OnionPeeledCell, OnionHeaderError> {
        let peeled = self.header.peel(self.class, key)?;
        Ok(OnionPeeledCell {
            class: self.class,
            layer: peeled.layer,
            next: peeled.next,
            carry: self.carry,
        })
    }
}

impl OnionPeeledCell {
    /// Return `λ_i`, for the admission and routing decisions made before the carry step.
    pub(crate) const fn layer(&self) -> &OnionLayer {
        &self.layer
    }

    /// A relay's step: `y_i = Dec⁰_{KDF₄₈(σ_in)}(y_{i−1})`, forwarded as a cell of the same class.
    ///
    /// # Errors
    ///
    /// [`KeyError`] if the key named by `σ_in` is weak.
    pub(crate) fn relay(self) -> Result<(OnionLayer, OnionCell), KeyError> {
        let key = self.layer.inbound.key()?;
        Ok((self.layer, OnionCell {
            class: self.class,
            header: self.next,
            carry: self.carry.peel(&key),
        }))
    }

    /// A symbol hop's input: `v = pad⁻¹ Dec^τ_{KDF₄₈(σ_in)}(y_{i−1})`.
    ///
    /// # Errors
    ///
    /// [`OnionCellError::Key`] for a weak key and [`OnionCellError::Carry`] if the value is
    /// rejected.
    pub(crate) fn consume(self) -> Result<(OnionLayer, Vec<u8>, OnionProducer), OnionCellError> {
        let value = self.carry.open(&self.layer.inbound.key()?)?;
        Ok((self.layer, value, OnionProducer {
            class: self.class,
            next: self.next,
        }))
    }
}

impl OnionProducer {
    /// A symbol hop's output: `v′` sealed under the segment keys of `σ_out`, forwarded as a cell
    /// of the received class.
    ///
    /// # Errors
    ///
    /// [`OnionCarryError::ValueTooWide`] if `|v′| ≥ C₀`.
    pub(crate) fn produce(
        self,
        keys: &OnionSegmentKeys,
        value: &[u8],
    ) -> Result<OnionCell, OnionCarryError> {
        OnionCell::seal(self.class, self.next, keys, value)
    }
}
