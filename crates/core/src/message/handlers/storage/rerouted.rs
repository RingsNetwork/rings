//! The remote placements of user DHT operations: the instances of
//! `swarm::transport::Placement` that the rerouting driver (`swarm::transport::reroute`)
//! composes with the rerouting automaton.
//!
//! `storage_fetch` sends one `SearchEntry` and `operate_entry` (store, append, tombstone,
//! compact, relay-inbox writes) one `OperateEntry` per remote placement. Each instance supplies
//! the per-placement `Compute`, the same DHT function the whole operation used to plan it
//! ([`PeerRing::lookup_placement`], [`PeerRing::operate_route`]), so a rerouted placement goes
//! where a fresh operation would, and the local settlement a recomputed route may reach. A
//! settlement reached after a deferral is the placement's single effect: the deferred send had
//! none.

use std::sync::Arc;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryLookupKey;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::entry::PlacementMiss;
use crate::dht::ChordStorageCache;
use crate::dht::OperateRoute;
use crate::dht::PeerRing;
use crate::dht::PlacementLookup;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::types::SearchEntry;
use crate::swarm::transport::Placement;
use crate::swarm::transport::Route;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms;

/// One placement of `storage_fetch`: a `SearchEntry` for `query`.
pub(super) struct LookupPlacement {
    /// The placement interrogated.
    pub(super) query: EntryLookupKey,
    /// Redundancy of the lookup round the response must match.
    pub(super) redundancy: u16,
}

/// A lookup placement settled here.
pub(super) enum LookupSettlement {
    /// A live value is stored here.
    Found(Entry),
    /// This node owns the placement and stores nothing for it.
    Missed(PlacementMiss),
}

impl LookupPlacement {
    /// The `SearchEntry` that interrogates `query` for this lookup round.
    pub(super) fn message(&self, query: EntryLookupKey) -> Box<Message> {
        Box::new(Message::SearchEntry(SearchEntry {
            resource: query.resource,
            placement: query.placement,
            redundancy: self.redundancy,
        }))
    }
}

impl Placement for LookupPlacement {
    type Local = LookupSettlement;

    async fn route(&self, dht: &PeerRing) -> Result<Route<LookupSettlement>> {
        Ok(
            match dht
                .lookup_placement(self.query, true, get_epoch_ms())
                .await?
            {
                PlacementLookup::Found(entry) => Route::Local(LookupSettlement::Found(entry)),
                PlacementLookup::Missed(miss) => Route::Local(LookupSettlement::Missed(miss)),
                PlacementLookup::Remote { next, query } => Route::Remote {
                    next,
                    message: self.message(query),
                },
            },
        )
    }

    async fn settle(&self, transport: &Arc<SwarmTransport>, local: LookupSettlement) -> Result<()> {
        match local {
            LookupSettlement::Found(entry) => {
                transport.dht.local_cache_put(entry.clone()).await?;
                super::repair_observed_storage_misses(transport.clone(), entry, self.redundancy)
                    .await
            }
            LookupSettlement::Missed(miss) => {
                transport.observe_storage_misses(self.query.resource, self.redundancy, [miss])
            }
        }
    }
}

/// One placement of `operate_entry`: an `OperateEntry` carrying the stamped operation.
pub(super) struct OperatePlacement(pub(super) PlacedEntryOperation);

impl OperatePlacement {
    /// The `OperateEntry` that carries this placement's operation.
    pub(super) fn message(&self) -> Box<Message> {
        Box::new(Message::OperateEntry(self.0.clone()))
    }
}

impl Placement for OperatePlacement {
    type Local = ();

    async fn route(&self, dht: &PeerRing) -> Result<Route<()>> {
        Ok(
            match dht.operate_route(self.0.placement, self.0.op.kind())? {
                OperateRoute::Owned => Route::Local(()),
                OperateRoute::Remote(next) => Route::Remote {
                    next,
                    message: self.message(),
                },
            },
        )
    }

    async fn settle(&self, transport: &Arc<SwarmTransport>, (): ()) -> Result<()> {
        let dht = &transport.dht;
        dht.operate_storage_entry(get_epoch_ms(), self.0.placement, self.0.op.clone(), dht.did)
            .await
    }
}
