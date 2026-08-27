use async_trait::async_trait;

use super::PeerRing;
use super::PeerRingAction;
use super::RemoteAction;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryLookupEvidence;
use crate::dht::entry::EntryLookupKey;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::entry::PlacementMiss;
use crate::dht::types::ChordStorage;
use crate::dht::types::ChordStorageCache;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

impl PeerRing {
    /// Join an incoming replicated entry delta into local storage.
    ///
    /// Post: the stored value is the least upper bound of the previous local
    /// value and `incoming` when a previous value exists; otherwise it is
    /// `incoming` normalized for storage.
    pub(crate) async fn join_storage_entry(&self, key: Did, incoming: Entry) -> Result<Entry> {
        let incoming = incoming.try_into_storage_entry()?;
        let stored = if let Some(local) = self.storage.get(&key.to_string()).await? {
            local.join(incoming)?
        } else {
            incoming
        }
        .try_into_storage_entry()?;
        self.storage.put(&key.to_string(), &stored).await?;
        Ok(stored)
    }

    fn storage_fetch_fallback_successor(&self) -> Result<Option<Did>> {
        Ok(self
            .topology_state()?
            .successors
            .into_iter()
            .find(|successor| *successor != self.did))
    }

    async fn entry_lookup_inner<const REDUNDANT: u16>(
        &self,
        entry_key: Did,
        fallback_on_local_virtual_miss: bool,
    ) -> Result<PeerRingAction> {
        let mut ret = vec![];
        let mut misses = vec![];
        for placement_key in entry_key.rotate_affine(REDUNDANT)? {
            let query = EntryLookupKey::new(entry_key, placement_key);
            let act = match self.find_storage_owner(placement_key) {
                Ok(PeerRingAction::Some(succ)) => {
                    match self.storage.get(&placement_key.to_string()).await {
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
    pub(crate) async fn entry_lookup_for_fetch<const REDUNDANT: u16>(
        &self,
        entry_key: Did,
    ) -> Result<PeerRingAction> {
        self.entry_lookup_inner::<REDUNDANT>(entry_key, true).await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl<const REDUNDANT: u16> ChordStorage<PeerRingAction, REDUNDANT> for PeerRing {
    async fn entry_lookup(&self, entry_key: Did) -> Result<PeerRingAction> {
        self.entry_lookup_inner::<REDUNDANT>(entry_key, false).await
    }

    async fn entry_operate(&self, op: EntryOperation) -> Result<PeerRingAction> {
        let op = op.stamped(self.did)?;
        let entry_key = op.did()?;
        let mut ret = vec![];
        for entry_key in entry_key.rotate_affine(REDUNDANT)? {
            let act = match self.find_storage_owner(entry_key) {
                Ok(PeerRingAction::Some(_)) => {
                    let this = match self.storage.get(&entry_key.to_string()).await? {
                        Some(this) => this,
                        None => op.clone().gen_default_entry()?,
                    };
                    let entry = this.operate(op.clone(), self.did)?;
                    self.join_storage_entry(entry_key, entry).await?;
                    Ok(PeerRingAction::None)
                }
                Ok(PeerRingAction::RemoteAction(next, RemoteAction::FindSuccessor(_))) => {
                    Ok(PeerRingAction::RemoteAction(
                        next,
                        RemoteAction::FindEntryForOperate(PlacedEntryOperation {
                            placement: entry_key,
                            op: op.clone(),
                        }),
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
impl ChordStorageCache<PeerRingAction> for PeerRing {
    async fn local_cache_put(&self, entry: Entry) -> Result<()> {
        self.cache.put(&entry.did.to_string(), &entry).await
    }

    async fn local_cache_get(&self, entry_key: Did) -> Result<Option<Entry>> {
        self.cache.get(&entry_key.to_string()).await
    }
}
