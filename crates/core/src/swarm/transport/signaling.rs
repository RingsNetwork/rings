//! Connection signaling: offering a connection to a peer, answering a remote offer, and
//! accepting the answer to this end's offer.

use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::WebrtcConnectionState;

use super::pending::AnswerSlot;
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
use crate::message::PayloadSender;
use crate::swarm::callback::InnerSwarmCallback;

impl SwarmTransport {
    /// Connect a given Did. If the did is already connected, return Err,
    /// else try prepare offer and establish connection by dht.
    ///
    /// The offer is sent under the application discipline: this returns once the offer's first
    /// frame is admitted.
    pub async fn connect(&self, peer: Did, callback: InnerSwarmCallback) -> Result<()> {
        self.connect_with(peer, callback, self).await
    }

    /// [`Self::connect`] with the offer sent through `offer_sender`; the protocol context passes
    /// its [`ProtocolEgress`](super::egress::ProtocolEgress), so a connection a handler starts never
    /// waits on the relay that carries its offer.
    pub(crate) async fn connect_with<S>(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
        offer_sender: &S,
    ) -> Result<()>
    where
        S: PayloadSender + rings_runtime::MaybeSendSync + ?Sized,
    {
        let (attempt, offer_msg) = match self
            .prepare_connection_offer_with_attempt(peer, callback)
            .await
        {
            Ok(offer) => offer,
            Err(Error::AlreadyConnected) => return Err(Error::AlreadyConnected),
            Err(e) => {
                if self.get_connection(peer).is_some() {
                    tracing::debug!(
                        target: "rings_core::swarm::transport::handshake",
                        local = %self.dht.did,
                        peer = %peer,
                        error = ?e,
                        "connection request satisfied by concurrent handshake"
                    );
                    return Ok(());
                }
                self.record_peer_message_send_failed(
                    peer,
                    crate::measure::Authentication::LocallyAddressed,
                )
                .await;
                return Err(e);
            }
        };
        let sdp_len = offer_msg.sdp.len();
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            sdp_bytes = sdp_len,
            "connection offer send start"
        );
        match offer_sender
            .send_message(Message::ConnectNodeSend(offer_msg), peer)
            .await
        {
            Ok(tx_id) => {
                tracing::trace!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation = attempt.generation,
                    tx_id = %tx_id,
                    "connection offer send complete"
                );
            }
            Err(error) => {
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
                    tracing::debug!(
                        target: "rings_core::swarm::transport::handshake",
                        local = %self.dht.did,
                        peer = %peer,
                        generation = attempt.generation,
                        error = ?error,
                        "connection offer send failure satisfied by concurrent handshake"
                    );
                    return Ok(());
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Reserve a connection generation for `peer` and produce its offer; the attempt names the
    /// generation so the caller can accept or cancel exactly what it reserved.
    pub(in crate::swarm) async fn prepare_connection_offer_with_attempt(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
    ) -> Result<(PendingConnectionAttempt, ConnectNodeSend)> {
        let attempt = self.reserve_pending_connection(peer).await?;
        let callback = callback.with_pending_connection_attempt(attempt);
        let pending_connection = self.new_pending_connection(attempt, callback).await?;
        let attempt = pending_connection.attempt();
        let conn = pending_connection.connection();

        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            state = ?conn.webrtc_connection_state(),
            "connection offer create start"
        );
        let offer = match conn.connection.webrtc_create_offer().await {
            Ok(offer) => offer,
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation = attempt.generation,
                    error = ?error,
                    "connection offer create failed"
                );
                self.abandon_pending_connection(attempt, "creating connection offer")
                    .await;
                return Err(Error::Transport(error));
            }
        };
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            sdp_bytes = offer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection offer create complete"
        );
        let offer_str = match serde_json::to_string(&offer) {
            Ok(offer) => offer,
            Err(_) => {
                self.abandon_pending_connection(attempt, "serializing connection offer")
                    .await;
                return Err(Error::SerializeToString);
            }
        };
        let offer_msg = ConnectNodeSend {
            sdp: offer_str,
            dht_protocol_mode: self.dht_protocol_mode(),
        };

        Ok((attempt, offer_msg))
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

    async fn create_connection_answer(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
        offer: String,
    ) -> Result<ConnectNodeReport> {
        let attempt = self.reserve_pending_connection(peer).await?;
        let callback = callback.with_pending_connection_attempt(attempt);
        let pending_connection = self.new_pending_connection(attempt, callback).await?;
        let attempt = pending_connection.attempt();
        let conn = pending_connection.connection();

        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            offer_sdp_bytes = offer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection answer create start"
        );
        let answer = match conn.connection.webrtc_answer_offer(offer).await {
            Ok(answer) => answer,
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation = attempt.generation,
                    error = ?error,
                    "connection answer create failed"
                );
                self.abandon_pending_connection(attempt, "creating connection answer")
                    .await;
                return Err(Error::Transport(error));
            }
        };
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            answer_sdp_bytes = answer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection answer create complete"
        );
        let answer_str = match serde_json::to_string(&answer) {
            Ok(answer) => answer,
            Err(_) => {
                self.abandon_pending_connection(attempt, "serializing connection answer")
                    .await;
                return Err(Error::SerializeToString);
            }
        };
        let answer_msg = ConnectNodeReport {
            sdp: answer_str,
            dht_protocol_mode: self.dht_protocol_mode(),
        };

        Ok(answer_msg)
    }

    /// Answer the offer of remote connection.
    pub async fn answer_remote_connection(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<ConnectNodeReport> {
        if !self.accepts_connection_offer(offer_msg) {
            return Err(Error::InvalidMessage(
                "connection offer DHT protocol mismatch".to_string(),
            ));
        }
        let offer: String = serde_json::from_str(&offer_msg.sdp).map_err(Error::Deserialize)?;
        self.reconcile_incoming_offer_peer(peer).await?;
        self.create_connection_answer(peer, callback, offer).await
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
