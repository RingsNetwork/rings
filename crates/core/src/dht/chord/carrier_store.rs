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
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use futures::lock::Mutex as FuturesMutex;
use serde::Deserialize;
use serde::Serialize;

use super::EntryStorage;
#[cfg(doc)]
use crate::consts::REMOVAL_STORE_MAX_REMOVES;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryCrdt;
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
        let removes = value
            .crdt
            .tombstones
            .iter()
            .map(|tombstone| ChargedRemove {
                tombstone: *tombstone,
                charge: held(tombstone).unwrap_or(charge),
            })
            .collect::<Vec<_>>();
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
        Entry {
            did: self.did,
            data: Vec::new(),
            kind: EntryKind::Data,
            crdt: EntryCrdt {
                register: self.register.map(|register| register.version),
                dots: Vec::new(),
                co_dots: Vec::new(),
                tombstones: self.removes.iter().map(|remove| remove.tombstone).collect(),
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
    if dirty || !elements.crdt.tombstones.is_empty() || elements.crdt.register.is_some() {
        return elements.join(removals.as_entry()).map(Some);
    }
    let remove_side = removals.as_entry();
    Ok(Some(Entry {
        crdt: EntryCrdt {
            register: remove_side.crdt.register,
            dots: elements.crdt.dots,
            co_dots: elements.crdt.co_dots,
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

/// Tracks keys whose remove side may be newer than their element side because a two-stage write
/// touched `removals` and its matching `elements` write has not yet finished.
#[derive(Debug, Default)]
struct DirtyElements {
    /// Keys whose stored element side is not known to match the stored remove side's generation.
    dirty: BTreeSet<String>,
    /// Active `CarrierStore::put` calls per key.
    in_flight: BTreeMap<String, usize>,
}

/// The accounting of a removal store, filled by the recovery pass on first use (the lock law of
/// the module documentation).
#[derive(Debug)]
pub(crate) struct RemovalIndex {
    /// Whether the recovery pass has run: read without the lock once set.
    recovered: AtomicBool,
    /// The write lock: taken by the recovery pass and by a write that changes a remove side.
    writes: FuturesMutex<()>,
    /// The items, under edge accounting with [`REMOVAL_SHARES`].
    accounting: TableAccounting,
    /// Keys with an in-flight or incomplete cross-store (`R` then `E`) write.
    dirty_elements: Mutex<DirtyElements>,
}

impl RemovalIndex {
    /// The floor use of every slot in the removal store at `now_ms` (load-bounds paper, Def. 1).
    pub(crate) fn floor_use_at(&self, now_ms: u128) -> SlotFloorUse {
        self.accounting.floor_use_at(now_ms)
    }

    /// The number of items held by `principal`.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn held(&self, principal: Principal) -> usize {
        self.accounting.lock().held(principal)
    }

    /// An index not yet recovered, accounting in `accounting`.
    pub(crate) fn new(accounting: TableAccounting) -> Self {
        Self {
            recovered: AtomicBool::new(false),
            writes: FuturesMutex::new(()),
            accounting,
            dirty_elements: Mutex::new(DirtyElements::default()),
        }
    }

    /// Lock `dirty_elements` for one pure step.
    fn dirty_guard(&self) -> MutexGuard<'_, DirtyElements> {
        self.dirty_elements
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Return `(dirty, in_flight)` for `key`.
    fn dirty_state(&self, key: &str) -> (bool, bool) {
        let state = self.dirty_guard();
        (state.dirty.contains(key), state.in_flight.contains_key(key))
    }

    /// Whether an active `put` is currently in flight on `key`.
    fn is_in_flight(&self, key: &str) -> bool {
        self.dirty_guard().in_flight.contains_key(key)
    }

    /// Snapshot `(dirty_keys, in_flight_keys)` across all keys.
    fn dirty_snapshot(&self) -> (BTreeSet<String>, BTreeSet<String>) {
        let state = self.dirty_guard();
        (
            state.dirty.clone(),
            state.in_flight.keys().cloned().collect(),
        )
    }

    /// Clear `key` from `dirty` if no `put` is currently in flight on `key`.
    fn clear_dirty_if_idle(&self, key: &str) {
        let mut state = self.dirty_guard();
        if !state.in_flight.contains_key(key) {
            state.dirty.remove(key);
        }
    }
}

/// Tracks a `CarrierStore::put` across its remove-side and element-side writes so a failure or
/// cancellation after `removals` changes leaves `key` marked dirty until normalized.
struct ElementSyncGuard<'a> {
    /// The index tracking dirty keys.
    index: &'a RemovalIndex,
    /// The carrier key being written.
    key: String,
    /// Whether `key` was already dirty before this `put` began.
    was_dirty: bool,
    /// Whether `key` must remain dirty if this guard drops before [`Self::complete`].
    dirty_on_drop: bool,
    /// Whether the element-side write completed.
    completed: bool,
}

impl<'a> ElementSyncGuard<'a> {
    /// Register an in-flight `put` on `key`.
    fn new(index: &'a RemovalIndex, key: &str) -> Self {
        let mut state = index.dirty_guard();
        let was_dirty = state.dirty.contains(key);
        *state.in_flight.entry(key.to_owned()).or_default() += 1;
        Self {
            index,
            key: key.to_owned(),
            was_dirty,
            dirty_on_drop: was_dirty,
            completed: false,
        }
    }

    /// Mark `key` dirty immediately before mutating `removals`.
    fn mark_removal_write(&mut self) {
        self.index.dirty_guard().dirty.insert(self.key.clone());
        self.dirty_on_drop = true;
    }

    /// Restore the prior dirty state when a failed removal-store write left `removals` unchanged.
    fn restore_if_unchanged(&mut self) {
        self.dirty_on_drop = self.was_dirty;
    }

    /// Mark the element-side write complete and clear `dirty` when no other `put` on `key` is
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
        if self.dirty_on_drop {
            state.dirty.insert(self.key.clone());
        } else if !still_in_flight {
            state.dirty.remove(&self.key);
        }
    }
}

/// Guard for an admitted removal-store write whose backend outcome is not yet reconciled.
///
/// Dropping this guard before [`Self::complete`] or [`Self::reconcile`] means the caller was
/// cancelled mid-write (or read-back reconciliation itself failed), so the optimistic accounting
/// delta is reverted and `recovered` is cleared to force a full scan on the next operation.
struct UnfinishedRemovalWrite<'a> {
    /// The index whose accounting was optimistically updated.
    index: &'a RemovalIndex,
    /// The applied delta from `previous` to `next`.
    changes: &'a ItemChanges,
    /// Whether the backend outcome was confirmed or reconciled.
    completed: bool,
}

impl<'a> UnfinishedRemovalWrite<'a> {
    /// Arm a guard for `changes` applied to `index`.
    const fn new(index: &'a RemovalIndex, changes: &'a ItemChanges) -> Self {
        Self {
            index,
            changes,
            completed: false,
        }
    }

    /// Disarm after `removals` confirmed `next` was stored.
    fn complete(mut self) {
        self.completed = true;
    }

    /// Reconcile `index.accounting` against the `actual` record read back from `removals` after a
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
        let mut accounting = self.index.accounting.lock();
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
            self.changes.revert(&mut self.index.accounting.lock());
            self.index.recovered.store(false, Ordering::Release);
        }
    }
}

/// Guard for an in-flight removal-store delete whose backend outcome is not yet reconciled.
struct UnfinishedRemovalDelete<'a> {
    /// The index whose accounting must match `removals`.
    index: &'a RemovalIndex,
    /// Whether the delete outcome was confirmed or reconciled.
    completed: bool,
}

impl<'a> UnfinishedRemovalDelete<'a> {
    /// Arm a guard before calling `removals.remove`.
    const fn new(index: &'a RemovalIndex) -> Self {
        Self {
            index,
            completed: false,
        }
    }

    /// Disarm after `removals.remove` succeeded and discharge `previous`.
    fn complete(mut self, previous: &RemovalRecord) {
        self.completed = true;
        let mut accounting = self.index.accounting.lock();
        for (principal, at) in previous.items() {
            accounting.discharge(principal, at);
        }
    }

    /// Reconcile `index.accounting` against `actual` read back after `removals.remove` errored.
    fn reconcile(mut self, previous: &RemovalRecord, actual: Option<&RemovalRecord>) {
        self.completed = true;
        if actual == Some(previous) {
            return;
        }
        let mut accounting = self.index.accounting.lock();
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
            self.index.recovered.store(false, Ordering::Release);
        }
    }
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
        removals: &'a RemovalStorage,
        /// The charge index of `removals`.
        index: &'a RemovalIndex,
    },
}

impl CarrierStore<'_> {
    /// Whether this store is the factored replicated data store, where per-actor provenance is
    /// enforced (the fetch cache uses [`Self::Whole`]).
    pub(crate) const fn enforces_data_provenance(&self, kind: EntryKind) -> bool {
        matches!(self, Self::Factored { .. }) && matches!(kind, EntryKind::Data)
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
        if index.recovered.load(Ordering::Acquire) {
            return Ok(());
        }
        let _writes = index.writes.lock().await;
        Self::recover_locked(elements, removals, index).await
    }

    /// Rebuild `index.accounting` in a staging table and normalize every element side against its
    /// remove side while holding `index.writes`, publishing the accounting and `recovered = true`
    /// together only after the full scan succeeds.
    async fn recover_locked(
        elements: &EntryStorage,
        removals: &RemovalStorage,
        index: &RemovalIndex,
    ) -> Result<()> {
        if index.recovered.load(Ordering::Acquire) {
            return Ok(());
        }
        let shares = index.accounting.lock().shares();
        let mut rebuilt = SlotAccounting::new(shares);
        for (key, record) in removals.get_all().await? {
            for (principal, at) in record.items() {
                rebuilt.charge(principal, at);
            }
            let Some(held) = elements.get(&key).await? else {
                index.clear_dirty_if_idle(&key);
                continue;
            };
            let whole = !held.crdt.tombstones.is_empty() || held.crdt.register.is_some();
            let normalized = held.join(record.as_entry())?;
            if whole
                || normalized.data.len() != held.data.len()
                || normalized.crdt.co_dots.len() != held.crdt.co_dots.len()
            {
                elements.put(&key, &element_side(&normalized)).await?;
            }
            index.clear_dirty_if_idle(&key);
        }
        *index.accounting.lock() = rebuilt;
        index.recovered.store(true, Ordering::Release);
        Ok(())
    }

    /// Glue `elements_entry` and `removals_record` at `key`, forcing a full normalizing join (and
    /// best-effort element-side repair when idle) whenever `dirty` indicates a cross-store write
    /// may have left `elements` older than `removals`.
    async fn resolve_read(
        elements: &EntryStorage,
        index: &RemovalIndex,
        key: &str,
        elements_entry: Option<Entry>,
        removals_record: Option<&RemovalRecord>,
        dirty: bool,
        can_repair: bool,
    ) -> Result<Option<Entry>> {
        if !dirty {
            return glue(elements_entry, removals_record, false);
        }
        let Some(removals_record) = removals_record else {
            if can_repair {
                index.clear_dirty_if_idle(key);
            }
            return Ok(elements_entry);
        };
        let Some(held) = elements_entry else {
            if can_repair {
                index.clear_dirty_if_idle(key);
            }
            return Ok(Some(removals_record.as_entry()));
        };
        let whole = !held.crdt.tombstones.is_empty() || held.crdt.register.is_some();
        let held_data_len = held.data.len();
        let held_co_dots_len = held.crdt.co_dots.len();
        let normalized = held.join(removals_record.as_entry())?;
        if can_repair {
            let needs_write = whole
                || normalized.data.len() != held_data_len
                || normalized.crdt.co_dots.len() != held_co_dots_len;
            if !needs_write {
                index.clear_dirty_if_idle(key);
            } else if let Some(_writes) = index.writes.try_lock() {
                if !index.is_in_flight(key)
                    && elements.put(key, &element_side(&normalized)).await.is_ok()
                {
                    index.clear_dirty_if_idle(key);
                }
            }
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
                let can_repair = dirty && !in_flight_before && !in_flight_after;
                Self::resolve_read(
                    elements,
                    index,
                    key,
                    held,
                    record.as_ref(),
                    dirty,
                    can_repair,
                )
                .await
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
            let _writes = index.writes.lock().await;
            if !index.recovered.load(Ordering::Acquire) {
                Self::recover_locked(elements, removals, index).await?;
                previous = removals.get(key).await?;
                next = RemovalRecord::of(value, previous.as_ref(), introducer);
            }
            if next != previous {
                let changes = ItemChanges::of(previous.as_ref(), next.as_ref(), now_ms);
                changes.apply(&mut index.accounting.lock(), now_ms)?;
                element_sync.mark_removal_write();
                let write_guard = UnfinishedRemovalWrite::new(index, &changes);
                let written = match &next {
                    Some(record) => removals.put(key, record).await,
                    None => removals.remove(key).await,
                };
                if let Err(error) = written {
                    if let Ok(actual) = removals.get(key).await {
                        write_guard.reconcile(
                            previous.as_ref(),
                            next.as_ref(),
                            actual.as_ref(),
                            now_ms,
                        );
                        if actual == previous {
                            element_sync.restore_if_unchanged();
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
                elements.remove(key).await?;
                index.clear_dirty_if_idle(key);
                if let Some(mut record) = removals.get(key).await? {
                    let _writes = index.writes.lock().await;
                    if !index.recovered.load(Ordering::Acquire) {
                        Self::recover_locked(elements, removals, index).await?;
                        let Some(refreshed) = removals.get(key).await? else {
                            return Ok(());
                        };
                        record = refreshed;
                    }
                    let delete_guard = UnfinishedRemovalDelete::new(index);
                    if let Err(error) = removals.remove(key).await {
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
            let can_repair =
                dirty && !in_flight_before.contains(&key) && !in_flight_after.contains(&key);
            if let Some(carrier) = Self::resolve_read(
                elements,
                index,
                &key,
                Some(held),
                record.as_ref(),
                dirty,
                can_repair,
            )
            .await?
            {
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
