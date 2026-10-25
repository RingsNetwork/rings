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
//! - Shares (Law 6): each unstable remove is charged to the authenticated sender of the
//!   transition that raised it, or to the holder class for a node this owner's view names as an
//!   owner. A transition that raises a remove is refused when its sender's charges would pass
//!   [`REMOVAL_ORIGIN_SHARE`] or the store's would pass [`REMOVAL_STORE_MAX_REMOVES`]; a
//!   transition that raises none is never refused for room. An honest remove is therefore refused
//!   only by its topic's cap, its writer's own share, or a total that takes 63 other identities
//!   or a Byzantine owner (Prop. 6).
//! - Locks: a read takes no lock once the store is recovered, so it never waits on a write's
//!   I/O. The charge index is locked only by the recovery pass, once, and by a write whose
//!   remove side changed, across its admission and its remove-side write; a write that raises
//!   and drops nothing (an `Extend`, a heartbeat) never takes it. Writes to one key are ordered
//!   by the storage transition their callers hold, which is what keeps `R` before `E` per key.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use futures::lock::Mutex as FuturesMutex;
use serde::Deserialize;
use serde::Serialize;

use super::EntryStorage;
use crate::consts::REMOVAL_ORIGIN_SHARE;
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
use crate::storage::KvStorageInterface;

/// The store of remove sides (#920): a `RecordAuthority::Retained` store in production, which
/// refuses and never evicts.
pub type RemovalStorage =
    Box<rings_runtime::maybe_send_sync!(dyn KvStorageInterface<RemovalRecord>)>;

/// Whom an owner charges a remove it holds (removal-stability paper, Def. 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RemovalCharge {
    /// The authenticated sender of the transition that raised it: an operation's writer, or the
    /// signer of a sync message or lookup reply.
    Origin(Did),
    /// A node this owner's view names as an owner of the placement (itself, its predecessor, or
    /// a successor), which no share bounds: a Byzantine owner is a non-goal.
    Holder,
}

/// One remove of a remove side, with its charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChargedRemove {
    /// The remove.
    pub tombstone: EntryTombstone,
    /// Whom it is charged to.
    pub charge: RemovalCharge,
}

/// The register of a remove side, with its charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChargedRegister {
    /// The register.
    pub version: EntryVersion,
    /// Whom it is charged to.
    pub charge: RemovalCharge,
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
    /// remove and no register. The second component says whether anything was raised.
    fn of(value: &Entry, previous: Option<&Self>, charge: RemovalCharge) -> (Option<Self>, bool) {
        let mut raised = false;
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
                charge: held(tombstone).unwrap_or_else(|| {
                    raised = true;
                    charge
                }),
            })
            .collect::<Vec<_>>();
        let register = value.crdt.register.map(|version| ChargedRegister {
            version,
            charge: previous
                .and_then(|previous| previous.register)
                .filter(|held| held.version == version)
                .map(|held| held.charge)
                .unwrap_or_else(|| {
                    raised = true;
                    charge
                }),
        });
        if removes.is_empty() && register.is_none() {
            return (None, false);
        }
        let record = Self {
            did: value.did,
            register,
            removes,
        };
        (Some(record), raised)
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

    /// Every charge of this record with the instant its item is stable.
    fn charges(&self) -> impl Iterator<Item = (RemovalCharge, u128)> + '_ {
        self.removes
            .iter()
            .map(|remove| (remove.charge, stable_at(&remove.tombstone.dot.version)))
            .chain(
                self.register
                    .iter()
                    .map(|register| (register.charge, stable_at(&register.version))),
            )
    }

    /// The items of this record still unstable at `now_ms`, charged to `charge` (any charge when
    /// `None`).
    fn unstable(&self, charge: Option<RemovalCharge>, now_ms: u128) -> usize {
        self.charges()
            .filter(|(held, at)| charge.is_none_or(|charge| *held == charge) && *at > now_ms)
            .count()
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

/// The unstable items of the removal store per charge, by the instant each is stable.
///
/// Law: `held(c, t)` counts the items charged to `c` whose stable instant is after `t`; an item
/// that became stable is forgotten, whether or not its record has been rewritten yet.
#[derive(Debug, Default)]
struct ChargeIndex {
    by_charge: BTreeMap<RemovalCharge, BTreeMap<u128, usize>>,
}

impl ChargeIndex {
    /// Count `record`'s items.
    fn add(&mut self, record: &RemovalRecord) {
        for (charge, at) in record.charges() {
            *self
                .by_charge
                .entry(charge)
                .or_default()
                .entry(at)
                .or_default() += 1;
        }
    }

    /// Forget `record`'s items.
    fn subtract(&mut self, record: &RemovalRecord) {
        for (charge, at) in record.charges() {
            let Some(instants) = self.by_charge.get_mut(&charge) else {
                continue;
            };
            if let Some(count) = instants.get_mut(&at) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    instants.remove(&at);
                }
            }
        }
        self.by_charge.retain(|_, instants| !instants.is_empty());
    }

    /// The unstable items at `now_ms` charged to `charge`, or to anyone when `None`.
    fn held(&self, charge: Option<RemovalCharge>, now_ms: u128) -> usize {
        self.by_charge
            .iter()
            .filter(|(held, _)| charge.is_none_or(|charge| **held == charge))
            .flat_map(|(_, instants)| instants.range(now_ms.saturating_add(1)..))
            .map(|(_, count)| *count)
            .sum()
    }

    /// Refuse the transition `previous ↦ next` that raised items charged to `charge` if it passes
    /// the sender's share or the store's total (Law 6).
    fn admit(
        &self,
        previous: Option<&RemovalRecord>,
        next: Option<&RemovalRecord>,
        charge: RemovalCharge,
        now_ms: u128,
    ) -> Result<()> {
        let after = |of: Option<RemovalCharge>| {
            let before = previous.map_or(0, |record| record.unstable(of, now_ms));
            let after = next.map_or(0, |record| record.unstable(of, now_ms));
            self.held(of, now_ms)
                .saturating_sub(before)
                .saturating_add(after)
        };
        if let RemovalCharge::Origin(origin) = charge {
            let held = after(Some(charge));
            if held > REMOVAL_ORIGIN_SHARE {
                return Err(Error::RemovalCapacityExhausted {
                    origin: Some(origin),
                    held,
                    bound: REMOVAL_ORIGIN_SHARE,
                });
            }
        }
        let held = after(None);
        if held > REMOVAL_STORE_MAX_REMOVES {
            return Err(Error::RemovalCapacityExhausted {
                origin: None,
                held,
                bound: REMOVAL_STORE_MAX_REMOVES,
            });
        }
        Ok(())
    }
}

/// The charge index of a removal store, built by the recovery pass on first use (the lock law
/// of the module documentation).
#[derive(Debug, Default)]
pub(crate) struct RemovalIndex {
    /// Whether the recovery pass has run: read without the lock once set.
    recovered: AtomicBool,
    /// The charges; locked by the recovery pass and by a write that changes a remove side.
    charges: FuturesMutex<ChargeIndex>,
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
        let mut charges = index.charges.lock().await;
        if index.recovered.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut counted = ChargeIndex::default();
        for (key, record) in removals.get_all().await? {
            counted.add(&record);
            let Some(held) = elements.get(&key).await? else {
                continue;
            };
            let whole = !held.crdt.tombstones.is_empty() || held.crdt.register.is_some();
            let normalized = held.join(record.as_entry())?;
            if whole || normalized.data.len() != held.data.len() {
                elements.put(&key, &element_side(&normalized)).await?;
            }
        }
        *charges = counted;
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

    /// Store `value`, normalized, at `key` at `now_ms`, charging what it raises to `charge`
    /// (the write order, shares and lock law of the module documentation).
    ///
    /// Pre: the caller holds the storage transition, which orders the writes to one key.
    /// Post: on `Err(RemovalCapacityExhausted)` nothing changed.
    pub(crate) async fn put(
        &self,
        key: &str,
        value: &Entry,
        charge: RemovalCharge,
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
        let (next, raised) = RemovalRecord::of(value, previous.as_ref(), charge);
        if next != previous {
            let mut charges = index.charges.lock().await;
            if raised {
                charges.admit(previous.as_ref(), next.as_ref(), charge, now_ms)?;
            }
            match &next {
                Some(record) => removals.put(key, record).await?,
                None => removals.remove(key).await?,
            }
            if let Some(record) = &previous {
                charges.subtract(record);
            }
            if let Some(record) = &next {
                charges.add(record);
            }
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
                    let mut charges = index.charges.lock().await;
                    removals.remove(key).await?;
                    charges.subtract(&record);
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
