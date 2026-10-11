//! The two egress disciplines of a node, named by who may wait for a send.
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
//!   application boundary ── ApplicationEgress ─▶ wait for capacity and readiness, then for
//!   (the Swarm API)                                the first frame's admission: the caller
//!                                                  learns whether the payload left
//!
//!   protocol context ────── ProtocolEgress ────▶ capacity and readiness now, or refused as
//!   (handlers, core effects,                       local backpressure; return once queued;
//!    maintenance)                                  the first frame's admission is watched
//!                                                  apart
//! ```
//!
//! Both are [`PayloadSender`]s and share every derived operation (originate, report, forward);
//! they differ only in [`PayloadSender::do_send_payload`], the one point where a send meets the
//! peer. The transport itself is no sender (#913 R9 M1): a [`ProtocolEgress`] can be taken
//! anywhere, but an [`ApplicationEgress`] only inside `crate::swarm`, the application boundary.
//! Handlers (`crate::message`) and maintenance (`crate::dht`) therefore cannot reach the
//! waiting discipline at all; the compiler, not a convention, keeps them local.

use std::sync::Arc;

use async_trait::async_trait;

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

/// Whether a sender may wait on a resource before its payload is queued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueueAdmission {
    /// The protocol discipline: the connection must be ready and the capacity free now;
    /// otherwise the send is refused as local backpressure.
    Immediate,
    /// The application discipline: wait, bounded, for the connection's readiness and for
    /// capacity.
    Waiting,
    /// The probe discipline: the connection must be ready now, and the transfer takes no
    /// shared capacity, because one outstanding probe per peer bounds it. Reached only through
    /// [`ProtocolEgress::send_probe`], whose [`IssuedProbe`] witnesses that bound.
    Reserved,
}

/// The sender of the protocol context: every send returns once its payload is queued, and
/// nothing in it waits on a remote peer (see the module law).
#[derive(Clone, Copy)]
pub(crate) struct ProtocolEgress<'transport> {
    transport: &'transport SwarmTransport,
}

impl SwarmTransport {
    /// The protocol-context sender over this transport.
    pub(crate) fn protocol_egress(&self) -> ProtocolEgress<'_> {
        ProtocolEgress { transport: self }
    }
}

/// The sender of the application boundary: a send returns once its first frame is admitted,
/// waiting, bounded, for capacity and readiness, so the caller learns whether the payload left.
/// Constructible only inside `crate::swarm`.
#[derive(Clone, Copy)]
pub(crate) struct ApplicationEgress<'transport> {
    transport: &'transport SwarmTransport,
}

impl SwarmTransport {
    /// The application-boundary sender over this transport; see the module law.
    pub(in crate::swarm) fn application_egress(&self) -> ApplicationEgress<'_> {
        ApplicationEgress { transport: self }
    }

    /// The signer of every payload this transport originates; signing sends nothing.
    pub(crate) fn message_signer(&self) -> MessageSigner<&DelegateeKey> {
        MessageSigner::new(&self.delegatee_key, self.network_id)
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl PayloadSender for ApplicationEgress<'_> {
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

    /// The application discipline: return once the first frame is admitted, so the caller
    /// learns whether the payload left.
    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()> {
        self.transport.send_payload_admitted(did, payload).await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl PayloadSender for ProtocolEgress<'_> {
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

    /// The protocol discipline: return once the payload is queued
    /// ([`SwarmTransport::send_payload_enqueued`]).
    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()> {
        self.transport
            .send_payload_enqueued(did, payload, QueueAdmission::Immediate)
            .await
    }
}

impl ProtocolEgress<'_> {
    /// Queue an issued liveness probe toward its peer and return, as every protocol send does.
    ///
    /// The probe takes no shared outbound capacity (see [`QueueAdmission::Reserved`]), so
    /// other transfers waiting on a peer that withholds credit cannot refuse the probe that
    /// judges the peer. Only a failure before the queue, no ready connection, is returned.
    pub(crate) async fn send_probe(&self, probe: IssuedProbe) -> Result<()> {
        let peer = probe.attempt.peer();
        self.transport
            .send_payload_enqueued(peer, probe.payload, QueueAdmission::Reserved)
            .await
    }
}
