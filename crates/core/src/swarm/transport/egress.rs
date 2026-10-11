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
//! Every sender is an [`Egress`], a [`PayloadSender`]; they share every derived operation
//! (originate, report, forward) and differ only in [`PayloadSender::do_send_payload`], the one
//! point where a send meets the peer. The transport itself is no sender: a protocol sender can
//! be taken anywhere, but the application sender only inside `crate::swarm`, the application
//! boundary, so handlers (`crate::message`) and maintenance (`crate::dht`) cannot reach the
//! waiting discipline through an egress. A handler's storage sync takes its discipline from the
//! egress it holds (`SwarmTransport::send_storage_sync_or_defer`), so a read repair it starts
//! returns once queued. Two waiting sends remain outside any egress, by design:
//! `send_payload_tracked` and `send_storage_sync_tracked`, called only by maintenance (storage
//! repair), which waits for a transfer's completion inside its own step deadline, never on a
//! lane.

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
use crate::error::Result;
use crate::message::MessageCategory;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::PayloadSender;

/// How a send meets its peer's resources before its payload is queued, and whether its
/// sender then awaits the transfer's completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueueAdmission {
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

/// Which discipline an [`Egress`] sends under; private, so only the constructors choose it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Discipline {
    /// Return once the first frame is admitted: the application boundary.
    Application,
    /// Return once queued, taking capacity now: the protocol context.
    Protocol,
    /// A probe's answer: return once queued, in the answer slot of its next hop when free.
    ProbeAnswer,
}

/// A sender over the transport under one discipline (see the module law). The derived
/// operations of [`PayloadSender`] are shared; only [`PayloadSender::do_send_payload`], where a
/// send meets the peer, differs.
#[derive(Clone, Copy)]
pub(crate) struct Egress<'transport> {
    transport: &'transport SwarmTransport,
    discipline: Discipline,
}

impl SwarmTransport {
    /// The protocol-context sender over this transport.
    pub(crate) fn protocol_egress(&self) -> Egress<'_> {
        Egress {
            transport: self,
            discipline: Discipline::Protocol,
        }
    }

    /// The sender of liveness-probe answers: a protocol sender whose transfer holds its next
    /// hop's answer slot instead of shared capacity (#913 R10 M6).
    pub(crate) fn probe_answer_egress(&self) -> Egress<'_> {
        Egress {
            transport: self,
            discipline: Discipline::ProbeAnswer,
        }
    }

    /// The application-boundary sender over this transport: a send returns once its first
    /// frame is admitted, waiting, bounded, for capacity and readiness, so the caller learns
    /// whether the payload left. Constructible only inside `crate::swarm`.
    pub(in crate::swarm) fn application_egress(&self) -> Egress<'_> {
        Egress {
            transport: self,
            discipline: Discipline::Application,
        }
    }

    /// The signer of every payload this transport originates; signing sends nothing.
    pub(crate) fn message_signer(&self) -> MessageSigner<&DelegateeKey> {
        MessageSigner::new(&self.delegatee_key, self.network_id)
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl PayloadSender for Egress<'_> {
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
        count: std::num::NonZeroU64,
    ) -> Result<std::ops::RangeInclusive<u64>> {
        SwarmTransport::reserve_transaction_sequences(self.transport, destination, class, count)
            .await
    }

    /// Meet the peer under this sender's discipline.
    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()> {
        let admission = match self.discipline {
            Discipline::Application => {
                return self
                    .transport
                    .send_payload_admitted(payload)
                    .await
                    .map(|_| ())
            }
            Discipline::Protocol => QueueAdmission::Immediate,
            Discipline::ProbeAnswer => QueueAdmission::Bounded(SenderBound::Answer(did)),
        };
        self.transport
            .send_payload_enqueued(did, payload, admission, None)
            .await
    }
}

impl Egress<'_> {
    /// Queue an issued liveness probe toward its peer and return, as every protocol send does.
    ///
    /// The probe takes no shared outbound capacity (see `outbound::sender_bounded`), so other
    /// transfers waiting on a peer that withholds credit cannot refuse the probe that judges
    /// the peer, and its transfer waits for credit as long as the probe's answer deadline, no
    /// shorter (#913 R10 M5). Only a failure before the queue, no ready connection, is returned.
    pub(crate) async fn send_probe(&self, probe: IssuedProbe) -> Result<()> {
        let (peer, payload, deadline) = probe.into_send();
        self.transport
            .send_payload_enqueued(
                peer,
                payload,
                QueueAdmission::Bounded(SenderBound::Probe),
                Some(deadline),
            )
            .await
    }

    /// Send `payload` toward its next hop under this discipline and report whether it left: the
    /// application discipline once its first frame is admitted, every other once queued, which
    /// is all it may wait for.
    pub(crate) async fn send_payload_with_outcome(
        &self,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        match self.discipline {
            Discipline::Application => self.transport.send_payload_admitted(payload).await,
            Discipline::Protocol | Discipline::ProbeAnswer => self
                .send_payload(payload)
                .await
                .map(|()| SendCompletionOutcome::Succeeded),
        }
    }

    /// Whether this sender returns once a payload is queued, before its first frame leaves: no
    /// outcome after the queue can reach it.
    pub(crate) fn returns_once_queued(&self) -> bool {
        self.discipline != Discipline::Application
    }
}

impl SwarmTransport {
    /// Test builds: the application egress, for tests outside `crate::swarm` that stand in for
    /// the application boundary.
    #[cfg(test)]
    pub(crate) fn application_egress_for_test(&self) -> Egress<'_> {
        self.application_egress()
    }
}
