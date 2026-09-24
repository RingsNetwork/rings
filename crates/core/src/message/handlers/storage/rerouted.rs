//! The remote placements of user DHT operations, each driven by the rerouting automaton.
//!
//! `storage_fetch` sends one `SearchEntry` and `operate_entry` (store, append, tombstone,
//! compact, relay-inbox writes) one `OperateEntry` per remote placement. Each of those sends is
//! a [`Placement`]: an instance supplies the per-placement `Compute` (the same DHT function the
//! whole operation used to plan it) and the local settlement a recomputed route may reach, and
//! [`reroute`] composes them with `swarm::transport::rerouting`. An operation drives its
//! placements concurrently, so one waiting for its trigger holds no other:
//!
//! ```text
//! reroute(P, first):
//!   R ← start ; route ← first
//!   loop
//!     stamp ← capacity epoch
//!     verdict ← route = Local(l)        ⇒ Verdict::local(P.settle(l))
//!               route = Remote(next, m) ⇒ attempt_remote(m, next)
//!     case δ(R, stamp, verdict) of
//!       Complete  ⇒ return Ok
//!       Fail(e)   ⇒ return Err(e)                \* fatal, ambiguous, or exhausted
//!       Await(A)  ⇒ route ← first route r computed after listening with
//!                           A.is_triggered(r, observation(A))
//!                   R ← A.resume
//! ```
//!
//! S1–S3 and L1 are the laws of `rerouting`; this module adds only that `Compute` is the
//! operation's own per-placement function ([`PeerRing::lookup_placement`],
//! [`PeerRing::operate_route`]), so a rerouted placement goes where a fresh operation would.
//! A settlement reached after a deferral is the placement's single effect: the deferred send
//! had none.

use std::sync::Arc;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryLookupKey;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::entry::PlacementMiss;
use crate::dht::ChordStorageCache;
use crate::dht::Did;
use crate::dht::OperateRoute;
use crate::dht::PeerRing;
use crate::dht::PlacementLookup;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::types::SearchEntry;
use crate::swarm::transport::LinkRoute;
use crate::swarm::transport::Rerouting;
use crate::swarm::transport::Step;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::transport::Verdict;
use crate::utils::get_epoch_ms;

/// Where one placement goes under the topology now.
pub(super) enum Route<Local> {
    /// This node settles the placement with `Local`.
    Local(Local),
    /// Send `message` toward `next`.
    Remote {
        /// The node the message is addressed to.
        next: Did,
        /// The placement's message under this route, boxed so a route is small beside a local
        /// settlement (serialization is transparent through the box).
        message: Box<Message>,
    },
}

/// One remote placement of a user DHT operation: the unit that reroutes.
pub(super) trait Placement {
    /// What a placement settled here carries.
    type Local;

    /// `Compute`: route this placement under the topology (and local storage) now.
    async fn route(&self, dht: &PeerRing) -> Result<Route<Self::Local>>;

    /// Settle a placement whose route is local.
    async fn settle(&self, transport: &Arc<SwarmTransport>, local: Self::Local) -> Result<()>;
}

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

/// Drive `placement` from its planned `first` route to completion (module flowchart).
///
/// Post: `Ok(())` iff one attempt was accepted or settled locally; every earlier attempt was
/// refused before acceptance (S1). `Err(ReroutingExhausted { .. })` after
/// `REROUTING_BUDGET + 1` refused sends (S3).
pub(super) async fn reroute<P: Placement>(
    transport: &Arc<SwarmTransport>,
    placement: &P,
    first: Route<P::Local>,
) -> Result<()> {
    let mut rerouting = Rerouting::start();
    let mut route = first;
    loop {
        let stamp = transport.capacity_stamp();
        let verdict = match route {
            Route::Local(local) => Verdict::local(placement.settle(transport, local).await),
            Route::Remote { next, message } => transport.attempt_remote(message, next).await,
        };
        let awaiting = match rerouting.after(stamp, verdict) {
            Step::Complete => return Ok(()),
            Step::Fail(error) => return Err(error),
            Step::Await(awaiting) => awaiting,
        };
        route = loop {
            let listeners = transport.rerouting_listeners(&awaiting);
            let fresh = placement.route(&transport.dht).await?;
            let link = match &fresh {
                Route::Local(_) => LinkRoute::Local,
                Route::Remote { next, .. } => LinkRoute::Remote(transport.link_hop(*next)?),
            };
            if awaiting.is_triggered(link, transport.observation(&awaiting)) {
                break fresh;
            }
            listeners.notified().await;
        };
        rerouting = awaiting.resume();
    }
}
