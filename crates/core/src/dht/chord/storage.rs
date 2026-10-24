use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;

use super::PeerRing;
use super::PeerRingAction;
use super::RemoteAction;
use crate::dht::entry::inbox::inbox_destination;
use crate::dht::entry::inbox::inbox_key;
use crate::dht::entry::AdmittedEntry;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryLookupEvidence;
use crate::dht::entry::EntryLookupKey;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::entry::PlacementMiss;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::relay_holder::RelayHolder;
use crate::dht::topology;
use crate::dht::types::ChordStorageCache;
use crate::dht::Did;
use crate::dht::EntryStorage;
use crate::error::Error;
use crate::error::Result;
use crate::utils::get_epoch_ms;

/// A lookup placement settled here.
#[derive(Debug)]
pub(crate) enum LookupSettlement {
    /// A live value is stored here.
    Found(Entry),
    /// This node owns the placement and stores nothing for it.
    Missed(PlacementMiss),
}

/// The step one placement of a lookup takes (see [`PeerRing::lookup_placement`]).
#[derive(Debug)]
pub(crate) enum PlacementLookup {
    /// The placement settles here.
    Settled(LookupSettlement),
    /// Ask `next` for `query`.
    Remote {
        /// The node the query goes to.
        next: Did,
        /// The placement the query interrogates.
        query: EntryLookupKey,
    },
}

/// Where one placement of an operation is applied (see [`PeerRing::operate_route`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperateRoute {
    /// This node accepts the placement.
    Owned,
    /// The operation is routed through `next`.
    Remote(Did),
}

/// The identity of a stored carrier: its kind and its placement.
///
/// Storage is partitioned by kind, so the two carriers one position can name (a data topic,
/// which any node may place at any position through an overwrite, and the relay inbox of the
/// peer just before that position) never contend for one slot: a topic parked at `d + 1` cannot
/// shadow the inbox kept for `d`. The data namespace keeps the historical rendering of a bare
/// placement, so values written by earlier builds stay addressable.
///
/// Law: `StorageKey::from_str(&key.to_string()) == Ok(key)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StorageKey {
    kind: EntryKind,
    placement: Did,
}

impl StorageKey {
    /// The rendering prefix of the relay-inbox namespace; the data namespace has none.
    const RELAY_INBOX_PREFIX: &'static str = "relay:";

    /// The slot of a carrier of `kind` placed at `placement`.
    pub(crate) const fn new(kind: EntryKind, placement: Did) -> Self {
        Self { kind, placement }
    }

    /// The slot of the relay inbox kept for `destination`.
    pub(crate) fn inbox_of(destination: Did) -> Self {
        Self::new(EntryKind::RelayMessage, inbox_key(destination))
    }

    /// The placement of the carrier.
    pub(crate) const fn placement(self) -> Did {
        self.placement
    }

    /// The kind of the carrier, which names the store it lives in.
    pub(crate) const fn kind(self) -> EntryKind {
        self.kind
    }
}

impl fmt::Display for StorageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            EntryKind::Data => write!(f, "{}", self.placement),
            EntryKind::RelayMessage => write!(f, "{}{}", Self::RELAY_INBOX_PREFIX, self.placement),
        }
    }
}

impl FromStr for StorageKey {
    type Err = Error;

    fn from_str(key: &str) -> Result<Self> {
        let (kind, placement) = match key.strip_prefix(Self::RELAY_INBOX_PREFIX) {
            Some(placement) => (EntryKind::RelayMessage, placement),
            None => (EntryKind::Data, key),
        };
        Ok(Self::new(kind, Did::from_str(placement)?))
    }
}

/// What a read does with a projection that retired something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Projection {
    /// Write it back, so retired payload bytes stop occupying the store: the store is the
    /// replicated storage, whose reads run under the storage transition, and the reader writes
    /// nothing of its own.
    WrittenBack,
    /// Return it only, writing nothing, not even the retirement of a value no longer live: the
    /// reader writes the slot itself under the transition (a join or an operation), whose write
    /// subsumes the write-back and the retirement, or the store is the fetch cache, whose reads
    /// take no transition (only its read-join puts and its writer's observations take the cache
    /// transition), so a read's write or removal could undo a concurrent put. A dead cached value
    /// stays until a put replaces it or the cache's count bound evicts it, and is reported absent
    /// meanwhile.
    ReturnedOnly,
}

impl Projection {
    /// Whether the read writes its projection back to the store.
    fn writes_back(self) -> bool {
        self == Self::WrittenBack
    }
}

/// Read `key` from `store`, retiring a value that is no longer live.
///
/// Pre: the caller holds the ring's storage transition when `projection` is
/// [`Projection::WrittenBack`], since a retirement is a write.
/// Post: `Ok(Some(entry))` implies `entry.is_live_at(now_ms)` and `entry` is projected by
/// [`Entry::retired_at`]`(now_ms)`. A stored value whose retention bound has elapsed (or that
/// predates retention bounds) and holds no unstable remove or register is reported absent on
/// every read path (storage, sync hand-off, lookups, the fetch cache), and removed by a
/// [`Projection::WrittenBack`] read, so expiry, like the element horizon, is enforced lazily
/// instead of by a sweeper.
async fn live_entry(
    store: &EntryStorage,
    key: &str,
    now_ms: u128,
    projection: Projection,
) -> Result<Option<Entry>> {
    match store.get(key).await? {
        Some(entry) => retire_unless_live(store, key, entry, now_ms, projection).await,
        None => Ok(None),
    }
}

/// The counts a projection can change: elements, element dots, removes, and whether a register
/// is held. On a stored value, which is normalized, [`Entry::retired_at`] only drops, so a
/// projection with the same shape is the same value.
fn projection_shape(entry: &Entry) -> (usize, usize, usize, bool) {
    (
        entry.data.len(),
        entry.crdt.dots.len(),
        entry.crdt.tombstones.len(),
        entry.crdt.register.is_some(),
    )
}

/// Keep `entry`, read from `store` at `key` and projected to its element horizon, iff it is live
/// at `now_ms`; otherwise report it absent, and remove it under [`Projection::WrittenBack`].
///
/// A live projection that retired something is written back under
/// [`Projection::WrittenBack`], so a carrier held live past its bound by a remove or register
/// holds only its remove side on disk, and no retired payload occupies the byte budget or the
/// row cap. Without a write-back (the fetch cache), the stored value is at most less retired
/// than the one returned: `retire_t ∘ retire_s = retire_max(s, t)`, so every later read
/// returns a value at least as retired.
async fn retire_unless_live(
    store: &EntryStorage,
    key: &str,
    entry: Entry,
    now_ms: u128,
    projection: Projection,
) -> Result<Option<Entry>> {
    let stored_shape = projection_shape(&entry);
    let Some(entry) = entry.live_at(now_ms) else {
        if projection.writes_back() {
            store.remove(key).await?;
        }
        return Ok(None);
    };
    if projection.writes_back() && projection_shape(&entry) != stored_shape {
        store.put(key, &entry).await?;
    }
    Ok(Some(entry))
}

/// Join the `admitted` value into the live value stored at `key`, project the join to its
/// element horizon at `now_ms`, and store it.
///
/// Pre: the caller holds the transition of `store`.
/// Post: the stored value is `(live ⊔ admitted).retired_at(now_ms)` when a live value exists,
/// otherwise `admitted.retired_at(now_ms)`, normalized for storage either way (the join
/// normalizes the union, the admission normalized `admitted`); it is returned. A result past
/// its tombstone bound is refused with `EntryTombstonesExceedMax` and the slot is unchanged
/// (#921).
async fn join_live_entry(
    store: &EntryStorage,
    key: &str,
    admitted: AdmittedEntry,
    now_ms: u128,
) -> Result<Entry> {
    let admitted = admitted.into_entry();
    let joined = match live_entry(store, key, now_ms, Projection::ReturnedOnly).await? {
        Some(live) => live.join(admitted)?,
        None => admitted,
    }
    .retired_at(now_ms);
    joined.check_tombstone_bound()?;
    store.put(key, &joined).await?;
    Ok(joined)
}

/// Apply `op` to the live value stored at `key`, or to the operation's empty carrier `⊥` when
/// none is live, and store the result iff it is live at `now_ms`: the operation transition an
/// owner applies to replicated storage and a writer to its own fetch cache.
///
/// Pre: the caller holds the transition of `store`, and `op` passed the admission law.
/// Post: the slot holds `(live or ⊥).operate(op)` when that is live at `now_ms`, and is clear
/// otherwise, since a stored value is always live when written; a result past its tombstone
/// bound is refused with `EntryTombstonesExceedMax` and the slot is unchanged (#921).
async fn operate_live_entry(
    store: &EntryStorage,
    key: &str,
    op: EntryOperation,
    actor: Did,
    now_ms: u128,
) -> Result<()> {
    let local = match live_entry(store, key, now_ms, Projection::ReturnedOnly).await? {
        Some(local) => local,
        None => op.gen_default_entry()?,
    };
    match local.operate(now_ms, op, actor)?.live_at(now_ms) {
        Some(value) => {
            value.check_tombstone_bound()?;
            store.put(key, &value).await
        }
        None => store.remove(key).await,
    }
}

/// Transition law: every read-modify-write of a slot (an operation, a join, an acknowledged
/// removal, and the retirement a read performs) runs under its store's transition, one at a
/// time: the storage transition for replicated storage, the cache transition for the fetch
/// cache. The two stores share no slot and no invariant, so each has its own lock, and no path
/// holds both. The inbound actor and the stabilizer write the same slots concurrently (a hold
/// arriving while the recipient drains its inbox, a hand-off joining while a repair pass reads,
/// two fetch replies, or a reply and the writer's own write, meeting in one cache slot), and a
/// store itself only orders single puts, so without this serialization one of two interleaved
/// read-modify-writes would overwrite the other and a held message, an observed reply or an
/// observed write could be lost. The transition is held across the store's own awaits and
/// nothing else, so it never nests and never waits on the network.
impl PeerRing {
    /// Read the live replicated entry of `kind` stored at `placement` on this node, projected at
    /// `now_ms` as every storage read is: test support for the controlled-network tests of
    /// downstream crates, which observe one replica; production reads a key through a lookup.
    /// The stores are crate-private, so no reader bypasses the projection.
    ///
    /// Post: read only. Unlike a read under the storage transition, it neither writes the
    /// projection back nor retires a value that is no longer live; it reports that as absent.
    #[cfg(feature = "dummy")]
    pub async fn stored_entry_at(
        &self,
        kind: EntryKind,
        placement: Did,
        now_ms: u128,
    ) -> Result<Option<Entry>> {
        let key = StorageKey::new(kind, placement).to_string();
        let Some(store) = self.carrier_storage(kind) else {
            return Ok(None);
        };
        live_entry(store, &key, now_ms, Projection::ReturnedOnly).await
    }

    /// Read the live replicated entry stored at `key`.
    pub(crate) async fn live_storage_entry(
        &self,
        key: StorageKey,
        now_ms: u128,
    ) -> Result<Option<Entry>> {
        let Some(store) = self.carrier_storage(key.kind()) else {
            return Ok(None);
        };
        let _transition = self.storage_transition.lock().await;
        live_entry(store, &key.to_string(), now_ms, Projection::WrittenBack).await
    }

    /// Every live entry of both namespaces with its key, retiring the rest. Each store yields
    /// only the carriers of its own kind: a relay carrier an earlier version kept in the data
    /// store is never read again, and leaves that store with its budget (#1021).
    ///
    /// Post: every returned entry satisfies `is_live_at(now_ms)`; every stored entry that does
    /// not has been removed.
    pub(crate) async fn live_storage_entries(
        &self,
        now_ms: u128,
    ) -> Result<Vec<(StorageKey, Entry)>> {
        let _transition = self.storage_transition.lock().await;
        let mut live = Vec::new();
        for kind in [EntryKind::Data, EntryKind::RelayMessage] {
            let Some(store) = self.carrier_storage(kind) else {
                continue;
            };
            for (key, entry) in store.get_all().await? {
                let slot = StorageKey::from_str(&key)?;
                if slot.kind() != kind {
                    continue;
                }
                if let Some(entry) =
                    retire_unless_live(store, &key, entry, now_ms, Projection::WrittenBack).await?
                {
                    live.push((slot, entry));
                }
            }
        }
        Ok(live)
    }

    /// Remove the value stored at `key` iff `ack` proves the receiver holds exactly that
    /// value: the ack-gated local cleanup of an ownership hand-off.
    ///
    /// Post: a value written after the hand-off copy was taken differs from the acked one and
    /// stays; the comparison and the removal are one storage transition, so a write landing
    /// between them cannot be removed by an ack for an older value.
    pub(crate) async fn remove_storage_entry_confirmed_by(
        &self,
        key: StorageKey,
        now_ms: u128,
        ack: &SyncedEntryAck,
    ) -> Result<()> {
        let Some(store) = self.carrier_storage(key.kind()) else {
            return Ok(());
        };
        let _transition = self.storage_transition.lock().await;
        let Some(local) =
            live_entry(store, &key.to_string(), now_ms, Projection::WrittenBack).await?
        else {
            return Ok(());
        };
        if ack.confirms_local_value(&local, now_ms) {
            store.remove(&key.to_string()).await?;
        }
        Ok(())
    }

    /// Join a peer-supplied replicated value into local storage at time `now_ms`.
    ///
    /// Pre: `incoming` is the value a peer supplied, not a local join result; it is judged by
    /// the admission law of [`Entry::validate_admissible_at`] here, so every replication path
    /// shares one rule.
    /// Post: the stored value is the least upper bound of the previous live local
    /// value and `incoming` when a previous value exists; otherwise it is
    /// `incoming` normalized for storage. Either is projected to its element horizon at
    /// `now_ms`, which commutes with the join. A result past its tombstone bound is refused with
    /// `EntryTombstonesExceedMax` and the slot is unchanged (#921), so an unacknowledged hand-off
    /// stays with its sender.
    pub(crate) async fn join_storage_entry(
        &self,
        now_ms: u128,
        key: Did,
        incoming: Entry,
    ) -> Result<Entry> {
        incoming.check_admissible_at(now_ms, self.network_id())?;
        let store = self.carrier_store_for_write(incoming.kind)?;
        let key = StorageKey::new(incoming.kind, key).to_string();
        let _transition = self.storage_transition.lock().await;
        // The join normalizes the union once, so the incoming value is normalized on its own
        // only when there is nothing to join it with (`join_live_entry` takes an admitted value,
        // normalized first, so it would hash every element twice).
        let local = live_entry(store, &key, now_ms, Projection::ReturnedOnly).await?;
        let stored = match local {
            Some(local) => local.join(incoming)?,
            None => incoming.try_into_storage_entry()?,
        }
        .retired_at(now_ms);
        stored.check_tombstone_bound()?;
        store.put(&key, &stored).await?;
        Ok(stored)
    }

    /// [`Self::join_storage_entry`] for a value already admitted, so its admission (signatures
    /// included) runs once, where the value entered: a peer's storage sync batch, or this node's
    /// own storage.
    ///
    /// Post: the stored value is the least upper bound of the previous live local value and
    /// `admitted` when a previous value exists; otherwise it is `admitted`, normalized by its
    /// admission. Either is projected to its element horizon at `now_ms`, which commutes with
    /// the join.
    pub(crate) async fn join_admitted_storage_entry(
        &self,
        now_ms: u128,
        key: Did,
        admitted: AdmittedEntry,
    ) -> Result<Entry> {
        let store = self.carrier_store_for_write(admitted.kind())?;
        let key = StorageKey::new(admitted.kind(), key).to_string();
        let _transition = self.storage_transition.lock().await;
        join_live_entry(store, &key, admitted, now_ms).await
    }

    /// Apply a stamped operation issued by `writer` to the value stored at `placement` at time
    /// `now_ms`.
    ///
    /// Pre: `op` is stamped, and `writer` is its signer as the shell verified it (this node for
    /// a local operation). Admission is checked on the delta `op` carries, never on the join
    /// result, so a locally derived version (an overwrite floor bumped by one step) is never
    /// mistaken for a peer clock running ahead; a relay inbox also passes the authority law
    /// under this node's own routing view.
    /// Post: the slot holds `local.operate(op)` normalized for storage and projected to its
    /// element horizon at `now_ms`, where `local` is the live stored value or the operation's
    /// default carrier, when that result is live; a result
    /// with no retention (a removal against nothing held) leaves the slot empty, so a stored
    /// value is always live when written.
    pub(crate) async fn operate_storage_entry(
        &self,
        now_ms: u128,
        placement: Did,
        op: EntryOperation,
        writer: Did,
    ) -> Result<()> {
        match op.entry().kind {
            EntryKind::Data => op.validate_admissible_at(now_ms, self.network_id())?,
            EntryKind::RelayMessage => {
                op.entry().validate_bounds_at(now_ms)?;
                // Only a hold needs to know who is responsible for the recipient.
                let admitted = match op {
                    EntryOperation::Extend(_) => {
                        self.admits_relay_hold(inbox_destination(placement), writer)?
                    }
                    _ => false,
                };
                op.validate_inbox_write(writer, admitted, now_ms, self.network_id())?;
            }
        }
        let store = self.carrier_store_for_write(op.kind())?;
        let key = StorageKey::new(op.kind(), placement).to_string();
        let _transition = self.storage_transition.lock().await;
        operate_live_entry(store, &key, op, self.did, now_ms).await
    }

    /// The step one placement of a lookup takes under the topology and local storage at
    /// `now_ms`: the per-placement `Compute` shared by a whole lookup and by the rerouting of one
    /// placement.
    ///
    /// ```text
    /// find_storage_owner(placement)
    ///   ├─ Some(owner) ─▶ [live value stored here?] ── yes ─▶ Found(value)
    ///   │                        │ no
    ///   │                        ├─ owner ≠ self ─────────────────────▶ Remote(owner)
    ///   │                        └─ owner = self ─▶ [fallback ∧ virtual nodes ∧ ∃ head]
    ///   │                                              ├─ yes ─▶ Remote(head)
    ///   │                                              └─ no  ─▶ Missed(placement, self)
    ///   └─ RemoteAction(next, FindSuccessor(id)) ──────────────────▶ Remote(next) for id
    /// ```
    pub(crate) async fn lookup_placement(
        &self,
        query: EntryLookupKey,
        fallback_on_local_virtual_miss: bool,
        now_ms: u128,
    ) -> Result<PlacementLookup> {
        match self.find_storage_owner(query.placement)? {
            PeerRingAction::Some(owner) => {
                // A lookup reads the data namespace: a relay inbox is never fetched, its
                // recipient drains it from local storage, so a lookup at its position sees it as
                // absent.
                let key = StorageKey::new(EntryKind::Data, query.placement);
                // A carrier past its retention bound serves no element: it answers as a miss,
                // so the lookup asks the next placement and read-repair joins the missed one,
                // instead of an empty value shadowing a replica with data.
                let served = self
                    .live_storage_entry(key, now_ms)
                    .await?
                    .filter(|value| value.bound_live_at(now_ms));
                if let Some(value) = served {
                    return Ok(PlacementLookup::Settled(LookupSettlement::Found(value)));
                }
                tracing::debug!("Cannot find entry in local storage, try to query from successor");
                if owner != self.did {
                    return Ok(PlacementLookup::Remote { next: owner, query });
                }
                let head =
                    if fallback_on_local_virtual_miss && self.storage_virtual_nodes_enabled()? {
                        self.with_topology_state(topology::successor_head)?
                    } else {
                        None
                    };
                Ok(match head {
                    Some(next) => PlacementLookup::Remote { next, query },
                    None => PlacementLookup::Settled(LookupSettlement::Missed(PlacementMiss::new(
                        query.placement,
                        owner,
                    ))),
                })
            }
            PeerRingAction::RemoteAction(next, RemoteAction::FindSuccessor(id)) => {
                Ok(PlacementLookup::Remote {
                    next,
                    query: EntryLookupKey::new(query.resource, id),
                })
            }
            action => Err(Error::unexpected_peer_ring_action(action)),
        }
    }

    /// Look up every placement of `entry_key`: the first stored value ends the lookup with the
    /// misses observed before it; otherwise every remote placement becomes a `FindEntry`.
    async fn entry_lookup_inner(
        &self,
        entry_key: Did,
        fallback_on_local_virtual_miss: bool,
        redundancy: u16,
    ) -> Result<PeerRingAction> {
        let now_ms = get_epoch_ms();
        let mut ret = vec![];
        let mut misses = vec![];
        for placement_key in entry_key.rotate_affine(redundancy)? {
            let query = EntryLookupKey::new(entry_key, placement_key);
            match self
                .lookup_placement(query, fallback_on_local_virtual_miss, now_ms)
                .await?
            {
                PlacementLookup::Settled(LookupSettlement::Found(value)) => {
                    return Ok(PeerRingAction::SomeEntry(EntryLookupEvidence::new(
                        value, misses,
                    )));
                }
                PlacementLookup::Settled(LookupSettlement::Missed(miss)) => misses.push(miss),
                PlacementLookup::Remote { next, query } => {
                    ret.push(PeerRingAction::RemoteAction(
                        next,
                        RemoteAction::FindEntry(query),
                    ));
                }
            }
        }
        if !misses.is_empty() {
            ret.push(PeerRingAction::EntryMisses(misses));
        }
        Ok(ret.into())
    }

    /// Look up an [`Entry`] for a local storage fetch.
    ///
    /// A fresh node with storage virtual nodes enabled can observe itself as the
    /// owner for an existing placement before sync has copied historical data
    /// locally. Local fetches may ask a known successor for that placement so
    /// read repair can converge instead of treating the fresh local miss as
    /// authoritative. Remote `SearchEntry` handling uses [`Self::entry_lookup`] and
    /// intentionally does not enable this fallback.
    pub(crate) async fn entry_lookup_for_fetch(
        &self,
        entry_key: Did,
        redundancy: u16,
    ) -> Result<PeerRingAction> {
        self.entry_lookup_inner(entry_key, true, redundancy).await
    }

    /// Look up an [`Entry`] by its ring key under `redundancy` placements, always
    /// through the DHT and never the local cache.
    ///
    /// An [`Entry`] key has the same representation as a node [`Did`], but it is
    /// not a node identity: it only chooses the node responsible for storing the
    /// entry. The returned action forwards the lookup when the storing node is
    /// not this one.
    pub(crate) async fn entry_lookup(
        &self,
        entry_key: Did,
        redundancy: u16,
    ) -> Result<PeerRingAction> {
        self.entry_lookup_inner(entry_key, false, redundancy).await
    }

    /// Where an operation of `kind` at `placement` is applied under the topology now: the
    /// per-placement `Compute` shared by [`Self::entry_operate`] and the rerouting of one
    /// placement.
    ///
    /// A relay carrier goes to its holder (`dht::relay_holder`, `Law (Holder)`), which this
    /// node's view names whenever it writes one: a holder places the inbox key of a destination
    /// it is responsible for, and a recipient owns its own. A view that names no holder refuses
    /// the operation to its sender with `RelayHolderUnavailable` (`Law (Bound)`).
    pub(crate) fn operate_route(&self, placement: Did, kind: EntryKind) -> Result<OperateRoute> {
        if kind.is_relay_inbox() {
            return match self.relay_holder(placement)? {
                RelayHolder::Holder(holder) if holder == self.did => Ok(OperateRoute::Owned),
                RelayHolder::Holder(holder) => Ok(OperateRoute::Remote(holder)),
                RelayHolder::Unheld | RelayHolder::Unknown => Err(Error::RelayHolderUnavailable),
            };
        }
        match self.find_storage_owner(placement)? {
            PeerRingAction::Some(_) => Ok(OperateRoute::Owned),
            PeerRingAction::RemoteAction(next, RemoteAction::FindSuccessor(_)) => {
                Ok(OperateRoute::Remote(next))
            }
            action => Err(Error::unexpected_peer_ring_action(action)),
        }
    }

    /// Apply `op` under a runtime `redundancy`: locally at every accepted placement, and as a
    /// [`RemoteAction::FindEntryForOperate`] toward every remote one.
    pub(crate) async fn entry_operate(
        &self,
        op: EntryOperation,
        redundancy: u16,
    ) -> Result<PeerRingAction> {
        let now_ms = get_epoch_ms();
        let op = op.stamped(now_ms, self.did)?;
        let entry_key = op.did()?;
        let kind = op.entry().kind;
        let redundancy = kind.replication(redundancy);
        let mut ret = vec![];
        for placement in entry_key.rotate_affine(redundancy)? {
            match self.operate_route(placement, kind)? {
                OperateRoute::Owned => {
                    self.operate_storage_entry(now_ms, placement, op.clone(), self.did)
                        .await?;
                }
                OperateRoute::Remote(next) => ret.push(PeerRingAction::RemoteAction(
                    next,
                    RemoteAction::FindEntryForOperate(Box::new(PlacedEntryOperation {
                        placement,
                        op: op.clone(),
                    })),
                )),
            }
        }
        Ok(ret.into())
    }
}

impl PeerRing {
    /// Read the cached carrier at `entry_key` as the cache holds it, for read-repair of a missed
    /// placement.
    ///
    /// Post: the live carrier projected at the current clock, a carrier past its retention
    /// bound included: such a carrier serves no element and is absent from
    /// [`ChordStorageCache::local_cache_get`] (see [`Entry::bound_live_at`]), but the
    /// removes and register that hold it live are what the repair spreads.
    ///
    /// `held` ⊇ `served`: `local_cache_get = filter(bound_live_at) ∘ local_cache_held`.
    /// A cache read takes no transition (only the cache's read-join puts do), so the
    /// projection is returned only.
    pub(crate) async fn local_cache_held(
        &self,
        entry_key: Did,
        now_ms: u128,
    ) -> Result<Option<Entry>> {
        live_entry(
            &self.cache,
            &entry_key.to_string(),
            now_ms,
            Projection::ReturnedOnly,
        )
        .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl ChordStorageCache<PeerRingAction> for PeerRing {
    /// Join a fetched entry into the cache: `cache[k] ← cache[k] ⊔ entry` (#864).
    ///
    /// Pre: `entry` satisfies the same admission law as a replicated write, so a peer cannot
    /// pin a fetched value in the cache past the retention bound it could obtain in storage.
    /// Post: the cached carrier is the join of every live reply observed for its key since the
    /// key last became resident, with this node's own writes applied
    /// ([`Self::local_cache_observe_write`]), so, within one residency, it does not depend on
    /// the order in which replicas answer; it is projected to its element horizon, as every
    /// stored value is.
    /// The read-join-write is one cache transition, so two concurrent replies cannot lose one
    /// side.
    async fn local_cache_put(&self, entry: Entry) -> Result<()> {
        if entry.kind.is_relay_inbox() {
            return Err(Error::RelayInboxOperationNotAllowed);
        }
        let now_ms = get_epoch_ms();
        let key = entry.did.to_string();
        let admitted = entry.validate_admissible_at(now_ms, self.network_id())?;
        let _transition = self.cache_transition.lock().await;
        join_live_entry(&self.cache, &key, admitted, now_ms)
            .await
            .map(drop)
    }

    /// Observe this node's own write `op` in the cache: `cache[k] ← (cache[k] or ⊥).operate(op)`,
    /// the transition every owner applies (#982).
    ///
    /// Pre: `op` is stamped, as the owners receive it; stamping is idempotent, so its dots are
    /// the ones every owner applies.
    /// Post: an overwrite or an extend leaves `cache[k] ⊔ stamped(op)`, below every replica
    /// that has applied `op` (`R.operate(op) = R ⊔ stamped(op)`), so the cache still ascends in
    /// `⊑`; a tombstone leaves covering removes at the dots of its values the cache holds, the
    /// dots this writer observed and asked to remove. An operation the admission law refuses,
    /// which no owner applies, and a relay inbox, which no lookup reads, are not observed.
    async fn local_cache_observe_write(&self, op: EntryOperation) -> Result<()> {
        let now_ms = get_epoch_ms();
        if op.kind().is_relay_inbox()
            || op
                .validate_admissible_at(now_ms, self.network_id())
                .is_err()
        {
            return Ok(());
        }
        let key = op.entry().did.to_string();
        let _transition = self.cache_transition.lock().await;
        operate_live_entry(&self.cache, &key, op, self.did, now_ms).await
    }

    /// Read a cached entry, as a lookup serves it.
    ///
    /// Post: a cached carrier past its retention bound, held live only by an unstable remove or
    /// register, is served as absent, as a replica serves it: a cache entry that serves no
    /// element must not answer a fetch as a found, empty topic while the owners hold live data.
    async fn local_cache_get(&self, entry_key: Did) -> Result<Option<Entry>> {
        let now_ms = get_epoch_ms();
        let held = self.local_cache_held(entry_key, now_ms).await?;
        Ok(held.filter(|entry| entry.bound_live_at(now_ms)))
    }
}
