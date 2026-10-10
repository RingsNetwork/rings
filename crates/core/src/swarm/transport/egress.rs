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
//!   application boundary ── SwarmTransport ──▶ wait for capacity and readiness, then for the
//!   (Swarm API, callers                         first frame's admission: the caller learns
//!    outside the actor)                         whether the payload left
//!
//!   protocol context ────── ProtocolEgress ──▶ capacity and readiness now, or refused as local
//!   (handlers, core effects,                    backpressure; return once queued; the first
//!    maintenance)                               frame's admission is watched apart
//! ```
//!
//! Both are [`PayloadSender`]s and share every derived operation (originate, report, forward);
//! they differ only in [`PayloadSender::do_send_payload`], the one point where a send meets the
//! peer. Code that runs in the protocol context receives a [`ProtocolEgress`], so it cannot
//! reach the waiting discipline through the sender it is given.

use std::sync::Arc;

use async_trait::async_trait;

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

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl PayloadSender for SwarmTransport {
    fn message_signer(&self) -> MessageSigner<&DelegateeKey> {
        MessageSigner::new(&self.delegatee_key, self.network_id)
    }

    fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
    }

    fn is_connected(&self, did: Did) -> bool {
        self.get_connection(did).is_some()
    }

    async fn reserve_transaction_sequences(
        &self,
        destination: Did,
        class: MessageCategory,
        count: std::num::NonZeroU64,
    ) -> Result<std::ops::RangeInclusive<u64>> {
        SwarmTransport::reserve_transaction_sequences(self, destination, class, count).await
    }

    /// The application discipline: return once the first frame is admitted, so the caller
    /// learns whether the payload left.
    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()> {
        self.send_payload_admitted(did, payload).await
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
        self.transport.is_connected(did)
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
        self.transport.send_payload_enqueued(did, payload).await
    }
}
