//! The transport shell of the rerouting automaton: its two effects (one send, and the
//! registrations of one wait) and the readings its wake guard evaluates.

use event_listener::EventListener;
use futures::future::select_all;
use serde::Serialize;

use super::Awaiting;
use super::LinkHop;
use super::Observation;
use super::Verdict;
use crate::dht::delivery::NextHop;
use crate::dht::delivery::Origination;
use crate::dht::Did;
use crate::error::DeferralTrigger;
use crate::error::Result;
use crate::message::MessagePayload;
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

    /// The first hop toward `next` now, with the `reply_via` read from the same topology
    /// snapshot: the delivery decision of [`origination`](crate::dht::delivery::origination)
    /// (#873), never an owner lookup, so the hop never passes `next`.
    ///
    /// When no route leaves this node (no linked peer lies toward `next`), the hop is `next`
    /// itself. That send is refused before acceptance with `SwarmMissDidInTable`, a
    /// `LinkChange` deferral, so the placement waits for a link or topology change instead of
    /// failing: the same wait as a hop whose generation died (L1), and still no hop past `next`.
    fn first_hop(&self, next: Did) -> Result<(NextHop, Origination)> {
        let origination = self.dht.origination(next, |peer| self.is_connected(peer))?;
        let hop = origination.hop.unwrap_or_else(|| NextHop::toward(next));
        Ok((hop, origination))
    }

    /// The link state of the peer `hop` names: its sendable generation, and whether that
    /// generation can make progress.
    fn link_of(&self, hop: Did) -> Result<LinkHop> {
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

    /// The link hop toward `next` now: the first hop [`Self::first_hop`] decides, its sendable
    /// generation, and whether that generation can make progress.
    pub(super) fn link_hop(&self, next: Did) -> Result<LinkHop> {
        let (hop, _) = self.first_hop(next)?;
        self.link_of(hop.peer)
    }

    /// Send `message` to `destination` through the first hop [`Self::first_hop`] decides,
    /// detached, and classify the outcome.
    ///
    /// The hop, its link and the payload's `reply_via` come from one decision, so the verdict
    /// is attributed to the hop the payload actually left through. Unlike
    /// `PayloadSender::send_message`, a `Cancelled` completion is not reported as a success: it
    /// is the pre-acceptance deferral the cancellation gate proves it to be.
    pub(super) async fn attempt_remote<T>(&self, message: T, destination: Did) -> Verdict
    where T: Serialize + Send {
        let (hop, origination) = match self.first_hop(destination) {
            Ok(first) => first,
            Err(error) => return Verdict::Failed(error),
        };
        let LinkHop { generation, .. } = match self.link_of(hop.peer) {
            Ok(link) => link,
            Err(error) => return Verdict::Failed(error),
        };
        let payload = match self
            .originated_payload(message, hop, destination, origination.reply_via)
            .await
        {
            Ok(payload) => payload,
            Err(error) => return Verdict::Failed(error),
        };
        let demand = match Self::payload_demand(&payload) {
            Ok(demand) => demand,
            Err(error) => return Verdict::Failed(error),
        };
        let result = self.send_payload_detached_with_outcome(payload).await;
        Verdict::remote(hop.peer, generation, demand, result)
    }

    /// Build a locally originated payload through `hop`, naming `reply_via`, after durably
    /// reserving its stream sequence: `PayloadSender::originate` with the hop already decided.
    async fn originated_payload<T>(
        &self,
        message: T,
        hop: NextHop,
        destination: Did,
        reply_via: Option<Did>,
    ) -> Result<MessagePayload>
    where
        T: Serialize + Send,
    {
        let sequence = *self
            .reserve_transaction_sequences(destination, std::num::NonZeroU64::MIN)
            .await?
            .start();
        MessagePayload::new_send_with_sequence(
            message,
            self.message_signer(),
            hop,
            destination,
            sequence,
            reply_via,
        )
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
