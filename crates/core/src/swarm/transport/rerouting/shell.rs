//! The transport shell of the rerouting automaton: its two effects (one send, and the
//! registrations of one wait) and the readings its wake guard evaluates.

use event_listener::EventListener;
use futures::future::select_all;
use serde::Serialize;

use super::Awaiting;
use super::LinkHop;
use super::Observation;
use super::Verdict;
use crate::dht::Did;
use crate::error::DeferralTrigger;
use crate::error::Result;
use crate::message::PayloadSender;
use crate::swarm::transport::outbound::CapacityView;
use crate::swarm::transport::outbound::PeerStamp;
use crate::swarm::transport::SwarmTransport;

impl SwarmTransport {
    /// Record that a transport callback observed a connection or data-channel state change.
    ///
    /// An active generation recovering from `Disconnected` has no lifecycle transition, so its
    /// callback is the only event that can make a waiting hop usable again.
    pub(crate) fn signal_link_transition(&self) {
        self.connection_lifecycle.link_transitions().advance();
    }

    /// The link hop toward `next` now: the peer `infer_next_hop` binds, its sendable
    /// generation, and whether that generation can make progress.
    pub(super) fn link_hop(&self, next: Did) -> Result<LinkHop> {
        let hop = self.infer_next_hop(next, None)?;
        let admitted = self.admitted_send_connection(hop)?;
        Ok(LinkHop {
            hop,
            generation: admitted
                .as_ref()
                .map(|admitted| admitted.attempt().generation()),
            usable: admitted
                .is_some_and(|admitted| admitted.connection().readiness().can_make_progress()),
        })
    }

    /// Send `message` to `destination` through the hop `infer_next_hop` binds, detached, and
    /// classify the outcome.
    ///
    /// Unlike `PayloadSender::send_message`, a `Cancelled` completion is not reported as a
    /// success: it is the pre-acceptance deferral the cancellation gate proves it to be.
    pub(super) async fn attempt_remote<T>(&self, message: T, destination: Did) -> Verdict
    where T: Serialize + Send {
        let LinkHop {
            hop, generation, ..
        } = match self.link_hop(destination) {
            Ok(link) => link,
            Err(error) => return Verdict::Failed(error),
        };
        let payload = match self.signed_payload(message, hop, destination).await {
            Ok(payload) => payload,
            Err(error) => return Verdict::Failed(error),
        };
        let demand = match Self::payload_demand(&payload) {
            Ok(demand) => demand,
            Err(error) => return Verdict::Failed(error),
        };
        let result = self.send_payload_detached_with_outcome(payload).await;
        Verdict::remote(hop, generation, demand, result)
    }

    /// Stamp the channel progress of `awaiting`'s hop, after its refusal was published.
    pub(super) fn peer_stamp(&self, awaiting: &Awaiting) -> PeerStamp {
        self.outbound_schedulers.peer_stamp(awaiting.hop)
    }

    /// Read the capacity of `awaiting`'s hop once, for one wait iteration's listeners and
    /// observation.
    pub(super) fn capacity_view(&self, awaiting: &Awaiting) -> CapacityView<'_> {
        self.outbound_schedulers.capacity_view(awaiting.hop)
    }

    /// Test hook: the rerouted placements waiting for a link event (`LinkChange` or
    /// `ChannelDrain`): only a waiting placement listens on the link epoch.
    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn link_waiters_for_test(&self) -> usize {
        self.connection_lifecycle.link_transitions().listeners()
    }

    /// Register for the next event of every class that can trigger `awaiting`.
    ///
    /// ```text
    /// LinkChange      ─▶ topology, link
    /// CapacityRelease ─▶ topology, and the `room_listeners` of `capacity`
    /// ChannelDrain    ─▶ topology, link, and the hop's `peer` progress and releases
    /// ```
    ///
    /// A peer whose stamped capacity is gone has no channel listener: its progress already
    /// reads drained and idle.
    ///
    /// Pre: the caller evaluates [`Awaiting::is_triggered`] *after* this call, on an
    /// [`observation`] of the same `capacity`, and awaits [`ReroutingListeners::notified`] only
    /// when it is false, so no event is lost (`Law (Wake)` of `Epoch`).
    pub(super) fn rerouting_listeners(
        &self,
        awaiting: &Awaiting,
        peer: &PeerStamp,
        capacity: &CapacityView<'_>,
    ) -> ReroutingListeners {
        let mut listeners = vec![self.dht.topology_epoch().listen()];
        let link = || self.connection_lifecycle.link_transitions().listen();
        match awaiting.cause.trigger() {
            DeferralTrigger::LinkChange => listeners.push(link()),
            DeferralTrigger::CapacityRelease => listeners.extend(capacity.room_listeners()),
            DeferralTrigger::ChannelDrain => {
                listeners.push(link());
                listeners.extend(peer.listen());
            }
        }
        ReroutingListeners(listeners)
    }
}

/// The observation the wake guard of `awaiting` reads on `capacity`, against the hop's `peer`
/// stamp.
pub(super) fn observation(
    awaiting: &Awaiting,
    peer: &PeerStamp,
    capacity: &CapacityView<'_>,
) -> Observation {
    Observation {
        room: capacity.has_room(awaiting.demand),
        peer: peer.progress(),
    }
}

/// One registration on the epochs a waiting placement's trigger reads.
pub(super) struct ReroutingListeners(Vec<EventListener>);

impl ReroutingListeners {
    /// Resolve at the first event of any listened class. No duration bounds this wait: it ends
    /// by an event (L1's fairness), or at the caller's stop.
    pub(super) async fn notified(self) {
        select_all(self.0).await;
    }
}
