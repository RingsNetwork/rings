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
use crate::dht::types::ChordStorage;
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

/// Read `key` from `store`, retiring a value that is no longer live.
///
/// Post: `Ok(Some(entry))` implies `entry.is_live_at(now_ms)`. A stored value whose
/// retention bound has elapsed (or that predates retention bounds) is removed and reported
/// absent, so expiry is enforced lazily on every read path instead of by a sweeper.
async fn live_entry(store: &EntryStorage, key: &str, now_ms: u128) -> Result<Option<Entry>> {
    match store.get(key).await? {
        Some(entry) => retire_unless_live(store, key, entry, now_ms).await,
        None => Ok(None),
    }
}

/// Keep `entry`, read from `store` at `key`, iff it is live at `now_ms`; remove it otherwise.
async fn retire_unless_live(
    store: &EntryStorage,
    key: &str,
    entry: Entry,
    now_ms: u128,
) -> Result<Option<Entry>> {
    if entry.is_live_at(now_ms) {
        return Ok(Some(entry));
    }
    store.remove(key).await?;
    Ok(None)
}

impl PeerRing {
    /// Read the live replicated entry stored at `key`.
    pub(crate) async fn live_storage_entry(
        &self,
        key: StorageKey,
        now_ms: u128,
    ) -> Result<Option<Entry>> {
        live_entry(&self.storage, &key.to_string(), now_ms).await
    }

    /// Every live replicated entry with its key, retiring the rest.
    ///
    /// Post: every returned entry satisfies `is_live_at(now_ms)`; every stored entry that does
    /// not has been removed.
    pub(crate) async fn live_storage_entries(
        &self,
        now_ms: u128,
    ) -> Result<Vec<(StorageKey, Entry)>> {
        let mut live = Vec::new();
        for (key, entry) in self.storage.get_all().await? {
            if let Some(entry) = retire_unless_live(&self.storage, &key, entry, now_ms).await? {
                live.push((StorageKey::from_str(&key)?, entry));
            }
        }
        Ok(live)
    }

    /// Join a peer-supplied replicated value into local storage at time `now_ms`.
    ///
    /// Pre: `incoming` is the value a peer supplied, not a local join result; it is admitted
    /// by [`Entry::validate_admissible_at`] here so every replication path shares one rule.
    /// Post: the stored value is the least upper bound of the previous live local
    /// value and `incoming` when a previous value exists; otherwise it is
    /// `incoming` normalized for storage.
    pub(crate) async fn join_storage_entry(
        &self,
        now_ms: u128,
        key: Did,
        incoming: Entry,
    ) -> Result<Entry> {
        incoming.validate_admissible_at(now_ms, self.network_id())?;
        let key = StorageKey::new(incoming.kind, key);
        let incoming = incoming.try_into_storage_entry()?;
        let stored = if let Some(local) = self.live_storage_entry(key, now_ms).await? {
            local.join(incoming)?
        } else {
            incoming
        }
        .try_into_storage_entry()?;
        self.storage.put(&key.to_string(), &stored).await?;
        Ok(stored)
    }

    /// Apply a stamped operation issued by `writer` to the value stored at `placement` at time
    /// `now_ms`.
    ///
    /// Pre: `op` is stamped, and `writer` is its signer as the shell verified it (this node for
    /// a local operation). Admission is checked on the delta `op` carries, never on the join
    /// result, so a locally derived version (a compaction floor bumped by one step) is never
    /// mistaken for a peer clock running ahead; a relay inbox also passes the authority law
    /// under this node's own routing view.
    /// Post: the slot holds `local.operate(op)` normalized for storage, where `local` is the
    /// live stored value or the operation's default carrier, when that result is live; a result
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
        let key = StorageKey::new(op.kind(), placement);
        let local = match self.live_storage_entry(key, now_ms).await? {
            Some(local) => local,
            None => op.gen_default_entry()?,
        };
        let stored = local
            .operate(now_ms, op, self.did)?
            .try_into_storage_entry()?;
        if stored.is_live_at(now_ms) {
            self.storage.put(&key.to_string(), &stored).await
        } else {
            self.storage.remove(&key.to_string()).await
        }
    }

    fn storage_fetch_fallback_successor(&self) -> Result<Option<Did>> {
        Ok(self
            .topology_state()?
            .successors
            .into_iter()
            .find(|successor| *successor != self.did))
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
                    match self.live_storage_entry(key, now_ms).await {
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
                                    if let Some(next) = self.storage_fetch_fallback_successor()? {
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
    /// authoritative. Remote `SearchEntry` handling uses [`ChordStorage`] and
    /// intentionally does not enable this fallback.
    pub(crate) async fn entry_lookup_for_fetch(
        &self,
        entry_key: Did,
        redundancy: u16,
    ) -> Result<PeerRingAction> {
        self.entry_lookup_inner(entry_key, true, redundancy).await
    }

    /// Apply `op` under a runtime `redundancy`: locally at every accepted placement, and as a
    /// [`RemoteAction::FindEntryForOperate`] toward every remote one.
    pub(crate) async fn entry_operate_with_redundancy(
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

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl<const REDUNDANT: u16> ChordStorage<PeerRingAction, REDUNDANT> for PeerRing {
    async fn entry_lookup(&self, entry_key: Did) -> Result<PeerRingAction> {
        self.entry_lookup_inner(entry_key, false, REDUNDANT).await
    }

    async fn entry_operate(&self, op: EntryOperation) -> Result<PeerRingAction> {
        self.entry_operate_with_redundancy(op, REDUNDANT).await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl ChordStorageCache<PeerRingAction> for PeerRing {
    /// Cache a fetched entry.
    ///
    /// Pre: `entry` satisfies the same admission law as a replicated write, so a peer cannot
    /// pin a fetched value in the cache past the retention bound it could obtain in storage.
    async fn local_cache_put(&self, entry: Entry) -> Result<()> {
        if entry.kind.is_relay_inbox() {
            return Err(Error::RelayInboxOperationNotAllowed);
        }
        entry.validate_admissible_at(get_epoch_ms(), self.network_id())?;
        self.cache.put(&entry.did.to_string(), &entry).await
    }

    async fn local_cache_get(&self, entry_key: Did) -> Result<Option<Entry>> {
        live_entry(&self.cache, &entry_key.to_string(), get_epoch_ms()).await
    }
}
