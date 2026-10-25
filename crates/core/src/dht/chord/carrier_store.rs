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
//!   remove side, after it, so it holds no add that remove side covers or floors, and a read
//!   glues the two without hashing. Only a crash that rolls a cached element side back can break
//!   that; the first operation on the store after it is opened normalizes every element side that
//!   has a remove side (the recovery pass), and a whole carrier written before the factoring,
//!   whose element side still holds removes, is joined in full.
//! - Shares (load-bounds paper, Sec. V, re-keying Law 6 of the removal-stability paper): each
//!   unstable remove, and the register, is an item charged to the principal that delivered the
//!   transition raising it: the connection slot of the edge an operation or a sync arrived on, or
//!   this node for its own writes and repairs. The items are accounted under slot accounting with
//!   [`REMOVAL_SHARES`], so the store holds at most `R(s)·128 + 16 384 + 7 168` unstable items,
//!   [`REMOVAL_STORE_MAX_REMOVES`] at the default `s = 3`. A transition that raises an item the
//!   accounting refuses is refused whole; one that raises none is never refused for room. An
//!   honest remove is therefore refused only by its topic's cap, or by its slot's floor and fair
//!   claim, whatever other parties hold (Theorem 1); a slot's removes outlive its edge until they
//!   are stable (Prop. 1).
//! - Locks: a read takes no lock once the store is recovered, so it never waits on a write's
//!   I/O. The write lock is taken only by the recovery pass, once, and by a write whose remove
//!   side changed, across its admission and its remove-side write; a write that raises and drops
//!   nothing (an `Extend`, a heartbeat) never takes it. The accounting is locked for one pure
//!   step at a time beneath it. Writes to one key are ordered by the storage transition their
//!   callers hold, which is what keeps `R` before `E` per key.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

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
use crate::fair_admission::TableAccounting;
use crate::storage::KvStorageInterface;

/// The store of remove sides (#920): a `RecordAuthority::Retained` store in production, which
/// refuses and never evicts.
pub type RemovalStorage =
    Box<rings_runtime::maybe_send_sync!(dyn KvStorageInterface<RemovalRecord>)>;

/// The shares of the removal store (load-bounds paper, Sec. V): `f = 128` items per slot, a
/// pool of 16 384 and a local share of 7 168, so `B(s) = R(s)·128 + 23 552`, 65 536 at `s = 3`
/// and 256 more per successor.
pub(crate) const REMOVAL_SHARES: Shares = Shares {
    floor: 128,
    pool: 16_384,
    local: 7_168,
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
        crdt: EntryCrdt {
            register: None,
            dots: value.crdt.dots.clone(),
            tombstones: Vec::new(),
        },
        ..value.clone()
    }
}

/// `E ⊔ R` by the glue law: concatenation when the element side holds no remove side of its own,
/// and the full join for a whole carrier written before the factoring.
fn glue(elements: Option<Entry>, removals: Option<&RemovalRecord>) -> Result<Option<Entry>> {
    let Some(removals) = removals else {
        return Ok(elements);
    };
    let Some(elements) = elements else {
        return Ok(Some(removals.as_entry()));
    };
    if !elements.crdt.tombstones.is_empty() || elements.crdt.register.is_some() {
        return elements.join(removals.as_entry()).map(Some);
    }
    let remove_side = removals.as_entry();
    Ok(Some(Entry {
        crdt: EntryCrdt {
            register: remove_side.crdt.register,
            dots: elements.crdt.dots,
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
}

impl RemovalIndex {
    /// An index not yet recovered, accounting in `accounting`.
    pub(crate) fn new(accounting: TableAccounting) -> Self {
        Self {
            recovered: AtomicBool::new(false),
            writes: FuturesMutex::new(()),
            accounting,
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
        if index.recovered.load(Ordering::Acquire) {
            return Ok(());
        }
        for (key, record) in removals.get_all().await? {
            {
                let mut accounting = index.accounting.lock();
                for (principal, at) in record.items() {
                    accounting.charge(principal, at);
                }
            }
            let Some(held) = elements.get(&key).await? else {
                continue;
            };
            let whole = !held.crdt.tombstones.is_empty() || held.crdt.register.is_some();
            let normalized = held.join(record.as_entry())?;
            if whole || normalized.data.len() != held.data.len() {
                elements.put(&key, &element_side(&normalized)).await?;
            }
        }
        index.recovered.store(true, Ordering::Release);
        Ok(())
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
                let record = removals.get(key).await?;
                glue(elements.get(key).await?, record.as_ref())
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
        let previous = removals.get(key).await?;
        let next = RemovalRecord::of(value, previous.as_ref(), introducer);
        if next != previous {
            let _writes = index.writes.lock().await;
            let changes = ItemChanges::of(previous.as_ref(), next.as_ref(), now_ms);
            changes.apply(&mut index.accounting.lock(), now_ms)?;
            let written = match &next {
                Some(record) => removals.put(key, record).await,
                None => removals.remove(key).await,
            };
            if written.is_err() {
                // The store still holds `previous`, or a value the next start's recovery pass
                // counts afresh.
                changes.revert(&mut index.accounting.lock());
            }
            written?;
        }
        elements.put(key, &element_side(value)).await
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
                if let Some(record) = removals.get(key).await? {
                    let _writes = index.writes.lock().await;
                    removals.remove(key).await?;
                    let mut accounting = index.accounting.lock();
                    for (principal, at) in record.items() {
                        accounting.discharge(principal, at);
                    }
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
        let mut records = removals
            .get_all()
            .await?
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let mut carriers = Vec::new();
        for (key, held) in elements.get_all().await? {
            let record = records.remove(&key);
            if let Some(carrier) = glue(Some(held), record.as_ref())? {
                carriers.push((key, carrier));
            }
        }
        for (key, record) in records {
            carriers.push((key, record.as_entry()));
        }
        Ok(carriers)
    }
}
