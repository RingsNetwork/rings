//! The DHT signaling of the handshake (#947).
//!
//! An overlay offer leaves under a carrier that records its path ([`SignalingPath`]); every
//! reply, an answer or a counter-offer, retraces the path of the message it replies to, each hop
//! a link that message just crossed. An answer echoes the transaction its offer left under and is
//! applied only to the generation that sent that offer, so a late answer never reaches a newer
//! generation.
//!
//! ```text
//! A ──offer(tx_a), path rec.──▶ B        B answers:   B ──answer(tx_a), retrace──▶ A
//!                                        B prevails:  B ──offer_B(tx_b), retrace──▶ A
//!                                                     A ──answer(tx_b), retrace──▶ B
//! ```

use super::super::SwarmTransport;
use super::CounterOffer;
use super::CreatedAnswer;
use super::OfferReply;
use super::PendingConnectionAttempt;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageRelay;
use crate::message::PayloadSender;
use crate::message::SignalingPath;
use crate::swarm::callback::InnerSwarmCallback;

impl SwarmTransport {
    /// Connect a given Did. If the did is already connected, return Err,
    /// else try prepare offer and establish connection by dht.
    ///
    /// The offer travels under a carrier that records its path, so the answer, or the
    /// counter-offer of a peer whose own offer prevails, retraces it. Only the peer's own
    /// handshake taking the slot (`ConnectionAttemptSuperseded`) satisfies the request.
    pub async fn connect(&self, peer: Did, callback: InnerSwarmCallback) -> Result<()> {
        let (attempt, offer) = match self
            .prepare_connection_offer_with_attempt(peer, callback)
            .await
        {
            Ok(offer) => offer,
            Err(error) => return self.connect_failed(peer, error).await,
        };
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            sdp_bytes = offer.sdp.len(),
            "connection offer send start"
        );
        match self.send_connection_offer(attempt, offer, peer).await {
            Ok(tx_id) => {
                tracing::trace!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation = attempt.generation,
                    tx_id = %tx_id,
                    "connection offer send complete"
                );
                Ok(())
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
                self.connect_failed(peer, error).await
            }
        }
    }

    /// The outcome of a [`Self::connect`] that did not send its offer: satisfied when the
    /// peer's own handshake took the slot or an admitted connection exists, refused as
    /// `AlreadyConnected`, and otherwise a failure charged to the peer.
    async fn connect_failed(&self, peer: Did, error: Error) -> Result<()> {
        match error {
            Error::AlreadyConnected => Err(Error::AlreadyConnected),
            Error::ConnectionAttemptSuperseded { generation, .. } => {
                tracing::debug!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation,
                    "connection request satisfied by the peer's handshake"
                );
                Ok(())
            }
            error if self.get_connection(peer).is_some() => {
                tracing::debug!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    error = ?error,
                    "connection request satisfied by an admitted connection"
                );
                Ok(())
            }
            error => {
                self.record_peer_message_send_failed(
                    peer,
                    crate::measure::Authentication::LocallyAddressed,
                )
                .await;
                Err(error)
            }
        }
    }

    /// Send `offer`, created for `attempt`, to `peer` over the DHT under a carrier that records
    /// its path. The attempt is marked `Sent` under the offer's transaction before the offer
    /// leaves, and must still own its slot when it leaves; otherwise the error says why.
    async fn send_connection_offer(
        &self,
        attempt: PendingConnectionAttempt,
        offer: ConnectNodeSend,
        peer: Did,
    ) -> Result<uuid::Uuid> {
        let mut payload = self
            .originate(Message::ConnectNodeSend(offer.clone()), peer, None)
            .await?;
        payload.relay = payload.relay.with_signaling_path(SignalingPath::outbound());
        let tx_id = payload.transaction.tx_id;
        self.mark_offer_sent(attempt, &offer, tx_id)?;
        if !self.is_current_connection_attempt(attempt)? {
            return Err(self.lost_slot(attempt)?);
        }
        self.send_payload(payload).await?;
        Ok(tx_id)
    }

    /// The carrier of the reply to the connect-signaling `request`: a return along the path
    /// the request recorded, whose first hop must be a link of this node.
    fn signaling_reply_carrier(&self, request: &MessagePayload) -> Result<MessageRelay> {
        let carrier = request
            .relay
            .reply_along_path(self.dht.did, request.transaction.origin())?;
        if self.is_connected(carrier.next_hop) {
            Ok(carrier)
        } else {
            Err(Error::RelayPathHopUnlinked {
                hop: carrier.next_hop,
            })
        }
    }

    /// Reply to the offer carried by `request`: answer it, or send back this node's prevailing
    /// offer, retracing the request's path.
    ///
    /// The reply carrier is decided before any generation is reserved, so an offer whose path
    /// cannot be retraced reserves nothing. An answer that cannot be handed to its carrier
    /// abandons its generation, so the peer's next offer is not refused for the pending
    /// timeout; a delivered one marks its generation `Answering(reply_sent)`.
    pub(crate) async fn answer_signaled_offer(
        &self,
        request: &MessagePayload,
        offer: &ConnectNodeSend,
        callback: InnerSwarmCallback,
    ) -> Result<()> {
        let carrier = self.signaling_reply_carrier(request)?;
        let peer = request.transaction.origin();
        match self.answer_remote_connection(peer, callback, offer).await? {
            OfferReply::Answer(CreatedAnswer { attempt, report }) => {
                let tx_id = request.transaction.tx_id;
                let reply = Message::ConnectNodeReport(report);
                match self.send_reply(request, carrier, tx_id, reply).await {
                    Ok(()) => self.mark_reply_sent(attempt).map(|_| ()),
                    Err(error) => {
                        self.abandon_pending_connection(attempt, "sending connection answer")
                            .await;
                        Err(error)
                    }
                }
            }
            OfferReply::CounterOffer(CounterOffer { offer, tx_id }) => {
                let reply = Message::ConnectNodeSend(offer);
                self.send_reply(request, carrier, tx_id, reply).await
            }
        }
    }

    /// Accept the answer `answer` from `peer` received over the DHT under transaction `tx_id`.
    /// It applies only to the generation whose sent offer left under `tx_id`; `false` when no
    /// such generation is pending (the offer expired, was cancelled or was already answered),
    /// in which case nothing is touched.
    pub(crate) async fn accept_signaled_answer(
        &self,
        peer: Did,
        tx_id: uuid::Uuid,
        answer: &ConnectNodeReport,
    ) -> Result<bool> {
        let bound = self.with_connection_lifecycle(|| {
            Ok(self.peer_lifecycles()?.offer_sent_under(peer, tx_id))
        })?;
        let Some(attempt) = bound else {
            return Ok(false);
        };
        self.accept_remote_connection(peer, answer, Some(attempt), Some(tx_id))
            .await
            .map(|()| true)
    }
}
