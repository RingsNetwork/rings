//! Unified onion-layer admission: process epoch, validity window, replay store and unit budgets
//! (#834 L9, D2).
//!
//! # Step
//!
//! One hop's admission is a deterministic transition with no effect:
//!
//! ```text
//! δ : S × Time × I → S × (1 + Rejection)       I = Did × Epoch × Expiry × Tag × Units
//! ```
//!
//! [`OnionAdmissionState::admit`] realises `δ` in place. Time is an argument and the only source
//! of randomness is the probe key given at construction, so a trace of inputs determines the
//! trace of verdicts. For an input `(from, e, x, ν, u)` arriving at `arr`, hop `i` admits iff
//!
//! ```text
//! e = epoch_i  ∧  arr < x ≤ arr + V  ∧  ν ∉ R_i[x]
//!              ∧  load_from + u ≤ B   ∧  load_global + u ≤ 64·B
//! ```
//!
//! and then commits `R_i[x] ← R_i[x] ∪ {ν}` and both unit charges together. Every rejection
//! leaves the budgets, the sender partitions and the replay store unchanged. Only the clock and
//! the expired filters advance, as described below.
//!
//! # Laws
//!
//! * **Clock.** The state's clock is the join of every `now` it has seen: `now := max(now, clock)`.
//!   A wall-clock rollback therefore cannot refund budget or bring back a dropped filter. A layer
//!   that a dropped filter would have caught has `x ≤ clock` and is rejected by the window.
//! * **Epoch (D2).** Only layers sealed for the current process epoch are admitted. A restarted
//!   process draws a fresh epoch, so every layer of the previous process is rejected, even though
//!   the replay store is lost with that process.
//! * **At most once (L9).** The expiry grid is `x ∈ Q·ℕ` ([`OnionExpiry`] is constructible only
//!   on it). `R_i[x]` is live exactly while `now < x` and is dropped as a whole when `x` passes.
//!   While it is live it has no false negatives, and once it is gone the window rejects `x`. So a
//!   pair `(x, ν)` is admitted at most once, and there are at most `V / Q = 5` live filters (the
//!   grid points of `(now, now + V]`), within the paper's bound `V / Q + 1 = 6`.
//! * **Five quanta per filter.** `arr < x ≤ arr + V` gives `arr ∈ [x − V, x)`. With `x = kQ` and
//!   `V = 5Q`, the arrival quanta are exactly `{k − 5, …, k − 1}`: five consecutive aligned quanta.
//!   This set is contained in the ledger window `(s − 5, s]` of the last admission into `R_i[x]`,
//!   at quantum `s ≤ k − 1`. That check bounded the whole set by the global cap.
//! * **Tags ≤ units.** Tags are counted per cell and budgets per unit, and every cell costs at
//!   least one unit. So `|R_i[x]| ≤ 64·B`, `R_i[x]` has at most `64` blocks, and its
//!   false-positive rate is `≤ 64·2⁻²⁶ = 2⁻²⁰`. A tag is live only if it was admitted within the
//!   current window, so all live filters together hold `≤ 64·B` tags in `≤ 64 + 5` blocks
//!   (`≈ 5.3 MB`).
//! * **Budgets.** Over any five aligned arrival quanta, one sender partition is charged `≤ B`
//!   units and the hop `≤ 64·B`. That is `B / V ≈ 109` units/s per sender and `≈ 6990` units/s in
//!   total.
//! * **Partition recycling.** At most `64` sender partitions exist. A new sender beyond them takes
//!   the slot of the least recently active partition (ties broken by DID) with a fresh ledger.
//!   Recycling never lowers any sender's remaining budget, since the evicted sender also returns
//!   with a fresh ledger. The global cap bounds identity rotation: however many identities a
//!   sender cycles through, the hop admits `≤ 64·B` units per window. An honest sender loses its
//!   slot only when 64 other senders have been active more recently, i.e. only when more than 64
//!   senders compete.

mod bloom;
mod ledger;
#[cfg(all(test, rings_native))]
mod tests;

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use rings_core::dht::Did;

pub(crate) use self::bloom::OnionReplayFilterKey;
use self::bloom::ReplayStore;
use self::ledger::QuantumLedger;
use super::OnionForwardNonce;
use super::ONION_FORWARD_EXPIRY_QUANTUM_MS;
use super::ONION_FORWARD_MAX_VALIDITY_MS;
use crate::onion::OnionExitEpoch;

/// Admission window `V = 150 s`: a layer is admissible at `arr` iff `arr < x ≤ arr + V`.
const ONION_ADMISSION_WINDOW_MS: u128 = ONION_FORWARD_MAX_VALIDITY_MS;

/// Arrival quanta per window, `N = V / Q = 5`, as the ledger's array length.
const ADMISSION_WINDOW_QUANTA: usize = 5;

/// `N = V / Q` in the quantum domain.
const ADMISSION_WINDOW_QUANTA_WIDE: u128 =
    ONION_ADMISSION_WINDOW_MS / ONION_FORWARD_EXPIRY_QUANTUM_MS;

/// Per-sender budget `B`: units of 16 KiB per window `V`.
const ONION_ADMISSION_SENDER_UNITS: u32 = 16_384;

/// Maximum number of sender partitions.
const ONION_ADMISSION_SENDERS: usize = 64;

/// Global budget `64·B` units per window `V`.
const ONION_ADMISSION_GLOBAL_UNITS: u32 = 64 * ONION_ADMISSION_SENDER_UNITS;

/// Compile-time laws: `V` is a whole number `N` of quanta, and the global budget is
/// `ONION_ADMISSION_SENDERS · B`.
const _: () = assert!(
    ONION_ADMISSION_WINDOW_MS.is_multiple_of(ONION_FORWARD_EXPIRY_QUANTUM_MS)
        && ADMISSION_WINDOW_QUANTA_WIDE == ADMISSION_WINDOW_QUANTA as u128
        && ONION_ADMISSION_GLOBAL_UNITS as usize
            == ONION_ADMISSION_SENDERS * ONION_ADMISSION_SENDER_UNITS as usize
);

/// A layer's quantised expiry `x ∈ Q·ℕ`, in milliseconds since the Unix epoch.
///
/// An off-grid instant is unrepresentable, which keeps the replay store's keys on the grid and
/// the number of live filters at most `V / Q`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OnionExpiry(u128);

impl OnionExpiry {
    /// The expiry at `ms` if it lies on the grid `Q·ℕ`, else `None`.
    pub(crate) fn from_ms(ms: u128) -> Option<Self> {
        ms.is_multiple_of(ONION_FORWARD_EXPIRY_QUANTUM_MS)
            .then_some(Self(ms))
    }

    /// The expiry instant in milliseconds.
    pub(crate) const fn as_ms(self) -> u128 {
        self.0
    }
}

/// Units of 16 KiB charged for one cell. A class-`b` cell costs `b / 16 KiB ≥ 1` units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionAdmissionUnits(NonZeroU32);

impl OnionAdmissionUnits {
    /// Wrap a positive unit count.
    pub(crate) const fn new(units: NonZeroU32) -> Self {
        Self(units)
    }

    /// The unit count.
    const fn get(self) -> u32 {
        self.0.get()
    }
}

/// The input alphabet `I` of the admission step: one peeled layer and its immediate sender.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionAdmissionRequest {
    /// Authenticated immediate sender, which owns the budget partition.
    pub(crate) from: Did,
    /// Process epoch the layer was sealed for.
    pub(crate) epoch: OnionExitEpoch,
    /// Quantised expiry `x` of the layer's loop.
    pub(crate) expiry: OnionExpiry,
    /// Replay tag `ν` of the layer.
    pub(crate) tag: OnionForwardNonce,
    /// Units charged for the cell's class.
    pub(crate) units: OnionAdmissionUnits,
}

/// Why a layer was not admitted. Every rejection drops the cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnionAdmissionRejection {
    /// The layer was sealed for another process epoch (D2).
    StaleEpoch,
    /// `x ∉ (arr, arr + V]`.
    OutsideWindow,
    /// `ν ∈ R_i[x]`: a replay, or a false positive at rate `≤ 2⁻²⁰`.
    Replayed,
    /// The sender's partition would exceed `B` units in the window.
    SenderBudget,
    /// The hop would exceed `64·B` units in the window.
    GlobalBudget,
}

/// One sender's budget partition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SenderPartition {
    /// Units charged to this sender in the window.
    ledger: QuantumLedger,
    /// Clock at this sender's last admission, the recycling order.
    last_active_ms: u128,
}

/// The admission state `S` of one hop for one process lifetime.
pub(crate) struct OnionAdmissionState {
    /// This process's epoch `epoch_i`.
    epoch: OnionExitEpoch,
    /// Monotone clock: the greatest `now` seen so far.
    clock_ms: u128,
    /// Replay store `R_i`.
    replay: ReplayStore,
    /// Units charged to the whole hop.
    global: QuantumLedger,
    /// At most [`ONION_ADMISSION_SENDERS`] sender partitions.
    senders: BTreeMap<Did, SenderPartition>,
}

impl OnionAdmissionState {
    /// The initial state of a process with epoch `epoch` and probe key `filter_key`, both drawn
    /// once at process start by the caller.
    pub(crate) fn new(epoch: OnionExitEpoch, filter_key: OnionReplayFilterKey) -> Self {
        Self {
            epoch,
            clock_ms: 0,
            replay: ReplayStore::new(filter_key),
            global: QuantumLedger::default(),
            senders: BTreeMap::new(),
        }
    }

    /// The step `δ`: admit or reject one layer arriving at `now_ms`.
    ///
    /// ```text
    ///  now := max(now, clock);  drop R[x] for every x ≤ now
    ///   │
    ///   ├─ e ≠ epoch ─────────────────────────────→ StaleEpoch
    ///   ├─ x ∉ (now, now + V] ────────────────────→ OutsideWindow
    ///   ├─ ν ∈ R[x] ──────────────────────────────→ Replayed
    ///   ├─ load_from(s) + u > B ──────────────────→ SenderBudget
    ///   ├─ load_global(s) + u > 64·B ─────────────→ GlobalBudget
    ///   │                                          (s = ⌊now / Q⌋; nothing committed so far)
    ///   └─ commit: recycle the LRU partition if `from` is new and 64 exist;
    ///              charge both ledgers; R[x] ← R[x] ∪ {ν}  ─→ Ok
    /// ```
    pub(crate) fn admit(
        &mut self,
        now_ms: u128,
        request: OnionAdmissionRequest,
    ) -> Result<(), OnionAdmissionRejection> {
        self.clock_ms = self.clock_ms.max(now_ms);
        let now_ms = self.clock_ms;
        self.replay.forget_through(now_ms);
        if request.epoch != self.epoch {
            return Err(OnionAdmissionRejection::StaleEpoch);
        }
        let expiry_ms = request.expiry.as_ms();
        if expiry_ms <= now_ms || expiry_ms > now_ms.saturating_add(ONION_ADMISSION_WINDOW_MS) {
            return Err(OnionAdmissionRejection::OutsideWindow);
        }
        let probe = self.replay.probe(request.tag);
        if self.replay.contains(request.expiry, &probe) {
            return Err(OnionAdmissionRejection::Replayed);
        }
        let quantum = now_ms / ONION_FORWARD_EXPIRY_QUANTUM_MS;
        let units = request.units.get();
        let sender = self
            .senders
            .get(&request.from)
            .map(|partition| partition.ledger)
            .unwrap_or_default()
            .charged(quantum, units, ONION_ADMISSION_SENDER_UNITS)
            .ok_or(OnionAdmissionRejection::SenderBudget)?;
        let global = self
            .global
            .charged(quantum, units, ONION_ADMISSION_GLOBAL_UNITS)
            .ok_or(OnionAdmissionRejection::GlobalBudget)?;
        if !self.senders.contains_key(&request.from)
            && self.senders.len() >= ONION_ADMISSION_SENDERS
        {
            // `BTreeMap` iterates in DID order and `min_by_key` keeps the first minimum, so ties
            // on the clock break towards the least DID and recycling is deterministic.
            if let Some(recycled) = self
                .senders
                .iter()
                .min_by_key(|(_, partition)| partition.last_active_ms)
                .map(|(did, _)| did.to_owned())
            {
                self.senders.remove(&recycled);
            }
        }
        self.senders.insert(request.from, SenderPartition {
            ledger: sender,
            last_active_ms: now_ms,
        });
        self.global = global;
        self.replay.insert(request.expiry, &probe);
        Ok(())
    }
}
