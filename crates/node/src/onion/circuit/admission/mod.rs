//! Unified onion-layer admission: process epoch, validity window, replay store and unit budgets
//! (#834 L9, D2).
//!
//! # Step
//!
//! One hop's admission is a deterministic transition with no effect:
//!
//! ```text
//! δ : S × Time × I → S × (1 + Rejection)
//! I = Cell(Did × Epoch × Expiry × Tag × Units) + Invalid(Did × Units)
//!   + LinkOpened(Did) + LinkClosed(Did)
//! ```
//!
//! [`OnionAdmissionState::admit`], [`OnionAdmissionState::charge_invalid`],
//! [`OnionAdmissionState::link_opened`] and [`OnionAdmissionState::link_closed`] realise `δ` on the
//! four summands in place. [`OnionAdmissionState::headroom`] is the query that the shell runs
//! before the ECDH. Time is an argument, and the only randomness is the probe key given at
//! construction, so a trace of inputs determines the trace of verdicts.
//!
//! The order follows the paper's hop algorithm:
//!
//! ```text
//! headroom(from, u)  →  ECDH, peel  →  γ fails: charge_invalid(from, u), drop
//!                                   →  γ holds: admit(from, e, x, ν, u)
//! admit = charge u  ;  e = epoch_i  ;  arr < x ≤ arr + V  ;  ν ∉ R_i[x]  ;  R_i[x] ← R_i[x] ∪ {ν}
//! ```
//!
//! # Laws
//!
//! * **Clock: safety.** The state's clock is the join of every `now` it has seen,
//!   `now := max(now, clock)`. A wall-clock rollback therefore cannot refund budget or revive a
//!   dropped filter. A layer that a dropped filter would have caught has `x ≤ clock`, so the window
//!   rejects it.
//! * **Clock: liveness cost.** Suppose the wall clock jumps forward by `Δ` and then back. `clock`
//!   stays pinned at the future instant. Honest layers are built from the true time, so for
//!   `Δ ≥ V` every one of them has `x ≤ clock` and is rejected until the wall clock catches up
//!   (about `Δ` later). Safety and liveness cannot both be kept inside one epoch: recovering early
//!   would mean forgetting filters that may still be live. So the effectful shell (#834 2a-4),
//!   when it sees `now < clock − V`, must start a fresh state under a fresh process epoch. That
//!   restart is safe by the epoch law.
//! * **Epoch (D2).** Only layers sealed for the current process epoch are admitted. A restarted
//!   process draws a fresh epoch, so every layer of the previous process is rejected, although the
//!   replay store was lost with that process.
//! * **At most once (L9).** The expiry grid is `x ∈ Q·ℕ`: an [`OnionExpiry`] is a quantum index,
//!   so it can only lie on the grid. `R_i[x]` is consulted only while `now < x`. It is dropped as a
//!   whole by the first step at or after `x`, because steps are the only way time enters the state.
//!   A live filter has no false negatives, and once `x` has passed the window rejects `x`. Hence a
//!   pair `(x, ν)` is admitted at most once. At most `V / Q = 5` filters are live, namely the grid
//!   points of `(clock, clock + V]`, within the paper's bound `V / Q + 1 = 6`. An idle hop keeps
//!   its filters until its next step. They stay within the memory bound below, and the 2a-4 shell
//!   decides whether to drive the step on a timer.
//! * **Five quanta per filter.** `arr < x ≤ arr + V` gives `arr ∈ [x − V, x)`. With `x = kQ` and
//!   `V = 5Q`, the arrival quanta are exactly `{k − 5, …, k − 1}`, five consecutive aligned quanta.
//!   They all lie in the ledger window `(s − 5, s]` of the last admission into `R_i[x]` (at quantum
//!   `s ≤ k − 1`), and that check bounded the whole set by the global cap.
//! * **Charging.** Every cell for which the hop computed its key is charged `u(b) = b / 16 KiB`
//!   exactly once: by `admit`, or by `charge_invalid` when `γ` fails. Replayed, expired and
//!   stale-epoch cells pay too, so a sender cannot spend the hop's ECDH work for free. A cell
//!   without headroom is rejected before it is charged and, through `headroom`, before the
//!   ECDH. That rejection changes nothing. A cell from a DID without a ledger is rejected the
//!   same way.
//! * **Tags ≤ units.** Tags are counted per cell and budgets per unit, and every cell costs at
//!   least one unit. So `|R_i[x]| ≤ 64·B`, `R_i[x]` has at most `64` blocks, and its
//!   false-positive rate is `≤ 64·2⁻²⁶ = 2⁻²⁰`. A tag is live only if it was admitted in the
//!   current ledger window, so all live filters together hold `≤ G = 64·B` tags in `≤ 64 + 5`
//!   blocks (`≈ 5.3 MB`).
//! * **Budgets.** Every aligned window of five arrival quanta charges one sender DID at most `B`
//!   units and the hop at most `G = 64·B`. An aligned window covers between `4Q` and `5Q` of wall
//!   time. So every interval of length `≤ 4Q` carries at most `B` units per sender. An interval
//!   just over `4Q` can straddle two windows and carry up to `2·B` (a burst at the end of quantum
//!   `s` and another at the start of `s + 5`). The long-run rate is `B / V ≈ 109` units/s per
//!   sender and `≈ 6990` units/s in total.
//! * **Sender ledgers.** A ledger is keyed by the DID of an authenticated link. It is created by
//!   [`OnionAdmissionState::link_opened`] and released only after
//!   [`OnionAdmissionState::link_closed`] *and* once its window load is zero. The table holds at
//!   most `2·R` ledgers, where `R` is the transport connection-registry capacity (#723). There is
//!   no recycling. Consequences:
//!   * A ledger is never reset while it carries load, so no DID regains budget by closing and
//!     reopening its link: a reconnecting DID finds its old ledger. `(iii)` of the paper's replay
//!     bounds therefore holds per DID, however many other DIDs churn.
//!   * At most `R` links are live, and a closed ledger drains within one window. So the table
//!     fills only under connection churn beyond `R` within `V`, and a link opened then fails
//!     closed.
//!   * `G` is independent of the table size and bounds what all DIDs admit together, and hence the
//!     replay store.

mod bloom;
mod ledger;
#[cfg(all(test, rings_native))]
mod tests;

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;

use rings_core::dht::Did;

pub(super) use self::bloom::OnionReplayFilterKey;
use self::bloom::ReplayStore;
use self::ledger::QuantumLedger;
use super::OnionForwardNonce;
use super::ONION_FORWARD_EXPIRY_QUANTUM_MS;
use super::ONION_FORWARD_MAX_VALIDITY_MS;
use super::ONION_FORWARD_PAYLOAD_TTL_MS;
use crate::onion::OnionExitEpoch;

/// Admission window `V = 150 s`: a layer is admissible at `arr` iff `arr < x ≤ arr + V`.
const ONION_ADMISSION_WINDOW_MS: u128 = ONION_FORWARD_MAX_VALIDITY_MS;

/// Arrival quanta per window, `N = V / Q = 5`, as the ledger's array length.
const ADMISSION_WINDOW_QUANTA: usize = 5;

/// `N = V / Q` in the quantum domain.
const ADMISSION_WINDOW_QUANTA_WIDE: u128 =
    ONION_ADMISSION_WINDOW_MS / ONION_FORWARD_EXPIRY_QUANTUM_MS;

/// Build-to-expiry offset `X₀ = V − Q` in quanta: `x = ⌈t_build / Q⌉·Q + X₀`.
const ONION_EXPIRY_OFFSET_QUANTA: u128 =
    ONION_FORWARD_PAYLOAD_TTL_MS / ONION_FORWARD_EXPIRY_QUANTUM_MS;

/// Per-sender budget `B`: units of 16 KiB per DID per aligned window of `N` quanta.
const ONION_ADMISSION_SENDER_UNITS: u32 = 16_384;

/// Global budget `G = 64·B` units per aligned window of `N` quanta. It is independent of the number
/// of links and bounds the replay store at 64 blocks per filter.
const ONION_ADMISSION_GLOBAL_UNITS: u32 = 64 * ONION_ADMISSION_SENDER_UNITS;

/// Compile-time laws: `V = N·Q`, and `X₀ = V − Q` is a whole number of quanta.
const _: () = assert!(
    ONION_ADMISSION_WINDOW_MS.is_multiple_of(ONION_FORWARD_EXPIRY_QUANTUM_MS)
        && ONION_FORWARD_PAYLOAD_TTL_MS.is_multiple_of(ONION_FORWARD_EXPIRY_QUANTUM_MS)
        && ADMISSION_WINDOW_QUANTA_WIDE == ADMISSION_WINDOW_QUANTA as u128
        && ONION_EXPIRY_OFFSET_QUANTA + 1 == ADMISSION_WINDOW_QUANTA_WIDE
);

/// A layer's quantised expiry `x ∈ Q·ℕ`, held as its quantum index `x / Q`.
///
/// An off-grid instant is unrepresentable, which keeps the replay store's keys on the grid and
/// the number of live filters at most `V / Q`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct OnionExpiry(u128);

impl OnionExpiry {
    /// The expiry of a loop built at `built_at_ms`: `x = ⌈t_build / Q⌉·Q + X₀`, the only source of
    /// the grid. Every build instant in one quantum maps to the same `x`, so a layer does not
    /// reveal the client's clock at a finer resolution than `Q`.
    pub(super) fn of_build(built_at_ms: u128) -> Self {
        Self(
            built_at_ms
                .div_ceil(ONION_FORWARD_EXPIRY_QUANTUM_MS)
                .saturating_add(ONION_EXPIRY_OFFSET_QUANTA),
        )
    }

    /// The expiry at `ms` if it lies on the grid `Q·ℕ`, else `None`.
    pub(super) fn from_ms(ms: u128) -> Option<Self> {
        ms.is_multiple_of(ONION_FORWARD_EXPIRY_QUANTUM_MS)
            .then_some(Self(ms / ONION_FORWARD_EXPIRY_QUANTUM_MS))
    }

    /// The expiry instant in milliseconds, saturating at the largest representable instant, which
    /// no window admits.
    pub(super) const fn as_ms(self) -> u128 {
        self.0.saturating_mul(ONION_FORWARD_EXPIRY_QUANTUM_MS)
    }

    /// The admission window: `arr < x ≤ arr + V`.
    pub(super) const fn admissible_at(self, arrival_ms: u128) -> bool {
        let expiry_ms = self.as_ms();
        arrival_ms < expiry_ms && expiry_ms <= arrival_ms.saturating_add(ONION_ADMISSION_WINDOW_MS)
    }

    /// Whether `x` has passed at `now`, i.e. `x ≤ now`, so that `R_i[x]` is dropped.
    const fn has_passed_at(self, now_ms: u128) -> bool {
        self.as_ms() <= now_ms
    }
}

/// Units of 16 KiB charged for one cell. A class-`b` cell costs `b / 16 KiB ≥ 1` units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OnionAdmissionUnits(NonZeroU32);

impl OnionAdmissionUnits {
    /// Wrap a positive unit count.
    pub(super) const fn new(units: NonZeroU32) -> Self {
        Self(units)
    }

    /// The unit count.
    const fn get(self) -> u32 {
        self.0.get()
    }
}

/// The input alphabet `I` of the admission step: one peeled layer and its immediate sender.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OnionAdmissionRequest {
    /// Authenticated immediate sender, whose link owns the charged ledger.
    pub(super) from: Did,
    /// Process epoch the layer was sealed for.
    pub(super) epoch: OnionExitEpoch,
    /// Quantised expiry `x` of the layer's loop.
    pub(super) expiry: OnionExpiry,
    /// Replay tag `ν` of the layer.
    pub(super) tag: OnionForwardNonce,
    /// Units charged for the cell's class.
    pub(super) units: OnionAdmissionUnits,
}

/// Why a cell was not admitted or charged. Every rejection drops the cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OnionAdmissionRejection {
    /// The layer was sealed for another process epoch (D2). The cell was charged.
    StaleEpoch,
    /// `x ∉ (arr, arr + V]`. The cell was charged.
    OutsideWindow,
    /// `ν ∈ R_i[x]`: a replay, or a false positive at rate `≤ 2⁻²⁰`. The cell was charged.
    Replayed,
    /// The sender's ledger lacks headroom for the cell's units. Nothing was charged.
    SenderBudget,
    /// The hop lacks headroom under `G = 64·B`. Nothing was charged.
    GlobalBudget,
    /// The cell's DID has no ledger, because no link from it was opened. Nothing was charged.
    UnlinkedSender,
    /// A link was opened while the ledger table was full (connection churn beyond `R` within `V`).
    SenderTableFull,
}

/// Whether the authenticated link of a ledger's DID is live.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SenderLink {
    /// The link is live.
    Open,
    /// [`OnionAdmissionState::link_closed`] was observed. The ledger is kept until it drains.
    Closed,
}

/// One sender DID's unit ledger and the state of its link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SenderLedger {
    /// Units charged to this DID in the window.
    ledger: QuantumLedger,
    /// Whether the DID's link is live.
    link: SenderLink,
}

impl SenderLedger {
    /// Whether the ledger may be released at `quantum`: its link is closed and it carries no load.
    fn is_releasable_at(&self, quantum: u128) -> bool {
        self.link == SenderLink::Closed && self.ledger.load(quantum) == 0
    }
}

/// The admission state `S` of one hop for one process lifetime.
pub(super) struct OnionAdmissionState {
    /// This process's epoch `epoch_i`.
    epoch: OnionExitEpoch,
    /// Monotone clock: the greatest `now` seen so far.
    clock_ms: u128,
    /// Replay store `R_i`.
    replay: ReplayStore,
    /// Units charged to the whole hop.
    global: QuantumLedger,
    /// Capacity `2·R` of the ledger table.
    sender_capacity: usize,
    /// At most `sender_capacity` sender ledgers.
    senders: BTreeMap<Did, SenderLedger>,
}

impl OnionAdmissionState {
    /// The initial state of a process with epoch `epoch` and probe key `filter_key`, both drawn
    /// once at process start by the caller. The ledger table holds `2·R` ledgers, where `R` is the
    /// transport connection-registry capacity.
    pub(super) fn new(
        epoch: OnionExitEpoch,
        filter_key: OnionReplayFilterKey,
        link_registry_capacity: NonZeroUsize,
    ) -> Self {
        Self {
            epoch,
            clock_ms: 0,
            replay: ReplayStore::new(filter_key),
            global: QuantumLedger::default(),
            sender_capacity: link_registry_capacity.get().saturating_mul(2),
            senders: BTreeMap::new(),
        }
    }

    /// The step `δ` on `LinkOpened(from)`: create `from`'s ledger, or reopen the one it left, which
    /// is never reset. A full table first releases the ledgers of closed, drained links, and then
    /// fails closed.
    pub(super) fn link_opened(
        &mut self,
        now_ms: u128,
        from: Did,
    ) -> Result<(), OnionAdmissionRejection> {
        let quantum = Self::quantum_of(self.advance(now_ms));
        if let Some(sender) = self.senders.get_mut(&from) {
            sender.link = SenderLink::Open;
            return Ok(());
        }
        if self.senders.len() >= self.sender_capacity {
            // Releasing a closed, drained ledger forgets nothing: it carries no load, and its
            // link is gone.
            self.senders
                .retain(|_, sender| !sender.is_releasable_at(quantum));
        }
        if self.senders.len() >= self.sender_capacity {
            return Err(OnionAdmissionRejection::SenderTableFull);
        }
        self.senders.insert(from, SenderLedger {
            ledger: QuantumLedger::default(),
            link: SenderLink::Open,
        });
        Ok(())
    }

    /// The step `δ` on `LinkClosed(from)`: mark `from`'s ledger closed, and release it at once if
    /// it carries no load. A closed ledger with load is released once it has drained and a new link
    /// needs its slot.
    pub(super) fn link_closed(&mut self, now_ms: u128, from: &Did) {
        let quantum = Self::quantum_of(self.advance(now_ms));
        if let Some(sender) = self.senders.get_mut(from) {
            sender.link = SenderLink::Closed;
            if sender.is_releasable_at(quantum) {
                self.senders.remove(from);
            }
        }
    }

    /// Whether a cell of `units` from `from` would be charged at `now`. This is the headroom check
    /// that the shell runs *before* the ECDH, so that a cell rejected for budget is never
    /// decrypted. It is a query and changes nothing.
    pub(super) fn headroom(
        &self,
        now_ms: u128,
        from: &Did,
        units: OnionAdmissionUnits,
    ) -> Result<(), OnionAdmissionRejection> {
        self.charges(Self::quantum_of(self.clock_ms.max(now_ms)), from, units)
            .map(|_| ())
    }

    /// The step `δ` on `Invalid(from, u)`: charge a cell whose `γ` check failed after its key was
    /// computed. The replay store is not touched, because a cell that failed `γ` has no
    /// authenticated `ν`.
    pub(super) fn charge_invalid(
        &mut self,
        now_ms: u128,
        from: &Did,
        units: OnionAdmissionUnits,
    ) -> Result<(), OnionAdmissionRejection> {
        let quantum = Self::quantum_of(self.advance(now_ms));
        self.charge(quantum, from, units)
    }

    /// The step `δ` on `Cell(from, e, x, ν, u)`: charge a peeled layer, then admit or reject it.
    ///
    /// ```text
    ///  now := max(now, clock);  drop R[x] for every x ≤ now;  s = ⌊now / Q⌋
    ///   │
    ///   ├─ from has no ledger ────────────────────→ UnlinkedSender  (nothing charged)
    ///   ├─ load_from(s) + u > B ──────────────────→ SenderBudget    (nothing charged)
    ///   ├─ load_global(s) + u > G ────────────────→ GlobalBudget    (nothing charged)
    ///   ├─ charge u to from's ledger and to the global ledger
    ///   ├─ e ≠ epoch ─────────────────────────────→ StaleEpoch      (charged)
    ///   ├─ ¬ x.admissible_at(now) ────────────────→ OutsideWindow   (charged)
    ///   ├─ ν ∈ R[x] ──────────────────────────────→ Replayed        (charged)
    ///   └─ R[x] ← R[x] ∪ {ν} ─────────────────────→ Ok              (charged)
    /// ```
    pub(super) fn admit(
        &mut self,
        now_ms: u128,
        request: OnionAdmissionRequest,
    ) -> Result<(), OnionAdmissionRejection> {
        let now_ms = self.advance(now_ms);
        self.charge(Self::quantum_of(now_ms), &request.from, request.units)?;
        if request.epoch != self.epoch {
            return Err(OnionAdmissionRejection::StaleEpoch);
        }
        if !request.expiry.admissible_at(now_ms) {
            return Err(OnionAdmissionRejection::OutsideWindow);
        }
        let probe = self.replay.probe(request.tag);
        if self.replay.contains(request.expiry, &probe) {
            return Err(OnionAdmissionRejection::Replayed);
        }
        self.replay.insert(request.expiry, &probe);
        Ok(())
    }

    /// Advance the monotone clock to `max(now, clock)`, drop every filter whose `x` has passed,
    /// and return the new clock.
    fn advance(&mut self, now_ms: u128) -> u128 {
        self.clock_ms = self.clock_ms.max(now_ms);
        self.replay.forget_through(self.clock_ms);
        self.clock_ms
    }

    /// The arrival quantum `s = ⌊t / Q⌋` of an instant.
    const fn quantum_of(now_ms: u128) -> u128 {
        now_ms / ONION_FORWARD_EXPIRY_QUANTUM_MS
    }

    /// Charge `units` to `from`'s ledger and to the global ledger at `quantum`, both or neither.
    fn charge(
        &mut self,
        quantum: u128,
        from: &Did,
        units: OnionAdmissionUnits,
    ) -> Result<(), OnionAdmissionRejection> {
        let (sender, global) = self.charges(quantum, from, units)?;
        self.senders
            .get_mut(from)
            .ok_or(OnionAdmissionRejection::UnlinkedSender)?
            .ledger = sender;
        self.global = global;
        Ok(())
    }

    /// The pair of charged ledgers `(from, global)` after `units` more at `quantum`, or the budget
    /// that lacks headroom. Pure: [`Self::headroom`] and [`Self::charge`] share it, so the check
    /// before the ECDH and the charge after it apply one rule.
    fn charges(
        &self,
        quantum: u128,
        from: &Did,
        units: OnionAdmissionUnits,
    ) -> Result<(QuantumLedger, QuantumLedger), OnionAdmissionRejection> {
        let sender = self
            .senders
            .get(from)
            .ok_or(OnionAdmissionRejection::UnlinkedSender)?
            .ledger
            .charged(quantum, units.get(), ONION_ADMISSION_SENDER_UNITS)
            .ok_or(OnionAdmissionRejection::SenderBudget)?;
        let global = self
            .global
            .charged(quantum, units.get(), ONION_ADMISSION_GLOBAL_UNITS)
            .ok_or(OnionAdmissionRejection::GlobalBudget)?;
        Ok((sender, global))
    }
}
