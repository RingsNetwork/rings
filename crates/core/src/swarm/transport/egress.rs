//! The egress disciplines of a node, named by who may wait for a send.
//!
//! Law (inbound locality): the progress of an inbound lane depends only on local resources:
//! CPU, local locks, and local capacity taken without waiting. It never depends on a remote
//! peer, neither on its credit, nor on its connection's readiness, nor on a delivery or an
//! acknowledgement. An inbound event holds its lane until its handler completes, and the
//! transport holds the credit of every frame queued behind it, so a handler that waited on a
//! third peer would carry that peer's backpressure to the link the event arrived on, and with
//! it the liveness verdict on that link (a peer that withholds credit from this node would make
//! this node's other peers evict it).
//!
//! ```text
//!   application boundary ── application_egress ▶ wait for capacity and readiness, then for
//!   (the Swarm API)                                the first frame's admission: the caller
//!                                                  learns whether the payload left
//!
//!   protocol context ────── protocol_egress ───▶ capacity and readiness now, or refused as
//!   (handlers, core effects,                       local backpressure; return once queued;
//!    maintenance)                                  the first frame's admission is watched
//!                                                  apart
//!
//!   probe answers ───────── probe_answer_egress ▶ as protocol_egress, but a probe takes no
//!   probes ─────────────── Egress::send_probe     shared capacity and an answer none while
//!                                                  its next hop's answer slot is free
//!                                                  (`outbound::sender_bounded`)
//! ```
//!
//! Every sender is an [`Egress`] under a [`Discipline`] type, a [`PayloadSender`]; they share
//! every derived operation (originate, report, forward) and differ only in their discipline's
//! admission, where a send meets the peer. The transport itself is no sender. A protocol sender
//! can be taken anywhere, but the application sender only inside `crate::swarm`, the
//! application boundary, so handlers (`crate::message`) and maintenance (`crate::dht`) cannot
//! reach the waiting discipline through an egress; the handshakes that wait
//! (`SwarmTransport::connect`, `answer_remote_connection`) are likewise visible only there. A
//! probe is sent only through a protocol sender. A handler's storage sync takes its discipline
//! from the egress it holds, so a read repair it starts returns once queued. Two waiting sends
//! remain outside any egress, by design: `send_payload_tracked` and `send_storage_sync_tracked`,
//! called only by maintenance (storage repair), which waits for a transfer's completion inside
//! its own step deadline, never on a lane.

use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::ops::RangeInclusive;
use std::sync::Arc;

use async_trait::async_trait;

use super::delivery::SendCompletionOutcome;
use super::outbound::OutboundCompletion;
use super::outbound::SenderBound;
use super::IssuedProbe;
use super::SwarmTransport;
use crate::delegation::DelegateeKey;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::message::MessageCategory;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::PayloadSender;

/// How a send meets its peer's resources before its payload is queued, and whether its
/// sender then awaits the transfer's completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueueAdmission {
    /// The protocol discipline: the connection must be ready and the capacity free now;
    /// otherwise the send is refused as local backpressure.
    Immediate,
    /// The application discipline: wait, bounded, for the connection's readiness and for
    /// capacity.
    Waiting,
    /// A sender-bounded transfer (`outbound::sender_bounded`): the connection must be ready
    /// now, and the transfer takes no shared capacity while its bound admits it, falling back to
    /// the shared capacity otherwise.
    Bounded(SenderBound),
    /// A tracked send (storage maintenance, inside its own step deadline): wait for capacity as
    /// long as its own deadline, take the connection only if it can make progress now, and
    /// await every frame's completion.
    Tracked,
}

impl QueueAdmission {
    /// The completion policy of a transfer queued under this admission: only a tracked send
    /// awaits its frames; every other returns once queued or once its first frame is admitted.
    pub(super) const fn completion(self) -> OutboundCompletion {
        match self {
            Self::Tracked => OutboundCompletion::Tracked,
            Self::Immediate | Self::Waiting | Self::Bounded(_) => OutboundCompletion::Detached,
        }
    }
}

/// A discipline an [`Egress`] sends under: how a payload meets its next hop, as a type, so
/// what may wait is fixed where the sender is taken, not chosen at each send.
pub(crate) trait Discipline: Copy + Send + Sync + 'static {
    /// How `payload` toward `next_hop` meets its peer's resources.
    fn admission(next_hop: Did, payload: &MessagePayload) -> QueueAdmission;
}

/// The application boundary: a send waits, bounded, for capacity and readiness, then returns
/// once its first frame is admitted, so the caller learns whether the payload left.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Application;

/// The protocol context: capacity and readiness are taken now or refused as local
/// backpressure, and a send returns once queued.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Protocol;

/// A liveness probe's answer: a protocol send that holds its next hop's answer slot instead of
/// shared capacity when it goes straight to the prober it answers (`outbound::sender_bounded`);
/// an answer relayed through a neighbour is an ordinary protocol send, so other origins'
/// probes routed through that neighbour cannot occupy its slot.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProbeAnswer;

impl Discipline for Application {
    fn admission(_next_hop: Did, _payload: &MessagePayload) -> QueueAdmission {
        QueueAdmission::Waiting
    }
}

impl Discipline for Protocol {
    fn admission(_next_hop: Did, _payload: &MessagePayload) -> QueueAdmission {
        QueueAdmission::Immediate
    }
}

impl Discipline for ProbeAnswer {
    fn admission(next_hop: Did, payload: &MessagePayload) -> QueueAdmission {
        if payload.transaction.destination == next_hop {
            QueueAdmission::Bounded(SenderBound::Answer)
        } else {
            QueueAdmission::Immediate
        }
    }
}

/// A sender over the transport under the discipline `D` (see the module law). The derived
/// operations of [`PayloadSender`] are shared; only [`PayloadSender::do_send_payload`], where a
/// send meets the peer, depends on `D`.
#[derive(Clone, Copy)]
pub(crate) struct Egress<'transport, D> {
    transport: &'transport SwarmTransport,
    discipline: PhantomData<D>,
}

impl<'transport, D: Discipline> Egress<'transport, D> {
    /// The sender under `D` over `transport`; private, so only the constructors below choose a
    /// discipline.
    fn over(transport: &'transport SwarmTransport) -> Self {
        Self {
            transport,
            discipline: PhantomData,
        }
    }
}

impl SwarmTransport {
    /// The protocol-context sender over this transport.
    pub(crate) fn protocol_egress(&self) -> Egress<'_, Protocol> {
        Egress::over(self)
    }

    /// The sender of liveness-probe answers.
    pub(crate) fn probe_answer_egress(&self) -> Egress<'_, ProbeAnswer> {
        Egress::over(self)
    }

    /// The application-boundary sender over this transport. Constructible only inside
    /// `crate::swarm`.
    pub(in crate::swarm) fn application_egress(&self) -> Egress<'_, Application> {
        Egress::over(self)
    }

    /// The signer of every payload this transport originates; signing sends nothing.
    pub(crate) fn message_signer(&self) -> MessageSigner<&DelegateeKey> {
        MessageSigner::new(&self.delegatee_key, self.network_id)
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl<D: Discipline> PayloadSender for Egress<'_, D> {
    fn message_signer(&self) -> MessageSigner<&DelegateeKey> {
        self.transport.message_signer()
    }

    fn dht(&self) -> Arc<PeerRing> {
        self.transport.dht.clone()
    }

    fn is_connected(&self, did: Did) -> bool {
        self.transport.get_connection(did).is_some()
    }

    async fn reserve_transaction_sequences(
        &self,
        destination: Did,
        class: MessageCategory,
        count: NonZeroU64,
    ) -> Result<RangeInclusive<u64>> {
        SwarmTransport::reserve_transaction_sequences(self.transport, destination, class, count)
            .await
    }

    /// Meet the peer under this sender's discipline; see [`payload_left`].
    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()> {
        payload_left(self.send_payload_with_outcome(did, payload).await?, did)
    }
}

/// Whether a payload sent to `peer` left, from its send outcome.
///
/// Post: `Ok(())` iff `outcome = Succeeded`; a transfer cancelled before acceptance is
/// `Err(OutboundTransferCancelled)`, never a success, so the caller learns that the payload did
/// not leave and may send it again (#964). Only the waiting discipline produces `Cancelled`.
pub(super) fn payload_left(outcome: SendCompletionOutcome, peer: Did) -> Result<()> {
    match outcome {
        SendCompletionOutcome::Succeeded => Ok(()),
        SendCompletionOutcome::Cancelled => Err(Error::OutboundTransferCancelled { peer }),
    }
}

impl<D: Discipline> Egress<'_, D> {
    /// Send `payload` toward `next_hop` under this discipline and report whether it left: a
    /// waiting send once its first frame is admitted, every other once queued, which is all it
    /// may wait for.
    pub(crate) async fn send_payload_with_outcome(
        &self,
        next_hop: Did,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        match D::admission(next_hop, &payload) {
            QueueAdmission::Waiting => self.transport.send_payload_admitted(payload).await,
            admission => self
                .transport
                .send_payload_enqueued(next_hop, payload, admission, None)
                .await
                .map(|()| SendCompletionOutcome::Succeeded),
        }
    }
}

impl Egress<'_, Protocol> {
    /// Queue an issued liveness probe toward its peer and return, as every protocol send does.
    ///
    /// The probe takes no shared outbound capacity (see `outbound::sender_bounded`), and its
    /// transfer waits for credit as long as the probe's answer deadline, no shorter. Only a
    /// failure before the queue, no ready connection, is returned. Only a protocol sender
    /// sends probes.
    pub(crate) async fn send_probe(&self, probe: IssuedProbe) -> Result<()> {
        let (attempt, payload, deadline) = probe.into_send();
        self.transport
            .send_payload_enqueued(
                attempt.peer,
                payload,
                QueueAdmission::Bounded(SenderBound::Probe {
                    generation: attempt.generation,
                }),
                Some(deadline),
            )
            .await
    }
}

impl SwarmTransport {
    /// Test builds: the application egress, for tests outside `crate::swarm` that stand in for
    /// the application boundary.
    #[cfg(test)]
    pub(crate) fn application_egress_for_test(&self) -> Egress<'_, Application> {
        self.application_egress()
    }
}

#[cfg(test)]
mod tests {
    use super::payload_left;
    use crate::dht::Did;
    use crate::error::DeferralTrigger;
    use crate::error::Error;
    use crate::error::SendClass;
    use crate::swarm::transport::delivery::SendCompletionOutcome;

    /// Law: `payload_left(o, p) = Ok(())` iff `o = Succeeded`; a cancellation is the typed,
    /// retryable `OutboundTransferCancelled` for `p` (#964).
    #[test]
    fn test_a_cancelled_transfer_is_not_reported_as_sent() {
        let peer = Did::from(7_u32);
        assert!(payload_left(SendCompletionOutcome::Succeeded, peer).is_ok());
        let cancelled = payload_left(SendCompletionOutcome::Cancelled, peer);
        assert!(
            matches!(&cancelled, Err(Error::OutboundTransferCancelled { peer: refused }) if *refused == peer),
            "{cancelled:?}"
        );
        assert!(cancelled.is_err_and(|error| matches!(
            error.send_class(),
            SendClass::Deferrable(DeferralTrigger::LinkChange)
        )));
    }
}
