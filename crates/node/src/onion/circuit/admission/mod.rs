//! Unified onion-layer admission: process epoch, validity window, replay store and unit budgets
//! (#834 L9, D2).
//!
//! # Step
//!
//! One hop's admission is a deterministic transition with no effect:
//!
//! ```text
//! δ : S × Time × I → S × (Out + Rejection)
//! I = Charge(Link × Units) + Admit(Charge × Epoch × Expiry × Tag)
//!   + LinkOpened(Link) + LinkClosed(Link)          Link = Did × Generation
//! ```
//!
//! [`OnionAdmissionState::charge`], [`OnionAdmissionState::admit`],
//! [`OnionAdmissionState::link_opened`] and [`OnionAdmissionState::link_closed`] realise `δ` on the
//! four summands in place. Time is an argument, and the only randomness is the probe key given at
//! construction, so a trace of inputs determines the trace of verdicts. The order is the paper's
//! hop algorithm, with the charge taken on receipt (#834 L9):
//!
//! ```text
//! charge(link, u) at arr ──Err──→ drop   (nothing charged, no ECDH)
//!   │ Ok(token = (arr, epoch_i))
//!   ▼
//! α valid, ECDH, peel, γ ──fails──→ drop the token  (charged once, no admission)
//!   │ holds
//!   ▼
//! admit(token, e, x, ν) = e = epoch_i ; token.arr < x ≤ token.arr + V ; clock < x ;
//!                         ν ∉ R_i[x] ; R_i[x] ← R_i[x] ∪ {ν}
//! ```
//!
//! # Laws
//!
//! * **Charging.** A cell is charged `u(b) = b / 16 KiB` by [`OnionAdmissionState::charge`] on
//!   receipt, against the live link it arrived on, before its group element `α` is checked or its
//!   key computed. The charge returns an [`OnionAdmissionCharge`]: an *affine* token (neither
//!   `Clone` nor `Copy`, built only by `charge`) that records the arrival `arr` and the process
//!   epoch. [`OnionAdmissionState::admit`] consumes the token. Dropping it (for an invalid `α` or
//!   a failed `γ`) is a valid settlement: those cells are already paid for. Replayed, expired and
//!   stale-epoch cells pay too. A charge on a link that is not live, or without headroom, is
//!   refused and charges nothing.
//!
//!   What the types guarantee: a charge requires a live link, admission requires a paid token, and
//!   one token admits at most once. What they do not guarantee: the token is not bound to a cell or
//!   to its units, and peeling does not require one. The 2a-4 shell owes these: charge every
//!   received cell once, on the link it arrived on and with its class's units, and peel only a
//!   cell whose charge succeeded.
//! * **Clock: safety.** The state's clock is the join of every `now` it has seen,
//!   `now := max(now, clock)`. A wall-clock rollback therefore cannot refund budget or revive a
//!   dropped filter. The window is judged at the cell's arrival `token.arr`, which is the clock at
//!   its charge, not at its admission. Admission also requires `clock < x`, so a filter the clock
//!   has dropped is never written again, and at-most-once holds when an admission completes late.
//! * **Clock: liveness cost.** An honest layer built at `t` has `x ∈ [t + X₀, t + V)` with
//!   `X₀ = V − Q`. If the wall clock jumps forward by `Δ` and back, `clock` stays pinned at the
//!   future instant. Honest layers start failing the window once `Δ ≥ X₀ = 4Q`, and all of them
//!   fail once `Δ ≥ V`, until the wall clock catches up. Safety and liveness cannot both be kept
//!   inside one epoch: recovering early would mean forgetting filters that may still be live. So
//!   the effectful shell (#834 2a-4), when it sees `now < clock − X₀`, must move to a fresh process
//!   epoch with [`OnionAdmissionState::renewed`]. That keeps the live links and clears the
//!   ledgers, the clock and the replay store. It is safe by the epoch law, but it invalidates
//!   every loop in flight through this hop.
//! * **Skew (for 2a-4).** The window has no skew tolerance. A hop whose clock runs `δ < Q` behind
//!   or ahead of the builder's rejects about `δ / Q` of loops at the window's edges.
//! * **Epoch (D2).** Only layers sealed for the current process epoch are admitted. A restarted
//!   process draws a fresh epoch, so every layer of the previous process is rejected, although the
//!   replay store was lost with that process.
//! * **At most once (L9).** An [`OnionExpiry`] is a quantum index, so it lies on the grid
//!   `x ∈ Q·ℕ` by construction. `R_i[x]` is consulted only while `now < x`. It is dropped as a
//!   whole by the first step at or after `x`, because steps are the only way time enters the state.
//!   A live filter has no false negatives, and once `x` has passed the window rejects `x`. Hence a
//!   pair `(x, ν)` is admitted at most once. At most `V / Q = 5` filters are live, namely the grid
//!   points of `(clock, clock + V]`. An idle hop keeps its filters until its next step. They stay
//!   within the memory bound below, and the 2a-4 shell decides whether to drive a step on a timer.
//! * **Five quanta per filter.** `arr < x ≤ arr + V` gives `arr ∈ [x − V, x)`. With `x = kQ` and
//!   `V = 5Q`, the arrival quanta are exactly `{k − 5, …, k − 1}`, five consecutive aligned quanta.
//!   The window is judged at `token.arr`, the instant of the charge, so these are also the charging
//!   quanta, however late the admission completes. They all lie in the ledger window `(s − 5, s]`
//!   of the last charge admitted into `R_i[x]` (at quantum `s ≤ k − 1`), and that charge bounded the
//!   whole set by the global cap.
//! * **Tags ≤ units.** Tags are counted per cell and budgets per unit, and every admitted cell was
//!   charged at least one unit. So `|R_i[x]| ≤ G = 64·B`, `R_i[x]` has at most `64` blocks, and
//!   its false-positive rate is `≤ 64·2⁻²⁶ = 2⁻²⁰`. A tag is live only if it was admitted in the
//!   current ledger window, so all live filters together hold `≤ G` tags in `≤ 64 + 5` blocks
//!   (`≈ 5.3 MB`).
//! * **Budgets.** Every aligned window of five arrival quanta charges one sender DID at most `B`
//!   units and the hop at most `G = 64·B`. An aligned window covers between `4Q` and `5Q` of wall
//!   time. So every interval of length `≤ 4Q` carries at most `B` units per sender. An interval
//!   just over `4Q` can straddle two windows and carry up to `2·B` (a burst at the end of quantum
//!   `s` and another at the start of `s + 5`). The long-run rate is `B / V ≈ 109` units/s per
//!   sender and `≈ 6990` units/s in total. `G` is one pool shared by all links, which `γ`-failing
//!   cover cells also consume (for 2a-4).
//! * **Links and ledgers.** A link is one generation of an authenticated link to a DID,
//!   [`OnionAdmissionLink`] `= (did, generation)`: plain data, fed by the 2a-4 shell from core's
//!   `Admitted`/`Retired` events. The state keeps the set of live links, and one budget ledger per
//!   DID with at least one live link or some load.
//!   * [`OnionAdmissionState::link_opened`] makes a link live, creating its DID's ledger, and is
//!     idempotent on a live link. [`OnionAdmissionState::link_closed`] makes it not live, and is
//!     idempotent too: closing an unknown, refused or closed link changes nothing. A close
//!     therefore affects only its own generation, under any interleaving (`open(g₁) open(g₂)
//!     close(g₁)` leaves `g₂` live, and a late close of a refused `g₁` is a no-op). Nothing is held
//!     by the shell that could leak when dropped.
//!   * A cell is charged only against a live link, so a cell that arrives before its link is
//!     opened, after it is closed, or on a refused link is never charged and never decrypted.
//!   * A ledger is *releasable* when its DID has no live link and zero window load. Invariant:
//!     after every step no ledger is releasable, because `link_closed` releases a drained ledger
//!     at once and every step that enters a new quantum sweeps the ledgers that have drained.
//!     A load can reach zero only when the quantum advances, since charges only add to it.
//!   * The table holds at most `2·R` ledgers and `2·R` live links, where `R` is the transport
//!     connection-registry capacity (#723). There is no recycling. A ledger is never reset while it
//!     carries load, so no DID regains budget by closing and reopening links: a reconnecting DID
//!     finds its old ledger. `(iii)` of the paper's replay bounds therefore holds per DID, however
//!     many other DIDs churn.
//!   * At most `R` links are live, and a closed ledger drains within one window. So the table
//!     fills only under connection churn beyond `R` within `V`. `link_opened` then refuses the
//!     *link*: the shell must close it (fail closed). A refused peer may redial later as a new
//!     generation. There is no fairness here: a churner that opens cheap DIDs at rate `R / V` can
//!     win every freed slot. Eventual admission of an honest redial is a liveness *assumption* on
//!     churn, as #834 states, not a guarantee.
//!   * [`OnionAdmissionState::renewed`] (the epoch reset) keeps the live links, and every DID with
//!     a live link restarts with a zero-load ledger, so every live link still has a ledger. The
//!     per-DID bound `B` therefore holds within one epoch. A token charged before the reset is never
//!     admitted after it, because its epoch differs.
//!   * `G` is independent of the table size and bounds what all DIDs admit together, and hence the
//!     replay store.

mod bloom;
mod ledger;
#[cfg(all(test, rings_native))]
mod tests;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;

use rings_core::dht::Did;

pub(super) use self::bloom::OnionReplayFilterKey;
use self::bloom::ReplayStore;
use self::ledger::QuantumLedger;
use super::OnionExpiry;
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

/// Per-sender budget `B`: units of 16 KiB per DID per aligned window of `N` quanta.
const ONION_ADMISSION_SENDER_UNITS: u32 = 16_384;

/// Global budget `G = 64·B` units per aligned window of `N` quanta. It is independent of the number
/// of links and bounds the replay store at 64 blocks per filter.
const ONION_ADMISSION_GLOBAL_UNITS: u32 = 64 * ONION_ADMISSION_SENDER_UNITS;

/// Compile-time law: the ledger's array length is `N = V / Q`.
const _: () = assert!(ADMISSION_WINDOW_QUANTA_WIDE == ADMISSION_WINDOW_QUANTA as u128);

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

/// Evidence of one charge, taken at `arrival_ms` by a state of this epoch; it is not bound to a
/// particular cell or to its units (the shell's obligation, see the Charging law). It is affine: it is
/// neither `Clone` nor `Copy`, only [`OnionAdmissionState::charge`] can build it, and
/// [`OnionAdmissionState::admit`] consumes it. Dropping it settles an invalid `α` or a failed `γ`,
/// which are already paid for.
#[must_use = "a charged cell is admitted with its token, or dropped after an invalid α or γ"]
#[derive(Debug)]
pub(super) struct OnionAdmissionCharge {
    /// The cell's arrival: the monotone clock at its charge. The window is judged here.
    arrival_ms: u128,
    /// The epoch of the state that charged the cell.
    epoch: OnionExitEpoch,
}

/// One generation of an authenticated link from a sender DID, as core admits and retires it. It
/// is plain data, not a capability: the state keeps the set of live links, and the 2a-4 shell
/// feeds it from core's `Admitted`/`Retired` events.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct OnionAdmissionLink {
    /// The link's sender DID, which owns the budget ledger.
    pub(super) did: Did,
    /// Core's generation of the link to `did`.
    pub(super) generation: u64,
}

/// The authenticated fields of a peeled layer that admission decides on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OnionAdmissionLayer {
    /// Process epoch the layer was sealed for.
    pub(super) epoch: OnionExitEpoch,
    /// Quantised expiry `x` of the layer's loop.
    pub(super) expiry: OnionExpiry,
    /// Replay tag `ν` of the layer.
    pub(super) tag: OnionForwardNonce,
}

/// Why a cell could not be charged. Nothing was charged, and the cell must not be decrypted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OnionBudgetRejection {
    /// The cell's link is not live: not yet opened, closed, or refused.
    LinkNotLive,
    /// The sender's ledger lacks headroom for the cell's units.
    SenderBudget,
    /// The hop lacks headroom under `G = 64·B`.
    GlobalBudget,
}

/// Why a charged layer was not admitted. The cell was charged and is dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OnionAdmissionRejection {
    /// The layer, or its charge, belongs to another process epoch (D2).
    StaleEpoch,
    /// `x ∉ (token.arr, token.arr + V]`, or `x` has passed on the monotone clock.
    OutsideWindow,
    /// `ν ∈ R_i[x]`: a replay, or a false positive at rate `≤ 2⁻²⁰`.
    Replayed,
}

/// A link refused because the table is full (connection churn beyond `R` within `V`). The shell
/// must close the link, and the peer may redial later.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OnionLinkTableFull;

/// One sender DID's unit ledger and its live link generations.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SenderLedger {
    /// Units charged to this DID in the window.
    ledger: QuantumLedger,
    /// The generations of this DID's live links.
    live: BTreeSet<u64>,
}

impl SenderLedger {
    /// Whether the ledger may be released at `quantum`: it has no live link and no load.
    fn is_releasable_at(&self, quantum: u128) -> bool {
        self.live.is_empty() && self.ledger.load(quantum) == 0
    }
}

/// The admission state `S` of one hop for one process lifetime.
pub(super) struct OnionAdmissionState {
    /// This process's epoch `epoch_i`.
    epoch: OnionExitEpoch,
    /// Monotone clock: the greatest `now` seen so far.
    clock_ms: u128,
    /// The quantum at which drained ledgers were last swept.
    swept_quantum: u128,
    /// Replay store `R_i`.
    replay: ReplayStore,
    /// Units charged to the whole hop.
    global: QuantumLedger,
    /// Capacity `2·R`, both of the ledger table and of the live-link set.
    capacity: usize,
    /// Number of live links, the sum of every ledger's `live` set, at most `capacity`.
    live_links: usize,
    /// At most `capacity` sender ledgers, none of them releasable between steps.
    senders: BTreeMap<Did, SenderLedger>,
}

impl OnionAdmissionState {
    /// The initial state of a process with epoch `epoch` and probe key `filter_key`, both drawn
    /// once at process start by the caller. The table holds `2·R` ledgers and live links, where `R`
    /// is the transport connection-registry capacity.
    pub(super) fn new(
        epoch: OnionExitEpoch,
        filter_key: OnionReplayFilterKey,
        link_registry_capacity: NonZeroUsize,
    ) -> Self {
        Self {
            epoch,
            clock_ms: 0,
            swept_quantum: 0,
            replay: ReplayStore::new(filter_key),
            global: QuantumLedger::default(),
            capacity: link_registry_capacity.get().saturating_mul(2),
            live_links: 0,
            senders: BTreeMap::new(),
        }
    }

    /// The epoch reset: the state of a fresh epoch `epoch` with probe key `filter_key` over the same
    /// live links. Links belong to the transport, not to the epoch, so every live link stays live
    /// with a zero ledger. The clock, the loads, the replay store and the ledgers of closed links
    /// are cleared. It is safe by the epoch law, since every layer of the old epoch is rejected.
    pub(super) fn renewed(self, epoch: OnionExitEpoch, filter_key: OnionReplayFilterKey) -> Self {
        let senders = self
            .senders
            .into_iter()
            .filter(|(_, sender)| !sender.live.is_empty())
            .map(|(did, sender)| {
                (did, SenderLedger {
                    ledger: QuantumLedger::default(),
                    live: sender.live,
                })
            })
            .collect();
        Self {
            epoch,
            clock_ms: 0,
            swept_quantum: 0,
            replay: ReplayStore::new(filter_key),
            global: QuantumLedger::default(),
            capacity: self.capacity,
            live_links: self.live_links,
            senders,
        }
    }

    /// The step `δ` on `LinkOpened(link)`: make `link` live, creating its DID's ledger if it has
    /// none. Opening a live link again changes nothing. A full table refuses the link; the shell
    /// must then close it.
    pub(super) fn link_opened(
        &mut self,
        now_ms: u128,
        link: OnionAdmissionLink,
    ) -> Result<(), OnionLinkTableFull> {
        self.advance(now_ms);
        let has_ledger = self.senders.contains_key(&link.did);
        if self
            .senders
            .get(&link.did)
            .is_some_and(|sender| sender.live.contains(&link.generation))
        {
            return Ok(());
        }
        if self.live_links >= self.capacity || (!has_ledger && self.senders.len() >= self.capacity)
        {
            return Err(OnionLinkTableFull);
        }
        self.senders
            .entry(link.did)
            .or_insert_with(|| SenderLedger {
                ledger: QuantumLedger::default(),
                live: BTreeSet::new(),
            })
            .live
            .insert(link.generation);
        self.live_links = self.live_links.saturating_add(1);
        Ok(())
    }

    /// The step `δ` on `LinkClosed(link)`: make `link` no longer live, and release its DID's ledger
    /// at once if that leaves it releasable. It is idempotent: closing a link that is not live
    /// (unknown, refused, or already closed) changes nothing, so a close can never take another
    /// generation's liveness away.
    pub(super) fn link_closed(&mut self, now_ms: u128, link: OnionAdmissionLink) {
        let quantum = self.advance(now_ms);
        if let Some(sender) = self.senders.get_mut(&link.did) {
            if sender.live.remove(&link.generation) {
                self.live_links = self.live_links.saturating_sub(1);
            }
            if sender.is_releasable_at(quantum) {
                self.senders.remove(&link.did);
            }
        }
    }

    /// The step `δ` on `Charge(link, u)`: charge a cell received on `link` to its DID's ledger and to
    /// the global ledger, both or neither, before its key is computed. Only a live link is charged.
    ///
    /// ```text
    ///  now := max(now, clock);  drop R[x] for x ≤ now;  s = ⌊now / Q⌋;  sweep if s is new
    ///   ├─ link not live ─────────────────→ LinkNotLive
    ///   ├─ load_did(s) + u > B ───────────→ SenderBudget
    ///   ├─ load_global(s) + u > G ────────→ GlobalBudget
    ///   └─ charge both ledgers ───────────→ Ok(token = (clock, epoch_i))
    /// ```
    pub(super) fn charge(
        &mut self,
        now_ms: u128,
        link: &OnionAdmissionLink,
        units: OnionAdmissionUnits,
    ) -> Result<OnionAdmissionCharge, OnionBudgetRejection> {
        let quantum = self.advance(now_ms);
        let sender = self
            .senders
            .get_mut(&link.did)
            .filter(|sender| sender.live.contains(&link.generation))
            .ok_or(OnionBudgetRejection::LinkNotLive)?;
        let charged_sender = sender
            .ledger
            .charged(quantum, units.get(), ONION_ADMISSION_SENDER_UNITS)
            .ok_or(OnionBudgetRejection::SenderBudget)?;
        let charged_global = self
            .global
            .charged(quantum, units.get(), ONION_ADMISSION_GLOBAL_UNITS)
            .ok_or(OnionBudgetRejection::GlobalBudget)?;
        sender.ledger = charged_sender;
        self.global = charged_global;
        Ok(OnionAdmissionCharge {
            arrival_ms: self.clock_ms,
            epoch: self.epoch,
        })
    }

    /// The step `δ` on `Admit(token, e, x, ν)`: admit a charged, `γ`-verified layer, or reject it.
    /// The charge stands either way. The window is judged at the token's arrival, not at `now`.
    ///
    /// ```text
    ///  now := max(now, clock);  drop R[x] for x ≤ now
    ///   ├─ e ≠ epoch_i ∨ token.epoch ≠ epoch_i ──────→ StaleEpoch
    ///   ├─ ¬ x.admissible_at(token.arr) ∨ x ≤ clock ─→ OutsideWindow
    ///   ├─ ν ∈ R[x] ─────────────────────────────────→ Replayed
    ///   └─ R[x] ← R[x] ∪ {ν} ────────────────────────→ Ok
    /// ```
    ///
    /// A wire expiry off the grid never reaches this step: the shell parses it with
    /// `OnionExpiry::from_ms`, and `None` drops the cell, which is already charged.
    pub(super) fn admit(
        &mut self,
        now_ms: u128,
        charge: OnionAdmissionCharge,
        layer: OnionAdmissionLayer,
    ) -> Result<(), OnionAdmissionRejection> {
        let OnionAdmissionCharge { arrival_ms, epoch } = charge;
        self.advance(now_ms);
        if layer.epoch != self.epoch || epoch != self.epoch {
            return Err(OnionAdmissionRejection::StaleEpoch);
        }
        if !layer.expiry.admissible_at(arrival_ms) || layer.expiry.has_passed_at(self.clock_ms) {
            return Err(OnionAdmissionRejection::OutsideWindow);
        }
        let probe = self.replay.probe(layer.tag);
        if self.replay.contains(layer.expiry, &probe) {
            return Err(OnionAdmissionRejection::Replayed);
        }
        self.replay.insert(layer.expiry, &probe);
        Ok(())
    }

    /// Advance the monotone clock to `max(now, clock)`, drop every filter whose `x` has passed,
    /// and, on entering a new quantum, release every ledger that has become releasable. Return the
    /// current arrival quantum `⌊clock / Q⌋`.
    fn advance(&mut self, now_ms: u128) -> u128 {
        self.clock_ms = self.clock_ms.max(now_ms);
        self.replay.forget_through(self.clock_ms);
        let quantum = self.clock_ms / ONION_FORWARD_EXPIRY_QUANTUM_MS;
        if quantum > self.swept_quantum {
            self.senders
                .retain(|_, sender| !sender.is_releasable_at(quantum));
            self.swept_quantum = quantum;
        }
        quantum
    }
}
