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
//!   + LinkOpened(Link) + LinkClosed(Link) + Reconcile(𝒫 Link)
//! ρ : S × Epoch × Key × 𝒫 Link → S × (Refused + NotFresh)      (the reset; it takes no time)
//! Link = Did × Generation
//! ```
//!
//! [`OnionAdmissionState::charge`], [`OnionAdmissionState::admit`],
//! [`OnionAdmissionState::link_opened`], [`OnionAdmissionState::link_closed`], and
//! [`OnionAdmissionState::reconcile`] realise `δ` on the five summands in place, and
//! [`OnionAdmissionState::renew`] realises the reset `ρ`, which restarts the clock.
//! [`OnionAdmissionState::is_rolled_back_at`] is the pure query that tells the shell when to renew.
//! Time is an argument, and the only randomness is the probe key given at construction, so a trace
//! of inputs determines the trace of verdicts. The order is the paper's hop algorithm, with the
//! charge taken on receipt (#834 L9):
//!
//! ```text
//! charge(link, u) at arr ──Err──→ drop   (nothing charged, no ECDH)
//!   │ Ok(token = (arr, epoch_i))
//!   ▼
//! α valid, ECDH, peel, γ ──fails──→ drop the token  (charged once, no admission)
//!   │ holds
//!   ▼
//! admit(token, e, x, ν) = token.epoch = e = epoch_i ; token.arr < x ≤ token.arr + V ; clock < x ;
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
//!   the effectful shell (#834 2a-4), when [`OnionAdmissionState::is_rolled_back_at`] holds
//!   (`now < clock − X₀`), must move to a fresh process epoch with
//!   [`OnionAdmissionState::renew`]. That clears the ledgers, the clock and the replay store, and
//!   then reconciles the cleared state with core's snapshot, `ρ = reconcile ∘ clear`, so the live
//!   links are the snapshot up to refusals. It is safe by the epoch law, but it invalidates
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
//! * **Five quanta per filter.** `arr < x ≤ arr + V` gives `arr ∈ [x − V, x)`. With `x = kQ` and `V
//!   = 5Q`, the arrival quanta are exactly `{k − 5, …, k − 1}`, five consecutive aligned quanta.
//!   The window is judged at `token.arr`, the instant of the charge, so these are also the charging
//!   quanta, however late the admission completes. They all lie in the ledger window `(s − 5, s]`
//!   of the last charge admitted into `R_i[x]` (at quantum `s ≤ k − 1`), and that charge bounded
//!   the whole set by the global cap.
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
//!   * **Event-pairing obligation (2a-4).** Between resets the live set shrinks only through
//!     `link_closed` and `reconcile`. A lost `Retired` therefore pins a live link, and a ledger
//!     slot, until a reconciliation repairs it. Core itself splits an `Admitted`/`Retired` pair
//!     when the callback is replaced, and a bounded shell queue may drop events. The shell must
//!     call [`OnionAdmissionState::reconcile`] with core's registry snapshot of live links whenever
//!     the callback is replaced, and on a periodic tick no longer than `V`. `reconcile` closes
//!     every live link absent from the snapshot and opens every snapshot link that is not live,
//!     so afterwards the live set is `L \ refused` for the snapshot `L`. The leak lasts at most one
//!     tick.
//!   * **Linearisation obligation (2a-4).** `reconcile` and `renew` both treat their snapshot as
//!     authoritative, so each snapshot must be linearised with the event stream it repairs. It must
//!     be delivered through the same ordered channel as `Admitted`/`Retired`, or read and applied
//!     atomically at the shell's queue-drain point. Under this obligation, after `reconcile` or
//!     `renew` the live set is exactly core's registry minus the refused links. Without it, a
//!     snapshot read before an `Admitted(g)` that is processed first would close the live `g` (its
//!     cells would be `LinkNotLive` for up to one tick), and the converse would reopen a retired
//!     `g`.
//!   * [`OnionAdmissionState::link_opened`] makes a link live, creating its DID's ledger, and is
//!     idempotent on a live link. [`OnionAdmissionState::link_closed`] makes it not live, and is
//!     idempotent too: closing an unknown, refused or closed link changes nothing. A close
//!     therefore affects only its own generation, under any interleaving (`open(g₁) open(g₂)
//!     close(g₁)` leaves `g₂` live, and a late close of a refused `g₁` is a no-op).
//!   * A cell is charged only against a live link, so a cell that arrives before its link is
//!     opened, after it is closed, or on a refused link is never charged and never decrypted.
//!   * A ledger is *releasable* when its DID has no live link and zero window load. Invariant:
//!     after every step no ledger is releasable, because `link_closed` releases a drained ledger
//!     at once and every step that enters a new quantum sweeps the ledgers that have drained.
//!     A load can reach zero only when the quantum advances, since charges only add to it.
//!   * The table holds at most `2·R` ledgers, where `R` is the transport connection-registry
//!     capacity (#723). There is no recycling. Within one epoch, a ledger is never reset while it
//!     carries load, so no DID regains budget by closing and reopening links: a reconnecting DID
//!     finds its old ledger. `(iii)` of the paper's replay bounds therefore holds per DID within an
//!     epoch, however many other DIDs churn. The live-link set is also capped at `2·R`. This is
//!     only a memory bound: under core's laws, which allow at most one live generation per DID and
//!     at most `R` DIDs, the cap is unreachable, and it only matters if the event obligation above
//!     is broken.
//!   * At most `R` links are live, and a closed ledger drains within one window. So the table
//!     fills only under connection churn beyond `R` within `V`. `link_opened` then refuses the
//!     *link*: the shell must close it (fail closed). A refused peer may redial later as a new
//!     generation. There is no fairness here: a churner that opens cheap DIDs at rate `R / V` can
//!     win every freed slot. The event path serves links first come, first served. `reconcile`
//!     opens a snapshot's links in DID order, so when the table is full, small DIDs win, and DID
//!     prefixes can be ground cheaply. Eventual admission of an honest redial is a liveness
//!     *assumption* on churn, as #834 states, not a guarantee.
//!   * [`OnionAdmissionState::renew`] (the epoch reset) requires a *fresh* epoch. Renewing into the
//!     current epoch would clear the replay store while keeping `epoch_i`, so a replay of an
//!     admitted `(x, ν)` could be admitted again. The state rejects only `e′ = epoch_i`. The shell
//!     owes the rest: it draws `e′` independently and uniformly from `2¹²⁸`, and redraws on that
//!     rejection. Over `k` resets, the chance of reusing one *fixed* earlier epoch is `≤ k·2⁻¹²⁸`.
//!     The chance that some reset reuses *any* earlier epoch, the `A → B → A` sequence that would
//!     revive `A`'s layers, is at most the birthday bound `k²·2⁻¹²⁹`. The reset rebuilds the live
//!     set from core's snapshot, and every DID with a live link starts with a zero-load ledger, so
//!     every live link still has a ledger. The per-DID bound `B` therefore holds within one epoch.
//!     A token charged before the reset is never admitted after it, because its epoch differs.
//!   * `G` is independent of the table size and bounds what all DIDs admit together, and hence the
//!     replay store.

mod bloom;
mod ledger;
#[cfg(all(test, rings_native))]
mod tests;

use std::collections::btree_map::Entry;
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
use super::ONION_FORWARD_PAYLOAD_TTL_MS;
use crate::onion::OnionExitEpoch;

/// Admission window `V = 150 s`: a layer is admissible at `arr` iff `arr < x ≤ arr + V`.
const ONION_ADMISSION_WINDOW_MS: u128 = ONION_FORWARD_MAX_VALIDITY_MS;

/// Arrival quanta per window, `N = V / Q = 5`, as the ledger's array length.
const ADMISSION_WINDOW_QUANTA: usize = 5;

/// Build-to-expiry offset `X₀ = V − Q`, the rollback threshold of the monotone clock.
const ONION_EXPIRY_OFFSET_MS: u128 = ONION_FORWARD_PAYLOAD_TTL_MS;

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
/// particular cell or to its units (the shell's obligation, see the Charging law). It is affine: it
/// is neither `Clone` nor `Copy`, only [`OnionAdmissionState::charge`] can build it, and
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
pub(super) enum OnionChargeRejection {
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

/// The links a table refused, which the shell must close. Returned by
/// [`OnionAdmissionState::reconcile`] and [`OnionAdmissionState::renew`].
#[must_use = "refused links must be closed by the shell"]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct OnionRefusedLinks(Vec<OnionAdmissionLink>);

impl OnionRefusedLinks {
    /// The refused links, in DID order.
    pub(super) fn links(&self) -> &[OnionAdmissionLink] {
        self.0.as_slice()
    }
}

/// A renewal refused because the requested epoch is the current one. Renewing into the current
/// epoch would clear the replay store while keeping `epoch_i`, and so re-admit replays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OnionEpochNotFresh;

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
            senders: BTreeMap::new(),
        }
    }

    /// Whether the wall clock has rolled back far enough behind the monotone clock that honest
    /// layers fail the window: `now < clock − X₀`. The shell then renews into a fresh epoch.
    pub(super) const fn is_rolled_back_at(&self, now_ms: u128) -> bool {
        now_ms.saturating_add(ONION_EXPIRY_OFFSET_MS) < self.clock_ms
    }

    /// The reset `ρ(epoch, key, live)`: the epoch reset. It requires a fresh `epoch` and otherwise
    /// changes nothing. On success, `ρ = reconcile ∘ clear`: the state is the initial state of
    /// `epoch` with probe key `filter_key`, reconciled with core's snapshot `live`, each DID with a
    /// zero-load ledger. It returns the snapshot links the table refuses, which the shell must
    /// close. The cleared table is empty and a snapshot has no more DIDs than links, so nothing is
    /// refused when `|live| ≤ 2·R`.
    pub(super) fn renew(
        &mut self,
        epoch: OnionExitEpoch,
        filter_key: OnionReplayFilterKey,
        live: impl IntoIterator<Item = OnionAdmissionLink>,
    ) -> Result<OnionRefusedLinks, OnionEpochNotFresh> {
        if epoch == self.epoch {
            return Err(OnionEpochNotFresh);
        }
        *self = Self {
            epoch,
            clock_ms: 0,
            swept_quantum: 0,
            replay: ReplayStore::new(filter_key),
            global: QuantumLedger::default(),
            capacity: self.capacity,
            senders: BTreeMap::new(),
        };
        Ok(self.reconcile(0, live))
    }

    /// The step `δ` on `Reconcile(live)`: make the live-link set equal core's registry snapshot
    /// `live`, up to refusals. Every live link absent from the snapshot is closed first, which
    /// frees its slot. Then every snapshot link that is not live is opened in DID order, with the
    /// table's usual refusal. Afterwards the live set is `live \ refused`, no link that was already
    /// live and is in the snapshot is refused, and a second reconciliation with the same snapshot,
    /// in any order, changes nothing. This repairs lost `Retired` and `Admitted` events. The
    /// snapshot must be linearised with the event stream (see the module laws).
    pub(super) fn reconcile(
        &mut self,
        now_ms: u128,
        live: impl IntoIterator<Item = OnionAdmissionLink>,
    ) -> OnionRefusedLinks {
        self.advance(now_ms);
        let mut snapshot = BTreeMap::<Did, BTreeSet<u64>>::new();
        for link in live {
            snapshot
                .entry(link.did)
                .or_default()
                .insert(link.generation);
        }
        let absent = self
            .senders
            .iter()
            .flat_map(|(&did, sender)| {
                let kept = snapshot.get(&did);
                sender
                    .live
                    .iter()
                    .copied()
                    .filter(move |generation| {
                        !kept.is_some_and(|generations| generations.contains(generation))
                    })
                    .map(move |generation| OnionAdmissionLink { did, generation })
            })
            .collect::<Vec<_>>();
        for link in absent {
            self.link_closed(now_ms, link);
        }
        let mut refused = Vec::new();
        for (did, generations) in snapshot {
            for generation in generations {
                let link = OnionAdmissionLink { did, generation };
                if self.link_opened(now_ms, link).is_err() {
                    refused.push(link);
                }
            }
        }
        OnionRefusedLinks(refused)
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
        let live_links = self.live_link_count();
        let ledgers = self.senders.len();
        let capacity = self.capacity;
        let sender = match self.senders.entry(link.did) {
            Entry::Occupied(occupied) if occupied.get().live.contains(&link.generation) => {
                return Ok(());
            }
            Entry::Occupied(occupied) if live_links < capacity => occupied.into_mut(),
            Entry::Vacant(vacant) if live_links < capacity && ledgers < capacity => {
                vacant.insert(SenderLedger {
                    ledger: QuantumLedger::default(),
                    live: BTreeSet::new(),
                })
            }
            Entry::Occupied(_) | Entry::Vacant(_) => return Err(OnionLinkTableFull),
        };
        sender.live.insert(link.generation);
        Ok(())
    }

    /// The step `δ` on `LinkClosed(link)`: make `link` no longer live, and release its DID's ledger
    /// at once if that leaves it releasable. It is idempotent: closing a link that is not live
    /// (unknown, refused, or already closed) changes nothing, so a close can never take another
    /// generation's liveness away.
    pub(super) fn link_closed(&mut self, now_ms: u128, link: OnionAdmissionLink) {
        let quantum = self.advance(now_ms);
        if let Entry::Occupied(mut occupied) = self.senders.entry(link.did) {
            occupied.get_mut().live.remove(&link.generation);
            if occupied.get().is_releasable_at(quantum) {
                occupied.remove();
            }
        }
    }

    /// The step `δ` on `Charge(link, u)`: charge a cell received on `link` to its DID's ledger and
    /// to the global ledger, both or neither, before its key is computed. Only a live link is
    /// charged.
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
        link: OnionAdmissionLink,
        units: OnionAdmissionUnits,
    ) -> Result<OnionAdmissionCharge, OnionChargeRejection> {
        let quantum = self.advance(now_ms);
        let sender = self
            .senders
            .get_mut(&link.did)
            .filter(|sender| sender.live.contains(&link.generation))
            .ok_or(OnionChargeRejection::LinkNotLive)?;
        let charged_sender = sender
            .ledger
            .charged(quantum, units.get(), ONION_ADMISSION_SENDER_UNITS)
            .ok_or(OnionChargeRejection::SenderBudget)?;
        let charged_global = self
            .global
            .charged(quantum, units.get(), ONION_ADMISSION_GLOBAL_UNITS)
            .ok_or(OnionChargeRejection::GlobalBudget)?;
        sender.ledger = charged_sender;
        self.global = charged_global;
        Ok(OnionAdmissionCharge {
            arrival_ms: self.clock_ms,
            epoch: self.epoch,
        })
    }

    /// The number of live links, `Σ_d |live(d)|`, derived rather than stored.
    fn live_link_count(&self) -> usize {
        self.senders.values().map(|sender| sender.live.len()).sum()
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
