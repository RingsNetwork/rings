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
//! ρ : S × Epoch × Key → S + NotFresh                            (the reset; it takes no time)
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
//!   What the types guarantee: a charge requires a live link, [`OnionAdmissionState::charge`]
//!   takes the received [`OnionCell`](crate::onion::sphinx::cell::OnionCell) and charges its own
//!   class's units, and returns it paired with its token as a `Charged` cell, the only value
//!   that can be peeled; admission requires that token, and one token admits at most once. So a
//!   cell is charged once, with its class's units, before its header is read, by construction
//!   (the hop step, `circuit::hop`, is the one caller).
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
//!   the shell (`circuit::shell`), when [`OnionAdmissionState::is_rolled_back_at`] holds
//!   (`now < clock − X₀`), must move to a fresh process epoch with
//!   [`OnionAdmissionState::renew`]. That clears the ledgers, the clock and the replay store, and
//!   then keeps the table's own live links with fresh ledgers, refusing nothing. It is safe by
//!   the epoch law, but it invalidates every loop in flight through this hop.
//! * **Skew.** The window has no skew tolerance. A hop whose clock runs `δ < Q` behind
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
//!   within the memory bound below.
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
//! * **Budgets.** Every aligned window of five arrival quanta charges one connection slot at most `B`
//!   units and the hop at most `G = 64·B`. An aligned window covers between `4Q` and `5Q` of wall
//!   time. So every interval of length `≤ 4Q` carries at most `B` units per slot. An interval
//!   just over `4Q` can straddle two windows and carry up to `2·B` (a burst at the end of quantum
//!   `s` and another at the start of `s + 5`). The long-run rate is `B / V ≈ 109` units/s per
//!   slot and `≈ 6990` units/s in total. `G` is split into a reserved per-slot floor `f > 0` across
//!   the `2·R` connection slots and a shared pool `P = G − 2·R·f` (#907, #1183): a slot's charges up
//!   to `f` draw only from its own floor and cannot be starved by noisy slots exhausting `P`, while
//!   over-floor charges (`> f` up to `B`) draw from `P`. `γ`-failing cover cells also consume
//!   these budgets.
//! * **Links, slots, and ledgers.** A link is one generation of an authenticated link to a DID,
//!   [`OnionAdmissionLink`] `= (did, generation)`: plain data. Core's link facts reach the
//!   data plane as `LinkFact::{Admitted, Retired}` in its namespace's queue, in core's order and
//!   never dropped (`extension::dispatch`), and the shell numbers each admission of a DID with a
//!   fresh generation. The state keeps the set of live links, and one budget ledger per connection
//!   slot ([`OnionSlotId`] via [`SlotRoster`]) with at least one live link or some window load.
//!   * **Event-pairing obligation.** Between resets the live set shrinks only through
//!     `link_closed` and `reconcile`, so a lost `Retired` would pin a live link and a slot.
//!     The dispatch queue loses no fact, so the obligation holds by construction once the
//!     data plane's namespace is registered; the links admitted before then are read once, by
//!     the [`OnionAdmissionState::reconcile`] the shell applies when it is installed.
//!     `reconcile` closes every live link absent from the snapshot and opens every snapshot link
//!     that is not live, so afterwards the live set is `L \ refused` for the snapshot `L`.
//!   * **Linearisation obligation.** `reconcile` treats its snapshot as authoritative, so the
//!     snapshot must be read and applied atomically at the shell's queue-drain point: the shell
//!     reads core's admitted peers inside the effect, under the protocol's transition gate.
//!     Every fact queued behind it then describes core's later changes, so after the queue
//!     drains the live set is exactly core's admitted links minus the refused ones. A fact
//!     queued before the read and applied after it may briefly close or reopen a link the
//!     snapshot already reflects; the facts after it restore core's state.
//!   * [`OnionAdmissionState::link_opened`] makes a link live, assigning its peer to a connection
//!     slot via [`SlotRoster`] (cleanest-first when connecting fresh, or resuming its retained slot
//!     when reconnecting within the window), and is idempotent on a live link.
//!     [`OnionAdmissionState::link_closed`] makes it not live, and is idempotent too: closing an
//!     unknown, refused or closed link changes nothing. A close therefore affects only its own
//!     generation, under any interleaving (`open(g₁) open(g₂) close(g₁)` leaves `g₂` live, and a
//!     late close of a refused `g₁` is a no-op).
//!   * A cell is charged only against a live link, so a cell that arrives before its link is
//!     opened, after it is closed, or on a refused link is never charged and never decrypted.
//!   * A slot ledger is *releasable* when its slot has no live link and zero window load. Invariant:
//!     after every step no ledger is releasable, because `link_closed` releases a drained slot
//!     ledger at once and every step that enters a new quantum sweeps the slot ledgers that have
//!     drained. A load can reach zero only when the quantum advances, since charges only add to it.
//!   * The table has `2·R` connection slots (`0..2·R`), where `R` is the transport
//!     connection-registry capacity (#723). When a link closes with non-zero window load, its slot
//!     is freed in the [`SlotRoster`] for cleanest-first reuse while the slot's ledger retains its
//!     residual load and remembers the peer until the window drains (#907, #1094, #1183). Thus a
//!     reconnecting DID resumes its old slot ledger without regaining budget, and rotating `> 2·R`
//!     Sybil connections within `V` reuses free slots cleanest-first instead of exhausting the
//!     ledger table with `OnionLinkTableFull`. The live-link set is capped at `2·R`. Under core's
//!     laws, which allow at most one live generation per DID and at most `R` DIDs, the cap is
//!     unreachable, and it only matters if the event obligation above is broken.
//!   * When `2·R` links are already live, `link_opened` refuses a new link with
//!     [`OnionLinkTableFull`], which stays not live (fail closed): it carries no onion cell, every
//!     cell from it is refused uncharged, and core's link itself is core's to keep (#935).
//!     `reconcile` orders snapshot links by liveness, assigned slot, and admission generation —
//!     never by `Did` — so grinding small `Did`s gives no advantage during reconciliation (#1183).
//!   * [`OnionAdmissionState::renew`] (the epoch reset) requires a *fresh* epoch. Renewing into the
//!     current epoch would clear the replay store while keeping `epoch_i`, so a replay of an
//!     admitted `(x, ν)` could be admitted again. The state rejects only `e′ = epoch_i`. The shell
//!     owes the rest: it draws `e′` independently and uniformly from `2¹²⁸`, and redraws on that
//!     rejection. Over `k` resets, the chance of reusing one *fixed* earlier epoch is `≤ k·2⁻¹²⁸`.
//!     The chance that some reset reuses *any* earlier epoch, the `A → B → A` sequence that would
//!     revive `A`'s layers, is at most the birthday bound `k²·2⁻¹²⁹`. The reset keeps the table's
//!     own live set, `ρ = reconcile(live_links(S)) ∘ clear`, which refuses nothing since it held
//!     that set before; every slot with a live link starts with a zero-load ledger, so every live
//!     link still has a ledger. The per-slot bound `B` therefore holds within one epoch.
//!     A token charged before the reset is never admitted after it, because its epoch differs.
//!   * `G` is independent of the table size and bounds what all slots admit together, and hence the
//!     replay store.

mod bloom;
mod ledger;
#[cfg(all(test, rings_native))]
mod tests;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::sync::Arc;

use rings_core::dht::Did;
use rings_core::message::FloorUse;
use rings_core::message::Principal;
use rings_core::message::SlotFloorUse;
use rings_core::message::SlotRoster;

pub(crate) use self::bloom::OnionReplayFilterKey;
use self::bloom::ReplayStore;
pub(crate) use self::ledger::OnionSlotId;
use self::ledger::QuantumLedger;
use self::ledger::SlotLedger;
use super::OnionExpiry;
use super::OnionReplayNonce;
use super::ONION_ADMISSION_WINDOW_MS;
use super::ONION_EXPIRY_OFFSET_MS;
use super::ONION_EXPIRY_QUANTUM_MS;
use crate::onion::sphinx::cell::Charged;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::OnionProcessEpoch;

/// Arrival quanta per window, `N = V / Q = 5`, as the ledger's array length.
const ADMISSION_WINDOW_QUANTA: usize = 5;

/// `N = V / Q` in the quantum domain.
const ADMISSION_WINDOW_QUANTA_WIDE: u128 = ONION_ADMISSION_WINDOW_MS / ONION_EXPIRY_QUANTUM_MS;

/// Per-sender (per-slot) budget `B`: units of 16 KiB per connection slot per aligned window of `N`
/// quanta.
pub(crate) const ONION_ADMISSION_SENDER_UNITS: u32 = 16_384;

/// Default per-slot floor `f`: units of 16 KiB reserved per connection slot in the global budget
/// when `2·R·f ≤ G / 2`. At `1_024` units (`16 MiB`), one slot's floor covers at least one cell of
/// the largest loop class (`MiB12`, `768` units) and divides `B` and `G` evenly.
pub(crate) const ONION_ADMISSION_SLOT_FLOOR_UNITS: u32 = 1_024;

/// Global budget `G = 64·B` units per aligned window of `N` quanta. It is independent of the number
/// of links and bounds the replay store at 64 blocks per filter.
const ONION_ADMISSION_GLOBAL_UNITS: u32 = 64 * ONION_ADMISSION_SENDER_UNITS;

/// Compile-time law: the ledger's array length is `N = V / Q`.
const _: () = assert!(ADMISSION_WINDOW_QUANTA_WIDE == ADMISSION_WINDOW_QUANTA as u128);

/// Compile-time law: a slot's floor fits within its per-slot budget `B`.
const _: () = assert!(
    ONION_ADMISSION_SLOT_FLOOR_UNITS > 0
        && ONION_ADMISSION_SLOT_FLOOR_UNITS <= ONION_ADMISSION_SENDER_UNITS
);

/// Units of 16 KiB charged for one cell. A class-`b` cell costs `b / 16 KiB ≥ 1` units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OnionAdmissionUnits(NonZeroU32);

impl OnionAdmissionUnits {
    /// Wrap a positive unit count.
    #[cfg(all(test, rings_native))]
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
pub(crate) struct OnionAdmissionCharge {
    /// The cell's arrival: the monotone clock at its charge. The window is judged here.
    arrival_ms: u128,
    /// The epoch of the state that charged the cell.
    epoch: OnionProcessEpoch,
}

/// One generation of an authenticated link from a sender DID, as the data plane numbers core's
/// admissions of it. It is plain data, not a capability: the state keeps the set of live links.
///
/// Law: the shell draws generations from one counter, so `(did, generation)` names one
/// admission of `did` for the life of the data plane.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OnionAdmissionLink {
    /// The link's sender DID.
    did: Did,
    /// The shell's generation of this admission of `did`.
    generation: u64,
}

impl OnionAdmissionLink {
    /// The link of `did` at `generation`.
    pub(crate) const fn new(did: Did, generation: u64) -> Self {
        Self { did, generation }
    }

    /// The peer at the other end.
    pub(crate) const fn peer(self) -> Did {
        self.did
    }

    /// The generation of this admission.
    pub(crate) const fn generation(self) -> u64 {
        self.generation
    }
}

/// The authenticated fields of a peeled layer that admission decides on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionAdmissionLayer {
    /// Process epoch the layer was sealed for.
    pub(crate) epoch: OnionProcessEpoch,
    /// Quantised expiry `x` of the layer's loop.
    pub(crate) expiry: OnionExpiry,
    /// Replay tag `ν` of the layer.
    pub(crate) tag: OnionReplayNonce,
}

/// Why a cell could not be charged. Nothing was charged, and the cell must not be decrypted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnionChargeRejection {
    /// The cell's link is not live: not yet opened, closed, or refused.
    LinkNotLive,
    /// The connection slot's ledger lacks headroom for the cell's units.
    SenderBudget,
    /// The hop lacks headroom in the shared pool `P` under `G = 64·B`.
    GlobalBudget,
}

/// Why a charged layer was not admitted. The cell was charged and is dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnionAdmissionRejection {
    /// The layer, or its charge, belongs to another process epoch (D2).
    StaleEpoch,
    /// `x ∉ (token.arr, token.arr + V]`, or `x` has passed on the monotone clock.
    OutsideWindow,
    /// `ν ∈ R_i[x]`: a replay, or a false positive at rate `≤ 2⁻²⁰`.
    Replayed,
}

/// A link refused because `2·R` live links already occupy the table. The link stays not live, and
/// the peer's next admission is a new generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionLinkTableFull;

/// The links a table refused, which stay not live. Returned by
/// [`OnionAdmissionState::reconcile`].
#[must_use = "refused links carry no onion cell, which the shell reports"]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct OnionRefusedLinks(Vec<OnionAdmissionLink>);

impl OnionRefusedLinks {
    /// The refused links, in the order `reconcile` attempted to open them.
    pub(crate) fn links(&self) -> &[OnionAdmissionLink] {
        self.0.as_slice()
    }
}

/// A renewal refused because the requested epoch is the current one. Renewing into the current
/// epoch would clear the replay store while keeping `epoch_i`, and so re-admit replays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionEpochNotFresh;

/// The admission state `S` of one hop for one process lifetime.
pub(crate) struct OnionAdmissionState {
    /// This process's epoch `epoch_i`.
    epoch: OnionProcessEpoch,
    /// Monotone clock: the greatest `now` seen so far.
    clock_ms: u128,
    /// The quantum at which drained ledgers were last swept.
    swept_quantum: u128,
    /// Replay store `R_i`.
    replay: ReplayStore,
    /// Units charged to the whole hop.
    global: QuantumLedger,
    /// Capacity `2·R`, both of the connection-slot table and of the live-link set.
    capacity: usize,
    /// Reserved per-slot floor `f` in units of 16 KiB.
    floor_units: u32,
    /// Shared pool `P = G − capacity · f` for over-floor charges.
    pool_units: u32,
    /// Aggregate over-floor load `Σ_κ max(0, load(κ) − f)` at the current quantum.
    pool_load: u32,
    /// Connection-slot roster assigning arriving peers cleanest-first.
    slot_roster: Arc<SlotRoster>,
    /// Whether `slot_roster` is owned locally (`true` in standalone states) or shared with
    /// `PeerRing::edges()` (`false` when attached via [`Self::with_slot_roster`], where the
    /// transport owns `arrive`/`depart`).
    owns_slot_roster: bool,
    /// Active or draining slot ledgers, keyed by connection slot `0..capacity`.
    slots: BTreeMap<OnionSlotId, SlotLedger>,
    /// The connection slot of every DID that has a live link or retained load in `slots`.
    peer_slots: BTreeMap<Did, OnionSlotId>,
}

impl OnionAdmissionState {
    /// The initial state of a process with epoch `epoch` and probe key `filter_key`, both drawn
    /// once at process start by the caller. The table holds `2·R` connection slots and live links,
    /// where `R` is the transport connection-registry capacity.
    pub(crate) fn new(
        epoch: OnionProcessEpoch,
        filter_key: OnionReplayFilterKey,
        link_registry_capacity: NonZeroUsize,
    ) -> Self {
        let capacity = link_registry_capacity.get().saturating_mul(2);
        let (floor_units, pool_units) = Self::slot_shares(capacity);
        Self {
            epoch,
            clock_ms: 0,
            swept_quantum: 0,
            replay: ReplayStore::new(filter_key),
            global: QuantumLedger::default(),
            capacity,
            floor_units,
            pool_units,
            pool_load: 0,
            slot_roster: Arc::new(SlotRoster::new(capacity)),
            owns_slot_roster: true,
            slots: BTreeMap::new(),
            peer_slots: BTreeMap::new(),
        }
    }

    /// Attach a shared [`SlotRoster`] (such as `PeerRing::edges()`) to this admission state.
    pub(crate) fn with_slot_roster(mut self, slot_roster: Arc<SlotRoster>) -> Self {
        let quantum = self.clock_ms / ONION_EXPIRY_QUANTUM_MS;
        slot_roster.set_extra_floor_use(self.floor_use_at(quantum));
        self.slot_roster = slot_roster;
        self.owns_slot_roster = false;
        self
    }

    /// The connection slot currently bound to `did` (either live or retained while draining).
    #[cfg(all(test, rings_native))]
    pub(crate) fn slot_of(&self, did: Did) -> Option<OnionSlotId> {
        self.peer_slots.get(&did).copied()
    }

    /// The per-slot floor `f` and shared pool `P` for `capacity` connection slots.
    #[cfg(all(test, rings_native))]
    pub(crate) const fn shares(&self) -> (u32, u32) {
        (self.floor_units, self.pool_units)
    }

    /// Compute `(floor_units, pool_units)` such that `capacity · floor_units + pool_units = G` and
    /// `floor_units > 0` whenever `capacity ≤ G`.
    fn slot_shares(capacity: usize) -> (u32, u32) {
        let slots = u32::try_from(capacity).unwrap_or(u32::MAX).max(1);
        let half_global = ONION_ADMISSION_GLOBAL_UNITS / 2;
        let floor_units = (half_global / slots)
            .min(ONION_ADMISSION_SLOT_FLOOR_UNITS)
            .max(u32::from(slots <= ONION_ADMISSION_GLOBAL_UNITS));
        let reserved = slots.saturating_mul(floor_units);
        let pool_units = ONION_ADMISSION_GLOBAL_UNITS.saturating_sub(reserved);
        (floor_units, pool_units)
    }

    /// Whether the wall clock has rolled back far enough behind the monotone clock that honest
    /// layers fail the window: `now < clock − X₀`. The shell then renews into a fresh epoch.
    pub(crate) const fn is_rolled_back_at(&self, now_ms: u128) -> bool {
        now_ms.saturating_add(ONION_EXPIRY_OFFSET_MS) < self.clock_ms
    }

    /// The reset `ρ(epoch, key)`: the epoch reset. It requires a fresh `epoch` and otherwise
    /// changes nothing. On success the state is the initial state of `epoch` with probe key
    /// `filter_key` and the same live links on their assigned slots, each with a zero-load ledger:
    /// `ρ = reconcile(live_links(S)) ∘ clear`. It refuses nothing, since the table held these links
    /// before, so it keeps no link the shell would have to close.
    pub(crate) fn renew(
        &mut self,
        epoch: OnionProcessEpoch,
        filter_key: OnionReplayFilterKey,
    ) -> Result<(), OnionEpochNotFresh> {
        (epoch != self.epoch)
            .then(|| {
                let (slots, peer_slots) = std::mem::take(&mut self.slots)
                    .into_iter()
                    .filter(|(_, slot)| !slot.live.is_empty())
                    .fold(
                        (BTreeMap::new(), BTreeMap::new()),
                        |(mut slots, mut peer_slots), (slot_id, slot)| {
                            peer_slots.extend(slot.live.keys().map(|&did| (did, slot_id)));
                            slots.insert(slot_id, SlotLedger {
                                ledger: QuantumLedger::default(),
                                live: slot.live,
                                retained: BTreeMap::new(),
                            });
                            (slots, peer_slots)
                        },
                    );
                self.slot_roster
                    .set_extra_floor_use(SlotFloorUse::default());
                *self = Self {
                    epoch,
                    clock_ms: 0,
                    swept_quantum: 0,
                    replay: ReplayStore::new(filter_key),
                    global: QuantumLedger::default(),
                    capacity: self.capacity,
                    floor_units: self.floor_units,
                    pool_units: self.pool_units,
                    pool_load: 0,
                    slot_roster: Arc::clone(&self.slot_roster),
                    owns_slot_roster: self.owns_slot_roster,
                    slots,
                    peer_slots,
                };
            })
            .ok_or(OnionEpochNotFresh)
    }

    /// The step `δ` on `Reconcile(live)`: make the live-link set equal core's registry snapshot
    /// `live`, up to refusals. Every live link absent from the snapshot is closed first, which
    /// frees its slot. Then every snapshot link is opened in slot/generation order (never `Did`
    /// order), with the table's usual refusal. Afterwards the live set is `live \ refused`, no link
    /// that was already live and is in the snapshot is refused, and a second reconciliation with
    /// the same snapshot, in any order, changes nothing. It seeds the links admitted before the
    /// data plane was installed. The snapshot must be linearised with the event stream (see the
    /// module laws).
    pub(crate) fn reconcile(
        &mut self,
        now_ms: u128,
        live: impl IntoIterator<Item = OnionAdmissionLink>,
    ) -> OnionRefusedLinks {
        self.advance(now_ms);
        let mut seen = BTreeSet::new();
        let mut snapshot = live
            .into_iter()
            .filter(|&link| seen.insert(link))
            .collect::<Vec<_>>();
        let mut absent =
            self.slots
                .iter()
                .flat_map(|(&slot_id, slot)| {
                    slot.live.iter().flat_map(move |(&did, generations)| {
                        generations.iter().copied().map(move |generation| {
                            (slot_id, OnionAdmissionLink::new(did, generation))
                        })
                    })
                })
                .filter(|(_, link)| !seen.contains(link))
                .collect::<Vec<_>>();
        absent.sort_by_key(|(slot_id, link)| (*slot_id, link.generation()));
        absent
            .into_iter()
            .for_each(|(_, link)| self.link_closed(now_ms, link));
        snapshot.sort_by_key(|&link| {
            let already_live = self.is_link_live(link);
            let assigned = self.assigned_slot_of(link.peer());
            (
                !already_live,
                assigned.is_none(),
                assigned.unwrap_or(OnionSlotId::new(0)),
                link.generation(),
            )
        });
        let refused = snapshot
            .into_iter()
            .filter(|&link| self.link_opened(now_ms, link).is_err())
            .collect();
        OnionRefusedLinks(refused)
    }

    /// The step `δ` on `LinkOpened(link)`: make `link` live, assigning its peer to a connection
    /// slot via [`SlotRoster`] (cleanest-first when connecting fresh, or resuming its retained
    /// slot when reconnecting while its prior slot still carries load). Opening a live link again
    /// changes nothing. When `2·R` links are already live, the table refuses the link, which stays
    /// not live.
    pub(crate) fn link_opened(
        &mut self,
        now_ms: u128,
        link: OnionAdmissionLink,
    ) -> Result<(), OnionLinkTableFull> {
        let quantum = self.advance(now_ms);
        match self.is_link_live(link) {
            true => Ok(()),
            false => (self.live_link_count() < self.capacity)
                .then_some(())
                .ok_or(OnionLinkTableFull)
                .and_then(|()| {
                    let did = link.peer();
                    let slot_id = self.acquire_slot_for(did, quantum)?;
                    let slot = self.slots.entry(slot_id).or_default();
                    slot.retained.remove(&did);
                    slot.live.entry(did).or_default().insert(link.generation());
                    self.peer_slots.insert(did, slot_id);
                    Ok(())
                }),
        }
    }

    /// The step `δ` on `LinkClosed(link)`: make `link` no longer live, free its slot in the
    /// [`SlotRoster`] when `link`'s peer has no remaining live generations on the slot, and
    /// release the slot ledger at once if it has no remaining live links and zero load. It is
    /// idempotent: closing a link that is not live (unknown, refused, or already closed) changes
    /// nothing, so a close can never take another generation's liveness away.
    pub(crate) fn link_closed(&mut self, now_ms: u128, link: OnionAdmissionLink) {
        let quantum = self.advance(now_ms);
        let did = link.peer();
        let removed_last_for_peer = self.peer_slots.get(&did).copied().and_then(|slot_id| {
            let slot = self.slots.get_mut(&slot_id)?;
            let last = slot.remove_live_generation(did, link.generation())?;
            last.then(|| {
                let remaining_peer = slot.live.first_key_value().map(|(&peer, _)| peer);
                let has_load = slot.ledger.load(quantum) > 0;
                let slot_empty = slot.live.is_empty();
                (slot_id, remaining_peer, has_load, slot_empty)
            })
        });
        if let Some((slot_id, remaining_peer, has_load, slot_empty)) = removed_last_for_peer {
            self.release_roster_slot_on_close(did, slot_id, remaining_peer);
            match (has_load, slot_empty) {
                (true, _) => {
                    if let Some(slot) = self.slots.get_mut(&slot_id) {
                        slot.retained.insert(did, quantum);
                    }
                }
                (false, false) => {
                    self.peer_slots.remove(&did);
                }
                (false, true) => {
                    self.peer_slots.remove(&did);
                    if let Some(removed) = self.slots.remove(&slot_id) {
                        removed.retained.into_keys().for_each(|retained_did| {
                            self.peer_slots.remove(&retained_did);
                        });
                    }
                }
            }
            self.slot_roster
                .set_extra_floor_use(self.floor_use_at(quantum));
        }
    }

    fn release_roster_slot_on_close(
        &self,
        did: Did,
        slot_id: OnionSlotId,
        remaining_peer: Option<Did>,
    ) {
        if self.owns_slot_roster
            && self.slot_roster.principal_of(Some(did))
                == u16::try_from(slot_id.index()).map_or(Principal::Unbound, Principal::Slot)
        {
            self.slot_roster.depart(did);
            remaining_peer
                .filter(|&peer| !self.slot_roster.holds(peer))
                .into_iter()
                .for_each(|peer| {
                    self.slot_roster.arrive_preferring(
                        peer,
                        u16::try_from(slot_id.index()).ok(),
                        &SlotFloorUse::default(),
                    );
                });
        }
    }

    /// The step `δ` on `Charge(link, u(b))` for a received cell: charge it `u(b) = b / 16 KiB`
    /// units, the units of its own class, and pair it with the token (#843). This is the only
    /// source of tokens outside this module, so a peel is always paid for, with its cell's units
    /// (see `sphinx::cell`).
    ///
    /// # Errors
    ///
    /// The [`OnionChargeRejection`] of the unit step; nothing is charged, and the cell must be
    /// dropped undecrypted.
    pub(crate) fn charge(
        &mut self,
        now_ms: u128,
        link: OnionAdmissionLink,
        cell: OnionCell,
    ) -> Result<Charged<OnionCell>, OnionChargeRejection> {
        let units = OnionAdmissionUnits(cell.class().units());
        self.charge_units(now_ms, link, units)
            .map(|charge| Charged::new(cell, charge))
    }

    /// The step `δ` on `Charge(link, u)`: charge a cell received on `link` to its connection
    /// slot's ledger (using its reserved floor `f` first and the shared pool `P` for any overflow)
    /// and to the global ledger, both or neither, before its key is computed. Only a live link is
    /// charged. Private: the crate charges cells through [`Self::charge`].
    ///
    /// ```text
    ///  now := max(now, clock);  drop R[x] for x ≤ now;  s = ⌊now / Q⌋;  sweep if s is new
    ///   ├─ link not live ──────────────────────────────────→ LinkNotLive
    ///   ├─ load_slot(s) + u > B ───────────────────────────→ SenderBudget
    ///   ├─ Δo > 0 ∧ pool_load(s) + Δo > P ─────────────────→ GlobalBudget
    ///   ├─ load_global(s) + u > G ─────────────────────────→ GlobalBudget
    ///   └─ charge slot, pool, and global ledgers ──────────→ Ok(token = (clock, epoch_i))
    /// ```
    fn charge_units(
        &mut self,
        now_ms: u128,
        link: OnionAdmissionLink,
        units: OnionAdmissionUnits,
    ) -> Result<OnionAdmissionCharge, OnionChargeRejection> {
        let quantum = self.advance(now_ms);
        let did = link.peer();
        let slot_id = self
            .peer_slots
            .get(&did)
            .copied()
            .ok_or(OnionChargeRejection::LinkNotLive)?;
        let slot = self
            .slots
            .get_mut(&slot_id)
            .filter(|slot| slot.contains_live(did, link.generation()))
            .ok_or(OnionChargeRejection::LinkNotLive)?;
        let old_over = slot.ledger.load(quantum).saturating_sub(self.floor_units);
        let charged_slot = slot
            .ledger
            .charged(quantum, units.get(), ONION_ADMISSION_SENDER_UNITS)
            .ok_or(OnionChargeRejection::SenderBudget)?;
        let new_over = charged_slot.load(quantum).saturating_sub(self.floor_units);
        let overflow = new_over.saturating_sub(old_over);
        let next_pool_load = if overflow > 0 {
            self.pool_load
                .checked_add(overflow)
                .filter(|&pool| pool <= self.pool_units)
                .ok_or(OnionChargeRejection::GlobalBudget)?
        } else {
            self.pool_load
        };
        let charged_global = self
            .global
            .charged(quantum, units.get(), ONION_ADMISSION_GLOBAL_UNITS)
            .ok_or(OnionChargeRejection::GlobalBudget)?;
        slot.ledger = charged_slot;
        self.global = charged_global;
        self.pool_load = next_pool_load;
        self.slot_roster
            .set_extra_floor_use(self.floor_use_at(quantum));
        Ok(OnionAdmissionCharge {
            arrival_ms: self.clock_ms,
            epoch: self.epoch,
        })
    }

    /// The live link of `did`, against which its cells are charged: core admits at most one
    /// generation of a DID at a time, so this is that generation. If a lost `Retired` has left
    /// two live until the next reconciliation, the newest is taken, since core's generations
    /// increase.
    pub(crate) fn live_link(&self, did: Did) -> Option<OnionAdmissionLink> {
        let slot_id = self.peer_slots.get(&did).copied()?;
        self.slots
            .get(&slot_id)
            .and_then(|slot| slot.live.get(&did))
            .and_then(|generations| generations.last().copied())
            .map(|generation| OnionAdmissionLink::new(did, generation))
    }

    /// The units charged to `did`'s connection slot in the window of the state's clock, for tests
    /// of the Paid law.
    #[cfg(all(test, rings_native))]
    pub(crate) fn sender_units(&self, did: Did) -> u32 {
        self.peer_slots
            .get(&did)
            .and_then(|slot_id| self.slots.get(slot_id))
            .map_or(0, |slot| {
                slot.ledger.load(self.clock_ms / ONION_EXPIRY_QUANTUM_MS)
            })
    }

    /// Every live link, in DID and generation order: the table's own view, linearised with the
    /// facts it has applied.
    pub(crate) fn live_links(&self) -> Vec<OnionAdmissionLink> {
        let mut links = self
            .slots
            .values()
            .flat_map(|slot| {
                slot.live.iter().flat_map(|(&did, generations)| {
                    generations
                        .iter()
                        .map(move |&generation| OnionAdmissionLink::new(did, generation))
                })
            })
            .collect::<Vec<_>>();
        links.sort();
        links
    }

    /// The number of live links, `Σ_κ Σ_d |live(κ, d)|`, derived rather than stored.
    fn live_link_count(&self) -> usize {
        self.slots.values().map(SlotLedger::live_link_count).sum()
    }

    /// Whether `link` is currently live on its peer's slot.
    fn is_link_live(&self, link: OnionAdmissionLink) -> bool {
        self.peer_slots
            .get(&link.peer())
            .and_then(|slot_id| self.slots.get(slot_id))
            .is_some_and(|slot| slot.contains_live(link.peer(), link.generation()))
    }

    fn roster_slot_of(&self, did: Did) -> Option<OnionSlotId> {
        match self.slot_roster.principal_of(Some(did)) {
            Principal::Slot(slot) if usize::from(slot) < self.capacity => {
                Some(OnionSlotId::new(usize::from(slot)))
            }
            _ => None,
        }
    }

    /// The slot already bound to `did` in `peer_slots` or pre-assigned in `slot_roster`, if within
    /// `0..capacity`.
    fn assigned_slot_of(&self, did: Did) -> Option<OnionSlotId> {
        self.peer_slots
            .get(&did)
            .copied()
            .or_else(|| self.roster_slot_of(did))
    }

    fn resume_existing_slot(&self, did: Did, quantum: u128) -> Option<OnionSlotId> {
        self.peer_slots.get(&did).copied().inspect(|&slot_id| {
            let slot_was_free = self.slots.get(&slot_id).is_none_or(SlotLedger::is_free);
            if self.owns_slot_roster && slot_was_free && !self.slot_roster.holds(did) {
                let floor_use = self.floor_use_at(quantum);
                self.slot_roster.arrive_preferring(
                    did,
                    u16::try_from(slot_id.index()).ok(),
                    &floor_use,
                );
            }
        })
    }

    fn acquire_roster_slot(&self, did: Did, floor_use: &SlotFloorUse) -> Option<OnionSlotId> {
        self.owns_slot_roster
            .then(|| {
                self.slot_roster.arrive(did, floor_use);
                self.roster_slot_of(did)
                    .filter(|slot_id| self.slots.get(slot_id).is_none_or(SlotLedger::is_free))
            })
            .flatten()
    }

    /// Resolve or assign a connection slot in `0..capacity` for `did` at `quantum`.
    fn acquire_slot_for(
        &mut self,
        did: Did,
        quantum: u128,
    ) -> Result<OnionSlotId, OnionLinkTableFull> {
        let floor_use = self.floor_use_at(quantum);
        self.resume_existing_slot(did, quantum)
            .or_else(|| {
                self.roster_slot_of(did).filter(|slot_id| {
                    self.slots.get(slot_id).is_none_or(SlotLedger::is_free)
                        && self.is_cleanest_free_roster_slot(*slot_id, &floor_use)
                })
            })
            .or_else(|| {
                self.acquire_roster_slot(did, &floor_use)
                    .or_else(|| self.cleanest_free_slot(&floor_use))
            })
            .ok_or(OnionLinkTableFull)
    }

    fn is_cleanest_free_roster_slot(
        &self,
        assigned: OnionSlotId,
        floor_use: &SlotFloorUse,
    ) -> bool {
        let assigned_use = u16::try_from(assigned.index())
            .map_or(FloorUse::CLEAN, |slot_u16| floor_use.of(slot_u16));
        assigned_use == FloorUse::CLEAN
            || self.cleanest_free_slot(floor_use).is_none_or(|cleanest| {
                let cleanest_use = u16::try_from(cleanest.index())
                    .map_or(FloorUse::CLEAN, |slot_u16| floor_use.of(slot_u16));
                assigned_use <= cleanest_use
            })
    }

    /// The floor use `min(load(κ), f) / f` of every charged slot at `quantum` (paper, Def. 1).
    pub(crate) fn floor_use_at(&self, quantum: u128) -> SlotFloorUse {
        let floor = usize::try_from(self.floor_units).unwrap_or(1).max(1);
        SlotFloorUse::from_slots(self.slots.iter().filter_map(|(&slot_id, slot)| {
            let slot_u16 = u16::try_from(slot_id.index()).ok()?;
            let load = usize::try_from(slot.ledger.load(quantum)).unwrap_or(usize::MAX);
            (load > 0).then(|| (slot_u16, FloorUse::of(load, floor)))
        }))
    }

    /// Fallback cleanest-first slot selection over `0..capacity` when an external `SlotRoster`
    /// was configured with fewer slots than `capacity`.
    fn cleanest_free_slot(&self, floor_use: &SlotFloorUse) -> Option<OnionSlotId> {
        (0..self.capacity)
            .filter(|&index| {
                self.slots
                    .get(&OnionSlotId::new(index))
                    .is_none_or(SlotLedger::is_free)
            })
            .min_by_key(|&index| {
                let use_at =
                    u16::try_from(index).map_or(FloorUse::CLEAN, |slot_u16| floor_use.of(slot_u16));
                (use_at, index)
            })
            .map(OnionSlotId::new)
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
    /// A wire expiry off the grid never reaches this step: the layer decoder parses it with
    /// `OnionExpiry::from_wire_ms`, and `None` drops the cell, which is already charged.
    pub(crate) fn admit(
        &mut self,
        now_ms: u128,
        charge: OnionAdmissionCharge,
        layer: OnionAdmissionLayer,
    ) -> Result<(), OnionAdmissionRejection> {
        let OnionAdmissionCharge { arrival_ms, epoch } = charge;
        self.advance(now_ms);
        (layer.epoch == self.epoch && epoch == self.epoch)
            .then_some(())
            .ok_or(OnionAdmissionRejection::StaleEpoch)?;
        (layer.expiry.admissible_at(arrival_ms) && !layer.expiry.has_passed_at(self.clock_ms))
            .then_some(())
            .ok_or(OnionAdmissionRejection::OutsideWindow)?;
        let probe = self.replay.probe(layer.tag);
        (!self.replay.contains(layer.expiry, &probe))
            .then(|| self.replay.insert(layer.expiry, &probe))
            .ok_or(OnionAdmissionRejection::Replayed)
    }

    /// Advance the monotone clock to `max(now, clock)`, drop every filter whose `x` has passed,
    /// and, on entering a new quantum, release every slot ledger that has become releasable and
    /// refresh the aggregate pool load. Return the current arrival quantum `⌊clock / Q⌋`.
    fn advance(&mut self, now_ms: u128) -> u128 {
        self.clock_ms = self.clock_ms.max(now_ms);
        self.replay.forget_through(self.clock_ms);
        let quantum = self.clock_ms / ONION_EXPIRY_QUANTUM_MS;
        if quantum > self.swept_quantum {
            let drained_dids = self
                .slots
                .values_mut()
                .flat_map(|slot| slot.take_drained_retained(quantum))
                .collect::<Vec<_>>();
            drained_dids.into_iter().for_each(|did| {
                self.peer_slots.remove(&did);
            });
            self.slots.retain(|_, slot| !slot.is_releasable_at(quantum));
            self.pool_load = self
                .slots
                .values()
                .map(|slot| slot.ledger.load(quantum).saturating_sub(self.floor_units))
                .fold(0_u32, u32::saturating_add);
            self.swept_quantum = quantum;
            self.slot_roster
                .set_extra_floor_use(self.floor_use_at(quantum));
        }
        quantum
    }
}
