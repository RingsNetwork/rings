//! The cell on a loop edge, `cell = χ ‖ y`, as one owned buffer with one parser (#834 D6, Hop
//! line 1).
//!
//! ```text
//! parse    : {0,1}^* ⇀ OnionCell,   w ↦ (b, w)       b = |w| (F = 0), χ = w[0, |χ|), y = w[|χ|, b)
//! to_bytes : OnionCell → {0,1}^*,   (b, w) ↦ w
//! to_bytes ∘ parse = id on its domain,   parse ∘ to_bytes = Some
//! ```
//!
//! The class `b` is never an argument at a hop: it is the observed length, fixed by [`parse`],
//! and every use downstream (the MAC label of `γ`, the width `C_b` of the slot, the class of the
//! forwarded cell) reads it from the cell. So a relabelled cell (#834 H1) can only arrive as a
//! cell of the other length, whose `γ` fails at the next honest hop.
//!
//! **Zero copy** (#843). A cell is exactly its `b`-byte buffer. The header is the view
//! `w[0, |χ|)` and the slot the view `w[|χ|, b)`: a relay peels `χ_i` into `χ_{i+1}` in place and
//! deciphers the slot in place, so the buffer it received is the buffer it forwards, with no
//! allocation and no copy of the slot; a consumer's value is a view into the same buffer.
//!
//! **Paid peeling** (#834 L9, #843). A hop's step is the chain
//!
//! ```text
//!           charge (admission, u(b))          peel (χ under d_i)            admit (e, x, ν)
//! OnionCell ────────────────────▶ Charged<OnionCell> ──────────▶ Charged<OnionPeeledCell> ─────▶
//!
//!                         step, by λ_i's application
//! OnionAdmittedCell ──┬─ relay:  Dec⁰ under KDF₄₈(σ_in)  ──▶ Relayed(next, OnionCell)
//!                     └─ symbol: Dec^τ under KDF₄₈(σ_in) ──▶ Consumed(f, ā, v, OnionSurb)
//!
//! OnionSurb υ = (next, χ_{i+1}, σ_out, x) ── produce(v′), keys of σ_out ──▶ (next, OnionCell)
//! ```
//!
//! and each arrow is the only way to its target type. [`Charged`] pairs a cell with the affine
//! token of the one charge admission took for it: admission's `charge` is the only source of
//! tokens and builds the `Charged` itself, the fields are private to this module, and this module
//! destructures a `Charged` only to peel its own cell or to admit its own layer. So every ECDH is
//! paid for, exactly once, with the units of the cell's own class, and a peeled layer is admitted
//! with the token of its own cell; a cell that is not charged cannot be peeled at all.
//!
//! The class flows along every arrow unchanged, the role is the layer's application, and the
//! output's keys come from `σ_out` in the hop's own layer: a hop supplies none of the three.
//! [`OnionSurb`] is D8's reply block `υ`, 2979 B (`next`, `χ`, `σ`, `x`), plus the class of its
//! cell, so a SURB pool costs that per entry whatever the class; its keys are derived when it is
//! spent. The client builds a loop's first cell with [`OnionCell::client`] and reads a returning
//! cell through [`OnionCell::loop_tag`] and [`OnionCell::open`].
//!
//! [`parse`]: OnionCell::parse

use core::fmt;

use rand::CryptoRng;
use rand::RngCore;
use rings_aez::KeyError;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::PublicKeyAddress;

use super::carry;
use super::carry::OnionCarryValue;
use super::carry::OnionOpenError;
use super::carry::OnionValueTooWide;
use super::class::OnionLoopClass;
use super::header::peel_in_place;
use super::header::OnionBlindingError;
use super::header::OnionHeader;
use super::header::OnionHeaderRoute;
use super::header::OnionLoopTag;
use super::header::OnionPeelError;
use super::header::ONION_HEADER_BYTES;
use super::header::ONION_HEADER_MAC_BYTES;
use super::layer::OnionArguments;
use super::layer::OnionLayer;
use super::layer::OnionLayerApplication;
use super::layer::OnionLayerHead;
use super::seed::OnionCarryKey;
use super::seed::OnionSegmentKeys;
use super::seed::OnionSegmentSeed;
use super::seed::ONION_CARRY_SEED_BYTES;
use crate::onion::circuit::OnionAdmissionCharge;
use crate::onion::circuit::OnionAdmissionLayer;
use crate::onion::circuit::OnionAdmissionRejection;
use crate::onion::circuit::OnionAdmissionState;
use crate::onion::circuit::OnionExpiry;
use crate::onion::OnionServiceName;

/// `|υ| = |next| + |χ| + |σ| + |x| = 20 + 2919 + 32 + 8 = 2979`, the encoded width of a reply block
/// in a `credit` frame (#834 D8).
pub(crate) const ONION_SURB_BYTES: usize =
    ONION_DID_BYTES + ONION_HEADER_BYTES + ONION_CARRY_SEED_BYTES + ONION_EXPIRY_BYTES;

/// `|next|`, a raw DID.
const ONION_DID_BYTES: usize = 20;

/// `|x|`, big-endian milliseconds.
const ONION_EXPIRY_BYTES: usize = 8;

// D8's figure: a reply block is 2979 bytes, whatever the class of its cell.
const _: () = assert!(ONION_SURB_BYTES == 2979);

/// One cell `(b, w)` of a loop edge, `w = χ ‖ y`.
///
/// Invariant: `|bytes| = b`, established by [`OnionCell::parse`] and by every constructor, which
/// takes the class of the cell it continues or of the client's choice.
pub(crate) struct OnionCell {
    /// `b`, the observed or chosen cell length.
    class: OnionLoopClass,
    /// `w = χ ‖ y`, exactly `b` bytes.
    bytes: Vec<u8>,
}

/// A value together with the affine token of the one admission charge taken for it.
///
/// It is built only by admission's `charge`, which charges the units of the cell's own class, and
/// it is taken apart only inside this module, by the step that consumes it. See the module
/// documentation for why that makes paid peeling structural.
pub(crate) struct Charged<C> {
    /// The charged value: a cell, then its peeled form.
    value: C,
    /// The token of its charge.
    charge: OnionAdmissionCharge,
}

/// A cell whose header this hop has peeled: `λ_i`, and the buffer now holding `χ_{i+1} ‖ y_{i−1}`.
pub(crate) struct OnionPeeledCell {
    /// `b`, the class of the received cell and of the cell this hop forwards.
    class: OnionLoopClass,
    /// `λ_i`.
    layer: OnionLayer,
    /// `χ_{i+1} ‖ y_{i−1}`: the received buffer, with the next header already in place.
    bytes: Vec<u8>,
}

/// A peeled cell whose layer admission has admitted (epoch, window, replay): the only value with
/// a carry step.
pub(crate) struct OnionAdmittedCell(OnionPeeledCell);

/// The outcome of a hop's carry step, chosen by its layer's application. The step consumes both
/// seeds of `λ_i`; admission has already read the rest of its head.
pub(crate) enum OnionStep {
    /// A relay removed one AEZ layer: the cell it forwards.
    Relayed {
        /// `next_i`, where the cell goes.
        next: Did,
        /// The forwarded cell, of the received class, in the received buffer.
        cell: OnionCell,
    },
    /// A symbol hop received its input: the application, its value and the reply block of its
    /// output.
    Consumed {
        /// `f_i`.
        symbol: OnionServiceName,
        /// `ā_i`.
        arguments: OnionArguments,
        /// `v`, a view into the received buffer, zeroized on drop.
        value: OnionCarryValue,
        /// The reply block `υ = (next, χ_{i+1}, σ_out, x)` that produces the forwarded cell,
        /// boxed: it is the size of a header, the relay variant a buffer handle.
        surb: Box<OnionSurb>,
    },
}

/// A single-use reply block `υ = (next, χ_υ, σ_υ, x_υ)` (#834 D8), with the class of its cell:
/// where the output goes, its header, the segment seed of its carry, and its expiry.
///
/// It holds neither a cell buffer nor derived keys, so its size is independent of the class: D8's
/// 2979 B (`next`, `χ`, `σ`, `x`) plus the class. Affine: no `Clone`, so a reply block is spent at
/// most once; a value too wide for its class hands it back unspent.
pub(crate) struct OnionSurb {
    /// `b`, the class of the cell it produces.
    class: OnionLoopClass,
    /// `next`, the DID the produced cell goes to.
    next: Did,
    /// `χ_υ`.
    header: OnionHeader,
    /// `σ_υ`, zeroized on drop. A move of the block copies it bitwise without a drop of the
    /// source, so a buffer a block left may still hold it: the builder sizes its vector of blocks
    /// once, and the seed is zeroized wherever the block ends.
    outbound: OnionSegmentSeed,
    /// `x_υ`.
    expiry: OnionExpiry,
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
    /// The cell is narrower than a header. The class invariant (`|w| ≥ |χ| + τ` for every
    /// class) excludes it, so this is a typed impossibility rather than a panic.
    #[error("onion cell narrower than its header")]
    Width,
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
    /// `w ↦ (b, w)` with `b = |w|`: the one parser of a received cell. The payload is copied
    /// only once its width is a class length, and the copy is the cell's buffer from then on.
    ///
    /// The only condition is the width: once `|w|` is a class length, `|w| ≥ |χ| + τ` holds for
    /// every class, and the header fields are read when the header is peeled.
    ///
    /// # Errors
    ///
    /// [`OnionCellWidth`] unless `|w|` is the length of a loop class.
    pub(crate) fn parse(payload: &[u8]) -> Result<Self, OnionCellWidth> {
        let class = Self::class_of(payload.len())?;
        Ok(Self {
            class,
            bytes: payload.to_vec(),
        })
    }

    /// The width judge: the class whose cell length is `width`.
    fn class_of(width: usize) -> Result<OnionLoopClass, OnionCellWidth> {
        OnionLoopClass::from_cell_bytes(width).ok_or(OnionCellWidth(width))
    }

    /// `w = χ ‖ y`, exactly `b` bytes: the cell's own buffer.
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Return the class `b`.
    pub(crate) const fn class(&self) -> OnionLoopClass {
        self.class
    }

    /// A class-`b` cell of `header` and a slot sealed with `value` under `keys`, in a fresh
    /// `b`-byte buffer: the constructor behind the client's first cell and every SURB reply.
    fn seal(
        class: OnionLoopClass,
        header: &OnionHeader,
        keys: &OnionSegmentKeys,
        value: &[u8],
    ) -> Result<Self, OnionValueTooWide> {
        let mut bytes = Vec::with_capacity(class.cell_bytes());
        header.encode_into(&mut bytes);
        bytes.resize(class.cell_bytes(), 0);
        let slot = bytes.get_mut(ONION_HEADER_BYTES..).unwrap_or_default();
        carry::seal(class, keys, value, slot)?;
        Ok(Self { class, bytes })
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
        let (header, tag) = OnionHeader::build(route, class, rng)?;
        Ok((Self::seal(class, &header, keys, value)?, tag))
    }

    /// The client tag `t_⋄` of a cell arriving at the client, position `H + 1`: the `γ` field.
    pub(crate) fn loop_tag(&self) -> OnionLoopTag {
        let mut tag = [0; ONION_HEADER_MAC_BYTES];
        tag.iter_mut()
            .zip(
                self.bytes
                    .get(ONION_HEADER_BYTES - ONION_HEADER_MAC_BYTES..ONION_HEADER_BYTES)
                    .unwrap_or_default(),
            )
            .for_each(|(slot, byte)| *slot = *byte);
        OnionLoopTag::new(tag)
    }

    /// The client's view of a returning cell: the last segment's value under `k_{c_n}`, opened in
    /// place.
    ///
    /// # Errors
    ///
    /// The [`OnionOpenError`] of the consumer's check.
    pub(crate) fn open(self, key: &OnionCarryKey) -> Result<OnionCarryValue, OnionOpenError> {
        let end = self.bytes.len();
        carry::open(key, self.bytes, ONION_HEADER_BYTES..end)
    }

    /// Peel `χ_i` in place under the hop's key, with the class the cell arrived in. Private: the
    /// only caller is [`Charged::peel`], so an uncharged cell is never peeled.
    fn peel(mut self, key: &DelegateeKey) -> Result<OnionPeeledCell, OnionPeelError> {
        let header = self
            .bytes
            .first_chunk_mut::<ONION_HEADER_BYTES>()
            .ok_or(OnionPeelError::Invalid)?;
        let layer = peel_in_place(header, self.class, key)?;
        Ok(OnionPeeledCell {
            class: self.class,
            layer,
            bytes: self.bytes,
        })
    }
}

impl fmt::Debug for OnionCell {
    /// Shows the class only: the bytes are ciphertext of no diagnostic value.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OnionCell")
            .field("class", &self.class)
            .finish_non_exhaustive()
    }
}

impl<C> Charged<C> {
    /// Pair `value` with the token of its charge: admission's `charge` pairs a cell with the
    /// token it just built, and [`Charged::peel`] carries the same token over to the peeled
    /// cell. A bare token exists nowhere else, so no other pairing is constructible.
    pub(in crate::onion) const fn new(value: C, charge: OnionAdmissionCharge) -> Self {
        Self { value, charge }
    }

    /// The charged value.
    pub(crate) const fn value(&self) -> &C {
        &self.value
    }
}

impl Charged<OnionCell> {
    /// Settle the charge without peeling: a cell whose `γ` is one of the client's own tags
    /// (position `H + 1`, D6′) returns to the client, whose tag entry holds its key. Dropping the
    /// token settles the charge, as for an invalid `α` or `γ`; no ECDH is spent.
    pub(crate) fn settle(self) -> OnionCell {
        self.value
    }

    /// Peel the charged cell's header under the hop's key; the token stays with the peeled
    /// cell. A failed peel drops the token, which settles the charge (#834 L9: an invalid `α` or
    /// `γ` is paid for).
    ///
    /// # Errors
    ///
    /// The [`OnionPeelError`] of the header.
    pub(crate) fn peel(
        self,
        key: &DelegateeKey,
    ) -> Result<Charged<OnionPeeledCell>, OnionPeelError> {
        let Self { value, charge } = self;
        value.peel(key).map(|peeled| Charged::new(peeled, charge))
    }
}

impl Charged<OnionPeeledCell> {
    /// Admit the peeled layer with the token of its own cell: epoch, window at the charge's
    /// arrival, and replay (#834 L9). A rejection keeps the charge taken and drops the cell.
    ///
    /// # Errors
    ///
    /// The [`OnionAdmissionRejection`] of the admission step.
    pub(crate) fn admit(
        self,
        admission: &mut OnionAdmissionState,
        now_ms: u128,
    ) -> Result<OnionAdmittedCell, OnionAdmissionRejection> {
        let Self { value, charge } = self;
        let head = &value.layer.head;
        admission.admit(now_ms, charge, OnionAdmissionLayer {
            epoch: head.epoch,
            expiry: head.expiry,
            tag: head.nonce,
        })?;
        Ok(OnionAdmittedCell(value))
    }
}

impl OnionAdmittedCell {
    /// Return the head of `λ_i`, for the decisions made before the carry step.
    pub(crate) const fn head(&self) -> &OnionLayerHead {
        &self.0.layer.head
    }

    /// The whole decrypted `λ_i`, head and seeds, for the non-interference law.
    #[cfg(test)]
    pub(crate) const fn layer(&self) -> &OnionLayer {
        &self.0.layer
    }

    /// The carry step of this position, by `λ_i`'s application; it consumes both seeds:
    ///
    /// ```text
    /// relay     y_i = Dec⁰_{KDF₄₈(σ_in)}(y_{i−1})   in place         ⇒ Relayed(next, cell)
    /// f ∈ Σ_W   v = pad⁻¹ Dec^τ_{KDF₄₈(σ_in)}(y_{i−1})  in place     ⇒ Consumed(f, ā, v, υ)
    ///           υ = (next, χ_{i+1}, σ_out, x)
    /// ```
    ///
    /// # Errors
    ///
    /// [`OnionStepError::Key`] for a weak key, and [`OnionStepError::Open`] if a symbol hop
    /// rejects its input.
    pub(crate) fn step(self) -> Result<OnionStep, OnionStepError> {
        let OnionPeeledCell {
            class,
            layer,
            mut bytes,
        } = self.0;
        let OnionLayer {
            head,
            inbound,
            outbound,
        } = layer;
        let key = inbound.key()?;
        match head.application {
            OnionLayerApplication::Relay => {
                carry::peel(
                    &key,
                    bytes.get_mut(ONION_HEADER_BYTES..).unwrap_or_default(),
                );
                Ok(OnionStep::Relayed {
                    next: head.next,
                    cell: OnionCell { class, bytes },
                })
            }
            OnionLayerApplication::Apply { symbol, arguments } => {
                let header = bytes
                    .first_chunk::<ONION_HEADER_BYTES>()
                    .map(OnionHeader::of)
                    .ok_or(OnionStepError::Width)?;
                let surb = Box::new(OnionSurb {
                    class,
                    next: head.next,
                    header,
                    outbound,
                    expiry: head.expiry,
                });
                let end = bytes.len();
                let value = carry::open(&key, bytes, ONION_HEADER_BYTES..end)?;
                Ok(OnionStep::Consumed {
                    symbol,
                    arguments,
                    value,
                    surb,
                })
            }
        }
    }
}

impl OnionSurb {
    /// A reply block the client builds for a return path (#834 D8): the header of the path, its
    /// first hop and the segment seed of its carry, at the loop's expiry.
    pub(crate) const fn new(
        class: OnionLoopClass,
        next: Did,
        header: OnionHeader,
        outbound: OnionSegmentSeed,
        expiry: OnionExpiry,
    ) -> Self {
        Self {
            class,
            next,
            header,
            outbound,
            expiry,
        }
    }

    /// `x_υ`: the pool drops the block at `x_υ` and spends the least `x` first (D8).
    pub(crate) const fn expiry(&self) -> OnionExpiry {
        self.expiry
    }

    /// Return the class `b` of the cell it produces.
    pub(crate) const fn class(&self) -> OnionLoopClass {
        self.class
    }

    /// `next_υ`, for the non-interference law.
    #[cfg(test)]
    pub(crate) const fn next(&self) -> Did {
        self.next
    }

    /// `σ_υ`, for the seed law.
    #[cfg(test)]
    pub(crate) const fn outbound(&self) -> &OnionSegmentSeed {
        &self.outbound
    }

    /// The widest value this reply block can carry, `C₀ − 1` of its class.
    #[cfg(test)]
    pub(crate) const fn capacity(&self) -> usize {
        self.class.value_capacity()
    }

    /// `υ = next ‖ χ_υ ‖ σ_υ ‖ x_υ`, the 2979-byte encoding a `credit` frame carries (D8); the
    /// class is the carrying loop's and is not encoded.
    pub(crate) fn encode_into(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&PublicKeyAddress::from(self.next).to_fixed_bytes());
        self.header.encode_into(bytes);
        bytes.extend_from_slice(self.outbound.as_bytes());
        bytes.extend_from_slice(&self.expiry.to_wire_ms().to_be_bytes());
    }

    /// The left inverse of [`Self::encode_into`] for a block carried in a class-`class` loop:
    /// `None` unless `|bytes| = |υ|` and `x_υ` lies on the grid.
    pub(crate) fn decode(class: OnionLoopClass, bytes: &[u8]) -> Option<Self> {
        let (next, rest) = bytes.split_first_chunk::<ONION_DID_BYTES>()?;
        let (header, rest) = rest.split_first_chunk::<ONION_HEADER_BYTES>()?;
        let (outbound, rest) = rest.split_first_chunk::<ONION_CARRY_SEED_BYTES>()?;
        let expiry = <[u8; ONION_EXPIRY_BYTES]>::try_from(rest).ok()?;
        Some(Self {
            class,
            next: Did::from(PublicKeyAddress::from(*next)),
            header: OnionHeader::of(header),
            outbound: OnionSegmentSeed::new(*outbound),
            expiry: OnionExpiry::from_wire_ms(u64::from_be_bytes(expiry))?,
        })
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
        match OnionCell::seal(self.class, &self.header, &keys, value) {
            Ok(cell) => Ok((self.next, cell)),
            Err(width) => Err(OnionProduceError::ValueTooWide {
                surb: Box::new(self),
                width,
            }),
        }
    }
}

impl fmt::Debug for OnionSurb {
    /// Shows the routing facts only: the header and the seed are key material of the loop.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OnionSurb")
            .field("class", &self.class)
            .field("next", &self.next)
            .field("expiry", &self.expiry)
            .finish_non_exhaustive()
    }
}
