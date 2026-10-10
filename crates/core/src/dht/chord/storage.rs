use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;

use super::PeerRing;
use super::PeerRingAction;
use super::RemoteAction;
use crate::dht::entry::inbox::inbox_destination;
use crate::dht::entry::inbox::inbox_key;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryLookupEvidence;
use crate::dht::entry::EntryLookupKey;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::entry::PlacementMiss;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::topology;
use crate::dht::types::ChordStorageCache;
use crate::dht::Did;
use crate::dht::EntryStorage;
use crate::error::Error;
use crate::error::Result;
use crate::utils::get_epoch_ms;

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
    /// subsumes the write-back and the retirement, or the store is the fetch cache, which has
    /// no transition, so a write or a removal could undo a concurrent put. A dead cached value
    /// stays until a put replaces it or the cache's count bound evicts it, and is reported
    /// absent meanwhile.
    ReturnedOnly,
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
        if projection == Projection::WrittenBack {
            store.remove(key).await?;
        }
        return Ok(None);
    };
    if projection == Projection::WrittenBack && projection_shape(&entry) != stored_shape {
        store.put(key, &entry).await?;
    }
    Ok(Some(entry))
}

/// Storage transition law: every read-modify-write of a slot (an operation, a join, an
/// acknowledged removal, and the retirement a read performs) runs under the ring's storage
/// transition, one at a time. The inbound actor and the stabilizer write the same slots
/// concurrently (a hold arriving while the recipient drains its inbox, a hand-off joining while
/// a repair pass reads), and the store itself only orders single puts, so without this
/// serialization one of two interleaved read-modify-writes would overwrite the other and a held
/// message could be lost. The transition is held across the store's own awaits and nothing
/// else, so it never nests and never waits on the network.
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
        Ok(self
            .storage
            .get(&key)
            .await?
            .and_then(|entry| entry.live_at(now_ms)))
    }

    /// Read the live replicated entry stored at `key`.
    pub(crate) async fn live_storage_entry(
        &self,
        key: StorageKey,
        now_ms: u128,
    ) -> Result<Option<Entry>> {
        let _transition = self.storage_transition.lock().await;
        live_entry(
            &self.storage,
            &key.to_string(),
            now_ms,
            Projection::WrittenBack,
        )
        .await
    }

    /// Every live replicated entry with its key, retiring the rest.
    ///
    /// Post: every returned entry satisfies `is_live_at(now_ms)`; every stored entry that does
    /// not has been removed.
    pub(crate) async fn live_storage_entries(
        &self,
        now_ms: u128,
    ) -> Result<Vec<(StorageKey, Entry)>> {
        let _transition = self.storage_transition.lock().await;
        let mut live = Vec::new();
        for (key, entry) in self.storage.get_all().await? {
            if let Some(entry) =
                retire_unless_live(&self.storage, &key, entry, now_ms, Projection::WrittenBack)
                    .await?
            {
                live.push((StorageKey::from_str(&key)?, entry));
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
        let _transition = self.storage_transition.lock().await;
        let Some(local) = live_entry(
            &self.storage,
            &key.to_string(),
            now_ms,
            Projection::WrittenBack,
        )
        .await?
        else {
            return Ok(());
        };
        if ack.confirms_local_value(&local, now_ms) {
            self.storage.remove(&key.to_string()).await?;
        }
        Ok(())
    }

    /// Remove the elements `removal` names from the relay carrier stored at `key`, as this node
    /// holds it at `now_ms`, and return the carrier that remains.
    ///
    /// Pre: `removal` is a removal delta of that carrier ([`Entry::removal_of`]). It is applied
    /// locally, outside the inbox write law, which admits a remote removal only from the
    /// recipient: the caller removes what fails the witness for good, which no write law could
    /// have admitted.
    /// Post: dots are unique to elements, so a join landing since `removal` was computed loses
    /// nothing it did not name; a carrier left without retention is removed.
    pub(crate) async fn remove_inbox_elements(
        &self,
        key: StorageKey,
        removal: Entry,
        now_ms: u128,
    ) -> Result<Option<Entry>> {
        let key = key.to_string();
        let _transition = self.storage_transition.lock().await;
        let Some(local) = live_entry(&self.storage, &key, now_ms, Projection::WrittenBack).await?
        else {
            return Ok(None);
        };
        let stored = local.tombstone(removal)?.retired_at(now_ms);
        if stored.is_live_at(now_ms) {
            self.storage.put(&key, &stored).await?;
            Ok(Some(stored))
        } else {
            self.storage.remove(&key).await?;
            Ok(None)
        }
    }

    /// Join a peer-supplied replicated value into local storage at time `now_ms`.
    ///
    /// Pre: `incoming` is the value a peer supplied, not a local join result; it is admitted
    /// by [`Entry::validate_admissible_at`] here so every replication path shares one rule.
    /// Post: the stored value is the least upper bound of the previous live local
    /// value and `incoming` when a previous value exists; otherwise it is
    /// `incoming` normalized for storage. Either is projected to its element horizon at
    /// `now_ms`, which commutes with the join.
    pub(crate) async fn join_storage_entry(
        &self,
        now_ms: u128,
        key: Did,
        incoming: Entry,
    ) -> Result<Entry> {
        incoming.validate_admissible_at(now_ms, self.network_id())?;
        let key = StorageKey::new(incoming.kind, key).to_string();
        let _transition = self.storage_transition.lock().await;
        // The join normalizes the union once, so the incoming value is normalized on its own
        // only when there is nothing to join it with.
        let stored = match live_entry(&self.storage, &key, now_ms, Projection::ReturnedOnly).await?
        {
            Some(local) => local.join(incoming)?,
            None => incoming.try_into_storage_entry()?,
        }
        .retired_at(now_ms);
        self.storage.put(&key, &stored).await?;
        Ok(stored)
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
                let responsible = match op {
                    EntryOperation::Extend(_) => {
                        self.inbox_hold_authority(inbox_destination(placement))?
                    }
                    _ => None,
                };
                op.validate_inbox_write(writer, responsible, now_ms, self.network_id())?;
            }
        }
        let key = StorageKey::new(op.kind(), placement).to_string();
        let _transition = self.storage_transition.lock().await;
        let local = match live_entry(&self.storage, &key, now_ms, Projection::ReturnedOnly).await? {
            Some(local) => local,
            None => op.gen_default_entry()?,
        };
        let stored = local.operate(now_ms, op, self.did)?.retired_at(now_ms);
        if stored.is_live_at(now_ms) {
            self.storage.put(&key, &stored).await
        } else {
            self.storage.remove(&key).await
        }
    }

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
            // A lookup reads the data namespace: a relay inbox is never fetched, its recipient
            // drains it from local storage, so a lookup at its position sees it as absent.
            let key = StorageKey::new(EntryKind::Data, placement_key);
            let act = match self.find_storage_owner(placement_key) {
                Ok(PeerRingAction::Some(succ)) => {
                    // A carrier past its retention bound serves no element: it answers as a
                    // miss, so the lookup asks the next placement and read-repair joins the
                    // missed one, instead of an empty value shadowing a replica with data.
                    let served = self
                        .live_storage_entry(key, now_ms)
                        .await
                        .map(|value| value.filter(|value| value.answers_lookups_at(now_ms)));
                    match served {
                        Ok(Some(value)) => {
                            let observed_misses = std::mem::take(&mut misses);
                            Ok(PeerRingAction::SomeEntry(EntryLookupEvidence::new(
                                value,
                                observed_misses,
                            )))
                        }
                        Ok(None) => {
                            tracing::debug!(
                                "Cannot find entry in local storage, try to query from successor"
                            );
                            if succ == self.did {
                                if fallback_on_local_virtual_miss
                                    && self.storage_virtual_nodes_enabled()?
                                {
                                    if let Some(next) =
                                        self.with_topology_state(topology::successor_head)?
                                    {
                                        Ok(PeerRingAction::RemoteAction(
                                            next,
                                            RemoteAction::FindEntry(query),
                                        ))
                                    } else {
                                        misses.push(PlacementMiss::new(placement_key, succ));
                                        Ok(PeerRingAction::None)
                                    }
                                } else {
                                    misses.push(PlacementMiss::new(placement_key, succ));
                                    Ok(PeerRingAction::None)
                                }
                            } else {
                                Ok(PeerRingAction::RemoteAction(
                                    succ,
                                    RemoteAction::FindEntry(query),
                                ))
                            }
                        }
                        Err(error) => Err(error),
                    }
                }
                Ok(PeerRingAction::RemoteAction(next, RemoteAction::FindSuccessor(id))) => {
                    Ok(PeerRingAction::RemoteAction(
                        next,
                        RemoteAction::FindEntry(EntryLookupKey::new(entry_key, id)),
                    ))
                }
                Ok(action) => Err(Error::unexpected_peer_ring_action(action)),
                Err(error) => Err(error),
            }?;
            if act.is_remote() {
                ret.push(act);
            } else if act.is_some_entry() {
                return Ok(act);
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
        for entry_key in entry_key.rotate_affine(redundancy)? {
            let act = match self.find_storage_owner_for(entry_key, kind) {
                Ok(PeerRingAction::Some(_)) => {
                    self.operate_storage_entry(now_ms, entry_key, op.clone(), self.did)
                        .await?;
                    Ok(PeerRingAction::None)
                }
                Ok(PeerRingAction::RemoteAction(next, RemoteAction::FindSuccessor(_))) => {
                    Ok(PeerRingAction::RemoteAction(
                        next,
                        RemoteAction::FindEntryForOperate(Box::new(PlacedEntryOperation {
                            placement: entry_key,
                            op: op.clone(),
                        })),
                    ))
                }
                Ok(action) => Err(Error::unexpected_peer_ring_action(action)),
                Err(error) => Err(error),
            }?;
            if act.is_remote() {
                ret.push(act);
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
    /// [`ChordStorageCache::local_cache_get`] (see [`Entry::answers_lookups_at`]), but the
    /// removes and register that hold it live are what the repair spreads.
    ///
    /// `held` ⊇ `served`: `local_cache_get = filter(answers_lookups_at) ∘ local_cache_held`.
    /// The fetch cache has no transition, so the projection is returned only.
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
    /// Cache a fetched entry.
    ///
    /// Pre: `entry` satisfies the same admission law as a replicated write, so a peer cannot
    /// pin a fetched value in the cache past the retention bound it could obtain in storage.
    /// Post: the cached value is `entry` projected to its element horizon, as every stored
    /// value is.
    async fn local_cache_put(&self, entry: Entry) -> Result<()> {
        if entry.kind.is_relay_inbox() {
            return Err(Error::RelayInboxOperationNotAllowed);
        }
        let now_ms = get_epoch_ms();
        entry.validate_admissible_at(now_ms, self.network_id())?;
        let entry = entry.try_into_storage_entry()?.retired_at(now_ms);
        self.cache.put(&entry.did.to_string(), &entry).await
    }

    /// Read a cached entry, as a lookup serves it.
    ///
    /// Post: a cached carrier past its retention bound, held live only by an unstable remove or
    /// register, is served as absent, as a replica serves it: a cache entry that serves no
    /// element must not answer a fetch as a found, empty topic while the owners hold live data.
    async fn local_cache_get(&self, entry_key: Did) -> Result<Option<Entry>> {
        let now_ms = get_epoch_ms();
        let held = self.local_cache_held(entry_key, now_ms).await?;
        Ok(held.filter(|entry| entry.answers_lookups_at(now_ms)))
    }
}
