//! Connection signaling: offering a connection to a peer, answering a remote offer, and
//! accepting the answer to this end's offer.

use std::future::Future;
use std::sync::Arc;

use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::WebrtcConnectionState;

use super::pending::AnswerSlot;
use super::pending::PendingTransportConnection;
use super::pending::RawConnectionOwner;
use super::IncomingOfferAdmittedPeer;
use super::PendingConnectionAttempt;
use super::SwarmTransport;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::swarm::callback::SwarmTransportCallback;
use crate::swarm::detached::spawn_detached;
use crate::swarm::detached::DetachedTask;

impl SwarmTransport {
    /// Connect a given Did. If the did is already connected, return Err,
    /// else try prepare offer and establish connection by dht.
    ///
    /// The application boundary: the offer is created here, waiting on ICE gathering, and sent
    /// under the application discipline, so this returns once the offer's first frame is
    /// admitted.
    pub(crate) async fn connect(&self, peer: Did, callback: SwarmTransportCallback) -> Result<()> {
        let Some(pending) = self.open_offer(peer, callback).await? else {
            return Ok(());
        };
        let attempt = pending.attempt();
        let offer = self.create_offer(&pending).await?;
        self.send_offer(attempt, offer, &self.application_egress())
            .await
    }

    /// Connect from the protocol context (#913 R9 M4): the generation is reserved and its
    /// connection created now, so a finger proof waiting for it has an owner at once; the offer,
    /// whose ICE gathering waits on STUN or TURN servers, is created and queued through the
    /// protocol egress on a task detached from the inbound event that asked for it.
    ///
    /// Law (inbound locality): nothing here waits on a remote party; a failure of the detached
    /// half abandons the generation it reserved, as the application path does.
    pub(crate) async fn connect_detached(
        self: &Arc<Self>,
        peer: Did,
        callback: SwarmTransportCallback,
    ) -> Result<()> {
        let Some(pending) = self.open_offer(peer, callback).await? else {
            return Ok(());
        };
        let attempt = pending.attempt();
        let transport = Arc::clone(self);
        self.negotiate_detached(attempt, "offer", async move {
            let offer = transport.create_offer(&pending).await?;
            transport
                .send_offer(attempt, offer, &transport.protocol_egress())
                .await
        })
        .await
    }

    /// Run the `side` (offer or answer) of `attempt`'s handshake, which waits on ICE gathering
    /// and the next hop, on a detached task, so its caller never waits on a remote party. A
    /// failure there is logged; the runtime refusing the task abandons the generation.
    async fn negotiate_detached(
        &self,
        attempt: PendingConnectionAttempt,
        side: &'static str,
        negotiate: impl Future<Output = Result<()>> + rings_runtime::MaybeSend + 'static,
    ) -> Result<()> {
        let task: DetachedTask = Box::pin(async move {
            if let Err(error) = negotiate.await {
                tracing::debug!(
                    target: "rings_core::swarm::transport::handshake",
                    peer = %attempt.peer,
                    generation = attempt.generation,
                    error = ?error,
                    side,
                    "detached connection negotiation did not leave"
                );
            }
        });
        if spawn_detached(task).is_err() {
            self.abandon_pending_connection(attempt, "scheduling connection negotiation")
                .await;
            return Err(Error::DetachedRuntimeUnavailable(
                "a protocol-context handshake",
            ));
        }
        Ok(())
    }

    /// Reserve a generation for `peer` and create its connection, both local. `None` when a
    /// concurrent handshake already satisfied the request.
    async fn open_offer(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
    ) -> Result<Option<PendingTransportConnection>> {
        match self.open_pending_connection(peer, callback).await {
            Ok(pending) => Ok(Some(pending)),
            Err(Error::AlreadyConnected) => Err(Error::AlreadyConnected),
            Err(error) => {
                if self.get_connection(peer).is_some() {
                    tracing::debug!(
                        target: "rings_core::swarm::transport::handshake",
                        local = %self.dht.did,
                        peer = %peer,
                        error = ?error,
                        "connection request satisfied by concurrent handshake"
                    );
                    return Ok(None);
                }
                self.record_peer_message_send_failed(
                    peer,
                    crate::measure::Authentication::LocallyAddressed,
                )
                .await;
                Err(error)
            }
        }
    }

    /// Send the offer of `attempt` through `offer_sender`; a failure abandons the generation.
    async fn send_offer<S>(
        &self,
        attempt: PendingConnectionAttempt,
        offer_msg: ConnectNodeSend,
        offer_sender: &S,
    ) -> Result<()>
    where
        S: PayloadSender + rings_runtime::MaybeSendSync + ?Sized,
    {
        let peer = attempt.peer;
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            sdp_bytes = offer_msg.sdp.len(),
            "connection offer send start"
        );
        let Err(error) = offer_sender
            .send_message(Message::ConnectNodeSend(offer_msg), peer)
            .await
        else {
            return Ok(());
        };
        tracing::warn!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            error = ?error,
            "connection offer send failed"
        );
        self.abandon_pending_connection(attempt, "sending connection offer")
            .await;
        if self.get_connection(peer).is_some() {
            return Ok(());
        }
        Err(error)
    }

    /// Reserve a connection generation for `peer` and produce its offer; the attempt names the
    /// generation so the caller can accept or cancel exactly what it reserved.
    pub(in crate::swarm) async fn prepare_connection_offer_with_attempt(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
    ) -> Result<(PendingConnectionAttempt, ConnectNodeSend)> {
        let pending = self.open_pending_connection(peer, callback).await?;
        let offer = self.create_offer(&pending).await?;
        Ok((pending.attempt(), offer))
    }

    /// Reserve a generation for `peer` and create its transport connection: local work only.
    async fn open_pending_connection(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
    ) -> Result<PendingTransportConnection> {
        let attempt = self.reserve_pending_connection(peer).await?;
        let callback = callback.with_pending_connection_attempt(attempt);
        self.new_pending_connection(attempt, callback).await
    }

    /// Create the offer of `pending`, which waits on ICE gathering; a failure abandons the
    /// generation.
    async fn create_offer(&self, pending: &PendingTransportConnection) -> Result<ConnectNodeSend> {
        let offer = pending.connection().connection.webrtc_create_offer();
        Ok(ConnectNodeSend {
            sdp: self.negotiate_sdp(pending, "offer", offer).await?,
            dht_protocol_mode: self.dht_protocol_mode(),
        })
    }

    /// Run `negotiate`, which produces the `side` ("offer" or "answer") of `pending` and waits on
    /// ICE gathering, and serialize its description; a failure abandons the generation.
    async fn negotiate_sdp<T, E>(
        &self,
        pending: &PendingTransportConnection,
        side: &'static str,
        negotiate: impl Future<Output = std::result::Result<T, E>>,
    ) -> Result<String>
    where
        T: serde::Serialize,
        E: std::fmt::Debug + Into<Error>,
    {
        let attempt = pending.attempt();
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            state = ?pending.connection().webrtc_connection_state(),
            side,
            "connection description create start"
        );
        let description = match negotiate.await {
            Ok(description) => description,
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %attempt.peer,
                    generation = attempt.generation,
                    error = ?error,
                    side,
                    "connection description create failed"
                );
                self.abandon_pending_connection(attempt, &format!("creating connection {side}"))
                    .await;
                return Err(error.into());
            }
        };
        let Ok(serialized) = serde_json::to_string(&description) else {
            self.abandon_pending_connection(attempt, &format!("serializing connection {side}"))
                .await;
            return Err(Error::SerializeToString);
        };
        Ok(serialized)
    }

    async fn reconcile_incoming_offer_peer(&self, peer: Did) -> Result<()> {
        self.expire_pending_connections().await?;
        match self.incoming_offer_admitted_peer(peer)? {
            IncomingOfferAdmittedPeer::Vacant => {}
            IncomingOfferAdmittedPeer::Routable => return Err(Error::AlreadyConnected),
            IncomingOfferAdmittedPeer::Unroutable(attempt) => {
                if self.disconnect_unavailable(attempt).await?.is_none()
                    && self.has_active_connection(peer)
                {
                    return Err(Error::AlreadyConnected);
                }
            }
        }

        if let Some(swarm_conn) = self.get_raw_connection(peer) {
            // Simultaneous offers use DID order: the larger local DID abandons
            // its pending offer. A raw connection without a lifecycle owner is
            // stale physical state and is removed only by exact identity.
            match self.raw_connection_owner(peer)? {
                RawConnectionOwner::Pending(attempt)
                    if swarm_conn.connection.webrtc_connection_state()
                        == WebrtcConnectionState::New
                        && self.dht.did > peer =>
                {
                    if !self.cancel_unadmitted_connection(attempt).await? {
                        return Err(Error::AlreadyConnected);
                    }
                }
                RawConnectionOwner::Orphan => {
                    if !self
                        .transport
                        .close_connection_if_current(&swarm_conn.connection)
                        .await
                        .map_err(Error::Transport)?
                    {
                        return Err(Error::AlreadyConnected);
                    }
                }
                RawConnectionOwner::Pending(_) | RawConnectionOwner::Owned => {
                    return Err(Error::AlreadyConnected);
                }
            }
        }
        Ok(())
    }

    /// Create the answer of `pending` to `offer`, which waits on ICE gathering; a failure
    /// abandons the generation.
    async fn create_answer(
        &self,
        pending: &PendingTransportConnection,
        offer: String,
    ) -> Result<ConnectNodeReport> {
        let answer = pending.connection().connection.webrtc_answer_offer(offer);
        Ok(ConnectNodeReport {
            sdp: self.negotiate_sdp(pending, "answer", answer).await?,
            dht_protocol_mode: self.dht_protocol_mode(),
        })
    }

    /// Validate `offer_msg` from `peer`, reconcile the peer's slot, and open the answering
    /// generation: local work only. Returns the decoded offer with the opened connection.
    async fn open_answer(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<(PendingTransportConnection, String)> {
        if !self.accepts_connection_offer(offer_msg) {
            return Err(Error::InvalidMessage(
                "connection offer DHT protocol mismatch".to_string(),
            ));
        }
        let offer: String = serde_json::from_str(&offer_msg.sdp).map_err(Error::Deserialize)?;
        self.reconcile_incoming_offer_peer(peer).await?;
        let pending = self.open_pending_connection(peer, callback).await?;
        Ok((pending, offer))
    }

    /// Answer the offer of remote connection, waiting on ICE gathering: the application
    /// boundary.
    pub(crate) async fn answer_remote_connection(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<ConnectNodeReport> {
        let (pending, offer) = self.open_answer(peer, callback, offer_msg).await?;
        self.create_answer(&pending, offer).await
    }

    /// Answer the offer `request` carries from the protocol context (#913 R9 M4): the slot is
    /// reconciled and the answering generation opened now; the answer, which waits on ICE
    /// gathering, is created and reported through the protocol egress on a detached task.
    pub(crate) async fn answer_remote_connection_detached(
        self: &Arc<Self>,
        callback: SwarmTransportCallback,
        request: &MessagePayload,
        offer_msg: &ConnectNodeSend,
    ) -> Result<()> {
        let peer = request.transaction.origin();
        let (pending, offer) = self.open_answer(peer, callback, offer_msg).await?;
        let attempt = pending.attempt();
        let (transport, request) = (Arc::clone(self), request.clone());
        self.negotiate_detached(attempt, "answer", async move {
            let answer = transport.create_answer(&pending, offer).await?;
            transport
                .protocol_egress()
                .send_report_message(&request, Message::ConnectNodeReport(answer))
                .await
        })
        .await
    }

    /// Accept the answer of remote connection.
    ///
    /// With `expected`, the answer is applied only to that generation: a slot owned by another
    /// generation in any phase (the peer's own offer superseded ours, pending, admitting or
    /// already admitted) is refused as `ConnectionAttemptSuperseded` before the transport is
    /// touched; otherwise, when no pending record with a transport object exists (the
    /// generation was cancelled or expired, or is past pending), `SwarmMissTransport`. The
    /// slot is read once, so one history classifies one way.
    pub(crate) async fn accept_remote_connection(
        &self,
        peer: Did,
        answer_msg: &ConnectNodeReport,
        expected: Option<PendingConnectionAttempt>,
    ) -> Result<()> {
        if !self.accepts_connection_answer(answer_msg) {
            return Err(Error::InvalidMessage(
                "connection answer DHT protocol mismatch".to_string(),
            ));
        }

        let answer: String = serde_json::from_str(&answer_msg.sdp).map_err(Error::Deserialize)?;

        let (attempt, conn) = match (expected, self.answer_slot(peer)?) {
            (Some(expected), AnswerSlot::Pending(owner, _) | AnswerSlot::Owned(owner))
                if owner != expected =>
            {
                return Err(Error::ConnectionAttemptSuperseded {
                    peer,
                    generation: expected.generation,
                });
            }
            (_, AnswerSlot::Pending(attempt, conn)) => (attempt, conn),
            (_, AnswerSlot::Vacant | AnswerSlot::Owned(_)) => {
                return Err(Error::SwarmMissTransport(peer));
            }
        };
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            answer_sdp_bytes = answer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection answer accept start"
        );
        if let Err(error) = conn.connection.webrtc_accept_answer(answer).await {
            self.abandon_pending_connection(attempt, "accepting connection answer")
                .await;
            tracing::warn!(
                target: "rings_core::swarm::transport::handshake",
                local = %self.dht.did,
                peer = %peer,
                error = ?error,
                "connection answer accept failed"
            );
            return Err(Error::Transport(error));
        }
        if !self.is_current_connection_attempt(attempt)? {
            return Err(Error::ConnectionAttemptSuperseded {
                peer,
                generation: attempt.generation,
            });
        }
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            state = ?conn.webrtc_connection_state(),
            "connection answer accept complete"
        );

        Ok(())
    }
}
