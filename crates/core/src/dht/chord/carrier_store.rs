//! The store of a carrier kind, and the data store factored into an element side and a remove
//! side (#920; removal-stability paper, Laws 2–4 and 6).
//!
//! A data carrier `x` is stored as two records under one key: its element side
//! `E(x) = (V, ∅, ⊥, β)` in the data store, at `(Cached, Rebuild)`, and its remove side
//! `R(x) = (∅, D, ρ, 0)` in the removal store, at `(Durable, Retained)`. A read glues them,
//! `x = E(x) ⊔ R(x)`; the remove side carries the least stamped bound, so read alone it is live
//! only through its unstable removes and answers a lookup as a miss.
//!
//! Laws:
//! - Eviction acts on elements: the data store's budget may evict the element side of any
//!   carrier; the removal store refuses and never evicts, so a remove leaves it only by
//!   retirement at stability, an acknowledged move, or a loss event (Law 1).
//! - Write order: a put writes the remove side first, and only when it changed, then the element
//!   side; a refusal of the remove side changes neither. A removal removes the element side, then
//!   the remove side. Under the removal store's durability, every crash leaves an element side
//!   no newer than the remove side it is glued to, which the recovery pass normalizes.
//! - Glue: at runtime an element side is always written from the same normalized carrier as its
//!   remove side, after it, so once both sides commit it holds no add that remove side covers or
//!   floors, and a read glues the two without hashing. A crash that rolls a cached element side
//!   back is normalized by the recovery pass on first use; a cross-store write that is still in
//!   flight or failed/cancelled before its element-side write completed leaves the key marked
//!   dirty so reads join in full (and repair the element side when idle); and a whole carrier
//!   written before the factoring, whose element side still holds removes, is joined in full.
//! - Shares (load-bounds paper, Sec. V, re-keying Law 6 of the removal-stability paper): each
//!   unstable remove, and the register, is an item charged to the principal that delivered the
//!   transition raising it: the connection slot of the edge an operation or a sync arrived on,
//!   this node for its own writes and repairs, or the custody principal for an ownership
//!   hand-off from the range's previous owner. The items are accounted under slot accounting
//!   with [`REMOVAL_SHARES`], so the store holds at most `R(s)·128 + 39 936` unstable items,
//!   [`REMOVAL_STORE_MAX_REMOVES`] at the default `s = 3`. A transition that raises an item the
//!   accounting refuses is refused whole; one that raises none is never refused for room. An
//!   honest remove is therefore refused only by its topic's cap, or by its principal's floor,
//!   fair claim or share, whatever other parties hold (Theorem 1, per item); a slot's removes
//!   outlive its edge until they are stable (Prop. 1).
//! - Locks: a read takes no lock once the store is recovered, so it never waits on a write's
//!   I/O. The write lock is taken only by the recovery pass, once, and by a write whose remove
//!   side changed, across its admission and its remove-side write; a write that raises and drops
//!   nothing (an `Extend`, a heartbeat) never takes it. The accounting is locked for one pure
//!   step at a time beneath it. Writes to one key are ordered by the storage transition their
//!   callers hold, which is what keeps `R` before `E` per key.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::task::Poll;

use futures::future::poll_fn;
use futures::lock::Mutex as FuturesMutex;
use serde::Deserialize;
use serde::Serialize;

use super::EntryStorage;
#[cfg(doc)]
use crate::consts::REMOVAL_STORE_MAX_REMOVES;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::entry::ElementDigest;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryCrdt;
use crate::dht::entry::EntryDot;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryTombstone;
use crate::dht::entry::EntryVersion;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::fair_admission::Principal;
use crate::fair_admission::Shares;
use crate::fair_admission::SlotAccounting;
use crate::fair_admission::SlotFloorUse;
use crate::fair_admission::TableAccounting;
use crate::storage::KvStorageInterface;

/// The store of remove sides (#920): a `RecordAuthority::Retained` store in production, which
/// refuses and never evicts.
pub type RemovalStorage =
    Box<rings_runtime::maybe_send_sync!(dyn KvStorageInterface<RemovalRecord>)>;

/// The shares of the removal store (load-bounds paper, Sec. V): `f = 128` items per slot, a
/// pool of 16 384, a local share of 7 168 and a custody share of 16 384 for the hand-off of a
/// range this node owns, so `B(s) = R(s)·128 + 39 936`, 81 920 at `s = 3` and 256 more per
/// successor.
pub(crate) const REMOVAL_SHARES: Shares = Shares {
    floor: 128,
    pool: 16_384,
    local: 7_168,
    inbox: 0,
    custody: 16_384,
};

/// One remove of a remove side, with its charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChargedRemove {
    /// The remove.
    pub tombstone: EntryTombstone,
    /// The principal that delivered the transition raising it.
    pub charge: Principal,
}

/// The register of a remove side, with its charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChargedRegister {
    /// The register.
    pub version: EntryVersion,
    /// The principal that delivered the transition raising it.
    pub charge: Principal,
}

/// The remove side `R(x)` of one data carrier as the removal store keeps it: its removes and
/// register, each with its charge. The charges are local to this owner and never sent.
///
/// Law: `as_entry` forgets the charges, so `R(x) = record.as_entry()` for the record of `x`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalRecord {
    /// The carrier's resource key.
    pub did: Did,
    /// The register, if the carrier holds one.
    pub register: Option<ChargedRegister>,
    /// The removes, in the carrier's order.
    pub removes: Vec<ChargedRemove>,
}

/// The instant a remove or register at `version` is stable: `τ + H + σ`.
fn stable_at(version: &EntryVersion) -> u128 {
    let horizon_ms = EntryKind::Data.element_horizon_ms().unwrap_or_default();
    version
        .logical_time_ms
        .saturating_add(u128::from(horizon_ms))
        .saturating_add(TS_OFFSET_TOLERANCE_MS)
}

const CO_REGISTER_DIGEST: ElementDigest = ElementDigest(*b"rings-core:co-register-floor\0\0\0\0");

impl RemovalRecord {
    /// The remove side of the normalized carrier `value`, charging `charge` with what it raised
    /// over `previous` and keeping the earlier charge of the rest; `None` when `value` holds no
    /// remove and no register.
    fn of(value: &Entry, previous: Option<&Self>, charge: Principal) -> Option<Self> {
        let held = |tombstone: &EntryTombstone| {
            previous.and_then(|previous| {
                previous
                    .removes
                    .iter()
                    .find(|held| held.tombstone == *tombstone)
                    .map(|held| held.charge)
            })
        };
        let mut removes = value
            .crdt
            .tombstones
            .iter()
            .map(|tombstone| ChargedRemove {
                tombstone: *tombstone,
                charge: held(tombstone).unwrap_or(charge),
            })
            .collect::<Vec<_>>();
        for &version in &value.crdt.co_registers {
            let encoded = EntryTombstone {
                element: CO_REGISTER_DIGEST,
                dot: EntryDot {
                    version,
                    index: u32::MAX,
                },
            };
            removes.push(ChargedRemove {
                tombstone: encoded,
                charge: held(&encoded).unwrap_or(charge),
            });
        }
        let register = value.crdt.register.map(|version| ChargedRegister {
            version,
            charge: previous
                .and_then(|previous| previous.register)
                .filter(|held| held.version == version)
                .map(|held| held.charge)
                .unwrap_or(charge),
        });
        if removes.is_empty() && register.is_none() {
            return None;
        }
        let record = Self {
            did: value.did,
            register,
            removes,
        };
        Some(record)
    }

    /// `R(x)`: the remove side as a carrier, with no element and the least stamped bound.
    fn as_entry(&self) -> Entry {
        let mut tombstones = Vec::with_capacity(self.removes.len());
        let mut co_registers = Vec::new();
        for remove in &self.removes {
            if remove.tombstone.element == CO_REGISTER_DIGEST
                && remove.tombstone.dot.index == u32::MAX
            {
                co_registers.push(remove.tombstone.dot.version);
            } else {
                tombstones.push(remove.tombstone);
            }
        }
        co_registers.sort();
        Entry {
            did: self.did,
            data: Vec::new(),
            kind: EntryKind::Data,
            crdt: EntryCrdt {
                register: self.register.map(|register| register.version),
                dots: Vec::new(),
                co_dots: Vec::new(),
                co_registers,
                proofs: Vec::new(),
                tombstones,
            },
            expires_at_ms: Some(0),
        }
    }

    /// Every item of this record: its principal and the instant it is stable.
    fn items(&self) -> impl Iterator<Item = (Principal, u128)> + '_ {
        self.removes
            .iter()
            .map(|remove| (remove.charge, stable_at(&remove.tombstone.dot.version)))
            .chain(
                self.register
                    .iter()
                    .map(|register| (register.charge, stable_at(&register.version))),
            )
    }
}

/// `E(x)`: the carrier `value` without its remove side.
fn element_side(value: &Entry) -> Entry {
    Entry {
        did: value.did,
        data: value.data.clone(),
        kind: value.kind,
        crdt: EntryCrdt {
            register: None,
            dots: value.crdt.dots.clone(),
            co_dots: value.crdt.co_dots.clone(),
            co_registers: Vec::new(),
            proofs: value.crdt.proofs.clone(),
            tombstones: Vec::new(),
        },
        expires_at_ms: value.expires_at_ms,
    }
}

/// `E ⊔ R` by the glue law: concatenation when the element side is known to come from the same
/// normalized generation as `removals` and holds no remove side of its own, and the full join
/// otherwise.
fn glue(
    elements: Option<Entry>,
    removals: Option<&RemovalRecord>,
    dirty: bool,
) -> Result<Option<Entry>> {
    let Some(removals) = removals else {
        return Ok(elements);
    };
    let Some(elements) = elements else {
        return Ok(Some(removals.as_entry()));
    };
    if dirty
        || !elements.crdt.tombstones.is_empty()
        || elements.crdt.register.is_some()
        || !elements.crdt.co_registers.is_empty()
    {
        return elements.join(removals.as_entry()).map(Some);
    }
    let remove_side = removals.as_entry();
    Ok(Some(Entry {
        crdt: EntryCrdt {
            register: remove_side.crdt.register,
            dots: elements.crdt.dots,
            co_dots: elements.crdt.co_dots,
            co_registers: remove_side.crdt.co_registers,
            proofs: elements.crdt.proofs,
            tombstones: remove_side.crdt.tombstones,
        },
        expires_at_ms: elements.expires_at_ms.max(remove_side.expires_at_ms),
        ..elements
    }))
}

/// The items one transition of a remove side changes: those that left and those it raised.
struct ItemChanges {
    /// The items `previous` held and `next` does not, unstable at the transition's instant.
    left: Vec<(Principal, u128)>,
    /// The items `next` holds and `previous` did not, unstable at that instant.
    raised: Vec<(Principal, u128)>,
}

impl ItemChanges {
    /// The multiset differences of the items of `previous` and `next` unstable at `now_ms`
    /// (pure).
    fn of(previous: Option<&RemovalRecord>, next: Option<&RemovalRecord>, now_ms: u128) -> Self {
        let mut counts = BTreeMap::<(Principal, u128), isize>::new();
        let unstable = |record: Option<&RemovalRecord>| {
            record
                .into_iter()
                .flat_map(RemovalRecord::items)
                .filter(|(_, at)| *at > now_ms)
                .collect::<Vec<_>>()
        };
        for item in unstable(previous) {
            *counts.entry(item).or_default() -= 1;
        }
        for item in unstable(next) {
            *counts.entry(item).or_default() += 1;
        }
        let mut changes = Self {
            left: Vec::new(),
            raised: Vec::new(),
        };
        for (item, count) in counts {
            let into = match count < 0 {
                true => &mut changes.left,
                false => &mut changes.raised,
            };
            into.extend(std::iter::repeat_n(item, count.unsigned_abs()));
        }
        changes
    }

    /// Apply the changes to `accounting` at `now_ms`: forget the items that left, then admit
    /// and charge each item raised, all or none.
    ///
    /// Post: on `Err(RemovalCapacityExhausted)` the accounting is as it was, but for the items
    /// that became stable by `now_ms`, which it forgets either way.
    fn apply(&self, accounting: &mut SlotAccounting, now_ms: u128) -> Result<()> {
        accounting.advance(now_ms);
        for (principal, at) in self.left.iter() {
            accounting.discharge(*principal, *at);
        }
        for (index, (principal, at)) in self.raised.iter().enumerate() {
            if let Err(refusal) = accounting.admit(*principal) {
                Self::undo(&self.left, self.raised.iter().take(index), accounting);
                return Err(Error::RemovalCapacityExhausted {
                    introducer: *principal,
                    refusal,
                });
            }
            accounting.charge(*principal, *at);
        }
        Ok(())
    }

    /// Revert the changes applied to `accounting`: forget the raised items and charge the left
    /// ones again, with no admission, since they held their places before.
    fn revert(&self, accounting: &mut SlotAccounting) {
        Self::undo(&self.left, self.raised.iter(), accounting);
    }

    /// Forget the `raised` items and charge the `left` ones again.
    fn undo<'a>(
        left: &[(Principal, u128)],
        raised: impl Iterator<Item = &'a (Principal, u128)>,
        accounting: &mut SlotAccounting,
    ) {
        for (principal, at) in raised {
            accounting.discharge(*principal, *at);
        }
        for (principal, at) in left {
            accounting.charge(*principal, *at);
        }
    }
}

/// An in-flight removal-store operation stored inside [`RemovalIndex::writes`] so a subsequent
/// recovery or write can await its completion if the original caller was cancelled mid-await.
type InFlightRemovalOp = Pin<Box<rings_runtime::maybe_send!(dyn Future<Output = ()> + 'static)>>;

/// Tracks keys whose remove side may be newer than their element side because a two-stage write
/// touched `removals` and its matching `elements` write has not yet finished.
#[derive(Debug, Default)]
struct DirtyElements {
    /// Keys whose stored element side is not known to match the stored remove side's generation.
    dirty: BTreeSet<String>,
    /// Active `CarrierStore::put` or `CarrierStore::remove` calls per key.
    in_flight: BTreeMap<String, usize>,
}

/// Shared state of [`RemovalIndex`], cloned into in-flight removal-store futures so a cancelled
/// caller leaves the operation inside `writes` for the next operation or recovery pass to finish.
#[derive(Debug)]
struct RemovalIndexInner {
    /// Whether the recovery pass has run: read without the lock once set.
    recovered: AtomicBool,
    /// The items, under edge accounting with [`REMOVAL_SHARES`].
    accounting: TableAccounting,
    /// Keys with an in-flight or incomplete cross-store (`R` then `E`) write.
    dirty_elements: Mutex<DirtyElements>,
}

/// The accounting of a removal store, filled by the recovery pass on first use (the lock law of
/// the module documentation).
pub(crate) struct RemovalIndex {
    /// Shared recovery flag, accounting table, and dirty-key state.
    inner: Arc<RemovalIndexInner>,
    /// The write lock: taken by the recovery pass and by a write that changes a remove side,
    /// holding any in-flight removal-store future across caller cancellation.
    writes: FuturesMutex<Option<InFlightRemovalOp>>,
}

impl fmt::Debug for RemovalIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemovalIndex")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl RemovalIndex {
    /// The floor use of every slot in the removal store at `now_ms` (load-bounds paper, Def. 1).
    pub(crate) fn floor_use_at(&self, now_ms: u128) -> SlotFloorUse {
        self.inner.accounting.floor_use_at(now_ms)
    }

    /// The number of items held by `principal`.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn held(&self, principal: Principal) -> usize {
        self.inner.accounting.lock().held(principal)
    }

    /// An index not yet recovered, accounting in `accounting`.
    pub(crate) fn new(accounting: TableAccounting) -> Self {
        Self {
            inner: Arc::new(RemovalIndexInner {
                recovered: AtomicBool::new(false),
                accounting,
                dirty_elements: Mutex::new(DirtyElements::default()),
            }),
            writes: FuturesMutex::new(None),
        }
    }

    /// Lock `dirty_elements` for one pure step.
    fn dirty_guard(&self) -> MutexGuard<'_, DirtyElements> {
        self.inner
            .dirty_elements
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Return `(dirty, in_flight)` for `key`.
    fn dirty_state(&self, key: &str) -> (bool, bool) {
        let state = self.dirty_guard();
        (state.dirty.contains(key), state.in_flight.contains_key(key))
    }

    /// Snapshot `(dirty_keys, in_flight_keys)` across all keys.
    fn dirty_snapshot(&self) -> (BTreeSet<String>, BTreeSet<String>) {
        let state = self.dirty_guard();
        (
            state.dirty.clone(),
            state.in_flight.keys().cloned().collect(),
        )
    }

    /// Clear `key` from `dirty` if no `put` or `remove` is currently in flight on `key`.
    fn clear_dirty_if_idle(&self, key: &str) {
        let mut state = self.dirty_guard();
        if !state.in_flight.contains_key(key) {
            state.dirty.remove(key);
        }
    }
}

/// Tracks a `CarrierStore::put` or `CarrierStore::remove` across its remove-side and element-side
/// writes so a failure or cancellation after `removals` changes leaves `key` marked dirty until
/// normalized.
struct ElementSyncGuard<'a> {
    /// The index tracking dirty keys.
    index: &'a RemovalIndex,
    /// The carrier key being written.
    key: String,
    /// Whether `key` was already dirty before this operation began.
    was_dirty: bool,
    /// Shared flag indicating whether `key` must remain dirty if this guard drops before
    /// [`Self::complete`].
    dirty_on_drop: Arc<AtomicBool>,
    /// Whether the element-side write completed.
    completed: bool,
}

impl<'a> ElementSyncGuard<'a> {
    /// Register an in-flight `put` or `remove` on `key`.
    fn new(index: &'a RemovalIndex, key: &str) -> Self {
        let mut state = index.dirty_guard();
        let was_dirty = state.dirty.contains(key);
        *state.in_flight.entry(key.to_owned()).or_default() += 1;
        Self {
            index,
            key: key.to_owned(),
            was_dirty,
            dirty_on_drop: Arc::new(AtomicBool::new(was_dirty)),
            completed: false,
        }
    }

    /// Mark `key` dirty immediately before mutating `removals`.
    fn mark_removal_write(&mut self) {
        self.index.dirty_guard().dirty.insert(self.key.clone());
        self.dirty_on_drop.store(true, Ordering::Release);
    }

    /// Mark the element-side write complete and clear `dirty` when no other operation on `key` is
    /// active.
    fn complete(mut self) {
        self.completed = true;
        let mut state = self.index.dirty_guard();
        let still_in_flight = match state.in_flight.get_mut(&self.key) {
            Some(count) if *count > 1 => {
                *count -= 1;
                true
            }
            Some(_) => {
                state.in_flight.remove(&self.key);
                false
            }
            None => false,
        };
        if !still_in_flight {
            state.dirty.remove(&self.key);
        }
    }
}

impl Drop for ElementSyncGuard<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut state = self.index.dirty_guard();
        let still_in_flight = match state.in_flight.get_mut(&self.key) {
            Some(count) if *count > 1 => {
                *count -= 1;
                true
            }
            Some(_) => {
                state.in_flight.remove(&self.key);
                false
            }
            None => false,
        };
        if self.dirty_on_drop.load(Ordering::Acquire) {
            state.dirty.insert(self.key.clone());
        } else if !still_in_flight {
            state.dirty.remove(&self.key);
        }
    }
}

/// Guard for an admitted removal-store write whose backend outcome is not yet reconciled.
///
/// Dropping this guard before [`Self::complete`] or [`Self::reconcile`] means the write future
/// itself was dropped or read-back reconciliation failed, so the optimistic accounting delta is
/// reverted and `recovered` is cleared to force a full scan on the next operation.
struct UnfinishedRemovalWrite<'a> {
    /// The shared index state whose accounting was optimistically updated.
    inner: &'a RemovalIndexInner,
    /// The applied delta from `previous` to `next`.
    changes: &'a ItemChanges,
    /// Whether the backend outcome was confirmed or reconciled.
    completed: bool,
}

impl<'a> UnfinishedRemovalWrite<'a> {
    /// Arm a guard for `changes` applied to `inner`.
    const fn new(inner: &'a RemovalIndexInner, changes: &'a ItemChanges) -> Self {
        Self {
            inner,
            changes,
            completed: false,
        }
    }

    /// Disarm after `removals` confirmed `next` was stored.
    fn complete(mut self) {
        self.completed = true;
    }

    /// Reconcile `inner.accounting` against the `actual` record read back from `removals` after a
    /// write error (such as a post-commit directory-sync failure).
    fn reconcile(
        mut self,
        previous: Option<&RemovalRecord>,
        next: Option<&RemovalRecord>,
        actual: Option<&RemovalRecord>,
        now_ms: u128,
    ) {
        self.completed = true;
        if actual == next {
            return;
        }
        let mut accounting = self.inner.accounting.lock();
        self.changes.revert(&mut accounting);
        if actual != previous {
            let actual_changes = ItemChanges::of(previous, actual, now_ms);
            for (principal, at) in actual_changes.left {
                accounting.discharge(principal, at);
            }
            for (principal, at) in actual_changes.raised {
                accounting.charge(principal, at);
            }
        }
    }
}

impl Drop for UnfinishedRemovalWrite<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.changes.revert(&mut self.inner.accounting.lock());
            self.inner.recovered.store(false, Ordering::Release);
        }
    }
}

/// Guard for an in-flight removal-store delete whose backend outcome is not yet reconciled.
struct UnfinishedRemovalDelete<'a> {
    /// The shared index state whose accounting must match `removals`.
    inner: &'a RemovalIndexInner,
    /// Whether the delete outcome was confirmed or reconciled.
    completed: bool,
}

impl<'a> UnfinishedRemovalDelete<'a> {
    /// Arm a guard before calling `removals.remove`.
    const fn new(inner: &'a RemovalIndexInner) -> Self {
        Self {
            inner,
            completed: false,
        }
    }

    /// Disarm after `removals.remove` succeeded and discharge `previous`.
    fn complete(mut self, previous: &RemovalRecord) {
        self.completed = true;
        let mut accounting = self.inner.accounting.lock();
        for (principal, at) in previous.items() {
            accounting.discharge(principal, at);
        }
    }

    /// Reconcile `inner.accounting` against `actual` read back after `removals.remove` errored.
    fn reconcile(mut self, previous: &RemovalRecord, actual: Option<&RemovalRecord>) {
        self.completed = true;
        if actual == Some(previous) {
            return;
        }
        let mut accounting = self.inner.accounting.lock();
        for (principal, at) in previous.items() {
            accounting.discharge(principal, at);
        }
        if let Some(actual) = actual {
            for (principal, at) in actual.items() {
                accounting.charge(principal, at);
            }
        }
    }
}

impl Drop for UnfinishedRemovalDelete<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.inner.recovered.store(false, Ordering::Release);
        }
    }
}

/// Poll `op_fut` inside `writes` so that if the caller is cancelled while `op_fut` is pending,
/// `op_fut` remains stored in `writes` for the next lock holder to await to completion before
/// scanning or mutating `removals`.
async fn run_in_flight_removal_op(
    writes: &mut Option<InFlightRemovalOp>,
    op_fut: InFlightRemovalOp,
) {
    *writes = Some(op_fut);
    poll_fn(|cx| match writes.as_mut() {
        Some(fut) => {
            let poll = fut.as_mut().poll(cx);
            if poll.is_ready() {
                *writes = None;
            }
            poll
        }
        None => Poll::Ready(()),
    })
    .await;
}

/// The store of one carrier kind: a whole store (relay inboxes, the fetch cache), or the data
/// store factored into element and remove sides.
pub(crate) enum CarrierStore<'a> {
    /// A store of whole carriers.
    Whole(&'a EntryStorage),
    /// The factored data store.
    Factored {
        /// The element sides.
        elements: &'a EntryStorage,
        /// The remove sides.
        removals: &'a Arc<RemovalStorage>,
        /// The charge index of `removals`.
        index: &'a RemovalIndex,
    },
}

impl CarrierStore<'_> {
    /// Whether this store enforces per-actor data provenance (`true` for all [`EntryKind::Data`]
    /// stores, including the [`Self::Whole`] local lookup cache, #1220).
    pub(crate) const fn enforces_data_provenance(&self, kind: EntryKind) -> bool {
        matches!(kind, EntryKind::Data)
    }

    /// Return `local` with cross-actor tombstones targeting `incoming` evicted when this store
    /// enforces data provenance, or unchanged otherwise.
    pub(crate) fn sanitize_local_tombstones(
        &self,
        local: Entry,
        incoming: &Entry,
        fallback_actor: Did,
    ) -> Entry {
        match self.enforces_data_provenance(incoming.kind) {
            true => local.without_cross_actor_tombstones_for(incoming, fallback_actor),
            false => local,
        }
    }

    /// Run the recovery pass once: normalize every element side that has a remove side against
    /// it, and count every remove side. Later calls only read the flag, and hold no lock.
    async fn ensure_recovered(
        elements: &EntryStorage,
        removals: &RemovalStorage,
        index: &RemovalIndex,
    ) -> Result<()> {
        if index.inner.recovered.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut writes = index.writes.lock().await;
        if let Some(in_flight) = writes.take() {
            in_flight.await;
        }
        Self::recover_locked(elements, removals, index).await
    }

    /// Rebuild `index.inner.accounting` in a staging table and normalize every element side
    /// against its remove side while holding `index.writes`, publishing the accounting and
    /// `recovered = true` together only after the full scan succeeds.
    async fn recover_locked(
        elements: &EntryStorage,
        removals: &RemovalStorage,
        index: &RemovalIndex,
    ) -> Result<()> {
        if index.inner.recovered.load(Ordering::Acquire) {
            return Ok(());
        }
        let shares = index.inner.accounting.lock().shares();
        let mut rebuilt = SlotAccounting::new(shares);
        for (key, record) in removals.get_all().await? {
            for (principal, at) in record.items() {
                rebuilt.charge(principal, at);
            }
            let Some(held) = elements.get(&key).await? else {
                index.clear_dirty_if_idle(&key);
                continue;
            };
            let normalized = held.join(record.as_entry())?;
            let repaired = element_side(&normalized);
            if repaired != held {
                elements.put(&key, &repaired).await?;
            }
            index.clear_dirty_if_idle(&key);
        }
        *index.inner.accounting.lock() = rebuilt;
        index.inner.recovered.store(true, Ordering::Release);
        Ok(())
    }

    /// Glue `elements_entry` and `removals_record` at `key`, forcing a full normalizing join
    /// whenever `dirty` indicates a cross-store write may have left `elements` older than
    /// `removals`.
    fn resolve_read(
        index: &RemovalIndex,
        key: &str,
        elements_entry: Option<Entry>,
        removals_record: Option<&RemovalRecord>,
        dirty: bool,
        can_clear_dirty: bool,
    ) -> Result<Option<Entry>> {
        if !dirty {
            return glue(elements_entry, removals_record, false);
        }
        let Some(removals_record) = removals_record else {
            if can_clear_dirty {
                index.clear_dirty_if_idle(key);
            }
            return Ok(elements_entry);
        };
        let Some(held) = elements_entry else {
            if can_clear_dirty {
                index.clear_dirty_if_idle(key);
            }
            return Ok(Some(removals_record.as_entry()));
        };
        let normalized = held.join(removals_record.as_entry())?;
        if can_clear_dirty && element_side(&normalized) == held {
            index.clear_dirty_if_idle(key);
        }
        Ok(Some(normalized))
    }

    /// The carrier stored at `key`. Takes no lock once recovered.
    pub(crate) async fn get(&self, key: &str) -> Result<Option<Entry>> {
        match self {
            Self::Whole(store) => store.get(key).await,
            Self::Factored {
                elements,
                removals,
                index,
            } => {
                Self::ensure_recovered(elements, removals, index).await?;
                let (dirty_before, in_flight_before) = index.dirty_state(key);
                let record = removals.get(key).await?;
                let held = elements.get(key).await?;
                let (dirty_after, in_flight_after) = index.dirty_state(key);
                let dirty = dirty_before || dirty_after;
                let can_clear_dirty = dirty && !in_flight_before && !in_flight_after;
                Self::resolve_read(index, key, held, record.as_ref(), dirty, can_clear_dirty)
            }
        }
    }

    /// Store `value`, normalized, at `key` at `now_ms`, charging what it raises to `introducer`,
    /// the principal that delivered it (the write order, shares and lock law of the module
    /// documentation).
    ///
    /// Pre: the caller holds the storage transition, which orders the writes to one key.
    /// Post: on `Err(RemovalCapacityExhausted)` nothing changed.
    pub(crate) async fn put(
        &self,
        key: &str,
        value: &Entry,
        introducer: Principal,
        now_ms: u128,
    ) -> Result<()> {
        let (elements, removals, index) = match self {
            Self::Whole(store) => return store.put(key, value).await,
            Self::Factored {
                elements,
                removals,
                index,
            } => (elements, removals, index),
        };
        Self::ensure_recovered(elements, removals, index).await?;
        let mut previous = removals.get(key).await?;
        let mut next = RemovalRecord::of(value, previous.as_ref(), introducer);
        let mut element_sync = ElementSyncGuard::new(index, key);
        if next != previous {
            let mut writes = index.writes.lock().await;
            if let Some(in_flight) = writes.take() {
                in_flight.await;
                previous = removals.get(key).await?;
                next = RemovalRecord::of(value, previous.as_ref(), introducer);
            }
            if !index.inner.recovered.load(Ordering::Acquire) {
                Self::recover_locked(elements, removals, index).await?;
                previous = removals.get(key).await?;
                next = RemovalRecord::of(value, previous.as_ref(), introducer);
            }
            if next != previous {
                let changes = ItemChanges::of(previous.as_ref(), next.as_ref(), now_ms);
                changes.apply(&mut index.inner.accounting.lock(), now_ms)?;
                let write_guard = UnfinishedRemovalWrite::new(&index.inner, &changes);
                element_sync.mark_removal_write();
                let removals_cloned = Arc::clone(removals);
                let inner = Arc::clone(&index.inner);
                let key_owned = key.to_owned();
                let next_owned = next.clone();
                let outcome = Arc::new(Mutex::new(None::<Result<()>>));
                let outcome_out = Arc::clone(&outcome);
                let op_fut: InFlightRemovalOp = Box::pin(async move {
                    let written = match &next_owned {
                        Some(record) => removals_cloned.put(&key_owned, record).await,
                        None => removals_cloned.remove(&key_owned).await,
                    };
                    if outcome_out
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .replace(written)
                        .is_some()
                    {
                        inner.recovered.store(false, Ordering::Release);
                    }
                });
                run_in_flight_removal_op(&mut writes, op_fut).await;
                let written = outcome
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
                    .unwrap_or(Ok(()));
                if let Err(error) = written {
                    if let Ok(actual) = removals.get(key).await {
                        write_guard.reconcile(
                            previous.as_ref(),
                            next.as_ref(),
                            actual.as_ref(),
                            now_ms,
                        );
                        if actual == previous {
                            element_sync
                                .dirty_on_drop
                                .store(element_sync.was_dirty, Ordering::Release);
                        }
                    }
                    return Err(error);
                }
                write_guard.complete();
            }
        }
        elements.put(key, &element_side(value)).await?;
        element_sync.complete();
        Ok(())
    }

    /// Remove the carrier stored at `key`: its element side, then its remove side.
    ///
    /// Pre: the caller holds the storage transition.
    pub(crate) async fn remove(&self, key: &str) -> Result<()> {
        match self {
            Self::Whole(store) => store.remove(key).await,
            Self::Factored {
                elements,
                removals,
                index,
            } => {
                Self::ensure_recovered(elements, removals, index).await?;
                let element_sync = ElementSyncGuard::new(index, key);
                elements.remove(key).await?;
                element_sync.complete();
                if let Some(mut record) = removals.get(key).await? {
                    let mut writes = index.writes.lock().await;
                    if let Some(in_flight) = writes.take() {
                        in_flight.await;
                        let Some(refreshed) = removals.get(key).await? else {
                            return Ok(());
                        };
                        record = refreshed;
                    }
                    if !index.inner.recovered.load(Ordering::Acquire) {
                        Self::recover_locked(elements, removals, index).await?;
                        let Some(refreshed) = removals.get(key).await? else {
                            return Ok(());
                        };
                        record = refreshed;
                    }
                    let delete_guard = UnfinishedRemovalDelete::new(&index.inner);
                    let removals_cloned = Arc::clone(removals);
                    let inner = Arc::clone(&index.inner);
                    let key_owned = key.to_owned();
                    let outcome = Arc::new(Mutex::new(None::<Result<()>>));
                    let outcome_out = Arc::clone(&outcome);
                    let op_fut: InFlightRemovalOp = Box::pin(async move {
                        let removed = removals_cloned.remove(&key_owned).await;
                        if outcome_out
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .replace(removed)
                            .is_some()
                        {
                            inner.recovered.store(false, Ordering::Release);
                        }
                    });
                    run_in_flight_removal_op(&mut writes, op_fut).await;
                    let removed = outcome
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take()
                        .unwrap_or(Ok(()));
                    if let Err(error) = removed {
                        if let Ok(actual) = removals.get(key).await {
                            delete_guard.reconcile(&record, actual.as_ref());
                        }
                        return Err(error);
                    }
                    delete_guard.complete(&record);
                }
                Ok(())
            }
        }
    }

    /// Every carrier of this store with its key: each element side glued to its remove side, and
    /// every remove side whose element side was evicted on its own.
    pub(crate) async fn get_all(&self) -> Result<Vec<(String, Entry)>> {
        let (elements, removals, index) = match self {
            Self::Whole(store) => return store.get_all().await,
            Self::Factored {
                elements,
                removals,
                index,
            } => (elements, removals, index),
        };
        Self::ensure_recovered(elements, removals, index).await?;
        let (dirty_before, in_flight_before) = index.dirty_snapshot();
        let mut records = removals
            .get_all()
            .await?
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let held_entries = elements.get_all().await?;
        let (dirty_after, in_flight_after) = index.dirty_snapshot();
        let mut carriers = Vec::new();
        for (key, held) in held_entries {
            let record = records.remove(&key);
            let dirty = dirty_before.contains(&key) || dirty_after.contains(&key);
            let can_clear_dirty =
                dirty && !in_flight_before.contains(&key) && !in_flight_after.contains(&key);
            if let Some(carrier) = Self::resolve_read(
                index,
                &key,
                Some(held),
                record.as_ref(),
                dirty,
                can_clear_dirty,
            )? {
                carriers.push((key, carrier));
            }
        }
        for (key, record) in records {
            if (dirty_before.contains(&key) || dirty_after.contains(&key))
                && !in_flight_before.contains(&key)
                && !in_flight_after.contains(&key)
            {
                index.clear_dirty_if_idle(&key);
            }
            carriers.push((key, record.as_entry()));
        }
        Ok(carriers)
    }
}
