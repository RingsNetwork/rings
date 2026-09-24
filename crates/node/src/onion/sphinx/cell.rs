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
//! OnionPeeledCell ─┬─ relay:  Dec⁰ under KDF₄₈(σ_in)  ──▶ Relayed(head, OnionCell)
//!                  └─ symbol: Dec^τ under KDF₄₈(σ_in) ──▶ Consumed(head, v, OnionSurb)
//!
//! OnionSurb υ = (next, χ_{i+1}, σ_out, x) ── produce(v′), keys of σ_out ──▶ (next, OnionCell)
//! ```
//!
//! The class flows along every arrow unchanged, the role is the layer's application, and the
//! output's keys come from `σ_out` in the hop's own layer: a hop supplies none of the three.
//! The step consumes both seeds of `λ_i` and yields its key-free head. [`OnionSurb`] is D8's reply
//! block `υ` (2979 B: `next`, `χ`, `σ`, `x`) plus the class byte of its cell, 2980 B, so a SURB
//! pool costs that per entry whatever the class; its keys are derived when it is spent. The
//! batched-credit codec of `υ` (D8) lands in 2a-4 and decodes into this type. The client has one
//! constructor, [`OnionCell::client`], which builds the header and seals the first segment in the
//! class it chooses, and reads a returning cell through [`OnionCell::loop_tag`] and
//! [`OnionCell::open`].
//!
//! Buffers: [`OnionCell::parse`] takes the received `Vec` and splits the slot off it (one copy of
//! the slot), and [`OnionCell::into_bytes`] re-encodes the forwarded header into the same
//! allocation and appends the slot (a second copy), so a relay allocates once and copies the slot
//! twice per cell. Relaying over a slice view of one owned buffer, with no copy, is #834 Phase
//! 2a-4 (#843).
//!
//! [`parse`]: OnionCell::parse

use rand::CryptoRng;
use rand::RngCore;
use rings_aez::KeyError;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
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
use super::layer::OnionLayerHead;
use super::seed::OnionCarryKey;
use super::seed::OnionSegmentKeys;
use super::seed::OnionSegmentSeed;

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

/// The outcome of a hop's carry step, chosen by its layer's application. The step consumes both
/// seeds of `λ_i`, so only its key-free head comes out.
pub(crate) enum OnionStep {
    /// A relay removed one AEZ layer: the cell it forwards.
    Relayed {
        /// The head of `λ_i`.
        head: OnionLayerHead,
        /// The forwarded cell, of the received class.
        cell: OnionCell,
    },
    /// A symbol hop received its input: the value and the reply block of its output.
    Consumed {
        /// The head of `λ_i`.
        head: OnionLayerHead,
        /// `v`, zeroized on drop.
        value: Zeroizing<Vec<u8>>,
        /// The reply block `υ = (next, χ_{i+1}, σ_out, x)` that produces the forwarded cell.
        surb: OnionSurb,
    },
}

/// A single-use reply block `υ = (next, χ_υ, σ_υ, x_υ)` (#834 D8), with the class of its cell:
/// where the output goes, its header, the segment seed of its carry, and its expiry.
///
/// It holds neither a cell buffer nor derived keys, so its size is independent of the class: D8's
/// 2979 B (`|next| = 20`, `|χ| = 2919`, `|σ| = 32`, `|x| = 8`) plus the class byte, 2980 B.
/// Affine: no `Clone`, so a reply block is spent at most once; a value too wide for its class
/// hands it back unspent.
#[derive(Debug)]
pub(crate) struct OnionSurb {
    /// `b`, the class of the cell it produces.
    class: OnionLoopClass,
    /// `next`, the DID the produced cell goes to.
    next: Did,
    /// `χ_υ`.
    header: OnionHeader,
    /// `σ_υ`, zeroized on drop.
    outbound: OnionSegmentSeed,
    /// `x_υ`, in milliseconds.
    expires_at_ms: u64,
}

// The reply block is sized by the header, not by the class of its cell (D8).
const _: () = assert!(size_of::<OnionSurb>() <= 3 * 1024);

/// A byte string whose length is no loop class: no cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0} bytes is not the length of any onion loop class")]
pub(crate) struct OnionCellWidth(pub(crate) usize);

/// Why a reply block did not produce its cell.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OnionProduceError {
    /// A key of `σ_out`'s segment is weak: the reply block is unusable and is dropped.
    #[error(transparent)]
    Key(#[from] KeyError),
    /// The value does not fit the class; the reply block comes back unspent.
    #[error("{width}")]
    ValueTooWide {
        /// The unspent reply block.
        surb: Box<OnionSurb>,
        /// `|v′|` against the class capacity.
        width: OnionValueTooWide,
    },
}

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
    /// Every step of the parse can fail only on a width: `|w|` is no class length, `|w| < |χ|`,
    /// or `|w| − |χ| < τ`. Once `|w|` is a class length the other two cannot hold, so the one
    /// error, the width, is the exact reason for any failure.
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
    /// Return the head of `λ_i`, for the admission and routing decisions made before the carry
    /// step.
    pub(crate) const fn head(&self) -> &OnionLayerHead {
        &self.layer.head
    }

    /// The carry step of this position, by `λ_i`'s application; it consumes both seeds:
    ///
    /// ```text
    /// relay     y_i = Dec⁰_{KDF₄₈(σ_in)}(y_{i−1})                 ⇒ Relayed(head, cell)
    /// f ∈ Σ_W   v = pad⁻¹ Dec^τ_{KDF₄₈(σ_in)}(y_{i−1})            ⇒ Consumed(head, v, υ)
    ///           υ = (next, χ_{i+1}, σ_out, x)
    /// ```
    ///
    /// # Errors
    ///
    /// [`OnionStepError::Key`] for a weak key, and [`OnionStepError::Open`] if a symbol hop
    /// rejects its input.
    pub(crate) fn step(self) -> Result<OnionStep, OnionStepError> {
        let OnionLayer {
            head,
            inbound,
            outbound,
        } = self.layer;
        let key = inbound.key()?;
        match head.application {
            OnionLayerApplication::Relay => {
                let mut carry = self.carry;
                carry::peel(&key, &mut carry);
                Ok(OnionStep::Relayed {
                    head,
                    cell: OnionCell {
                        class: self.class,
                        header: self.next,
                        carry,
                        buffer: self.buffer,
                    },
                })
            }
            OnionLayerApplication::Apply { .. } => {
                let value = carry::open(&key, self.carry)?;
                let surb = OnionSurb {
                    class: self.class,
                    next: head.next,
                    header: self.next,
                    outbound,
                    expires_at_ms: head.expires_at_ms,
                };
                Ok(OnionStep::Consumed { head, value, surb })
            }
        }
    }
}

impl OnionSurb {
    /// `x_υ`: the pool drops the block at `x_υ` and spends the least `x` first (D8).
    pub(crate) const fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }

    /// The widest value this reply block can carry, `C₀ − 1` of its class.
    pub(crate) const fn capacity(&self) -> usize {
        self.class.value_capacity()
    }

    /// Spend the reply block: `v′` sealed under the segment keys of `σ_υ`, as a cell of its class
    /// in a fresh buffer, with the DID it goes to.
    ///
    /// # Errors
    ///
    /// [`OnionProduceError::Key`] for a weak key of `σ_υ`, and
    /// [`OnionProduceError::ValueTooWide`] if `|v′| > capacity()`, which returns the block.
    pub(crate) fn produce(self, value: &[u8]) -> Result<(Did, OnionCell), OnionProduceError> {
        let keys = self.outbound.keys()?;
        match carry::seal(self.class, &keys, value) {
            Ok(carry) => Ok((self.next, OnionCell {
                class: self.class,
                carry,
                header: self.header,
                buffer: Vec::with_capacity(self.class.cell_bytes()),
            })),
            Err(width) => Err(OnionProduceError::ValueTooWide {
                surb: Box::new(self),
                width,
            }),
        }
    }
}
