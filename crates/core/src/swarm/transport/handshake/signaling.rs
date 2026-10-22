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
//!
//! Two contexts drive it. The application boundary ([`SwarmTransport::connect`]) creates the
//! offer, waiting on ICE gathering, and sends it under the application discipline. The protocol
//! context ([`SwarmTransport::connect_detached`], [`SwarmTransport::answer_signaled_offer`])
//! reserves the generation and decides the slot at once, so a finger proof waiting for it has an
//! owner, and creates and sends the description on a detached task through the protocol egress.
//!
//! Law (inbound locality): nothing the protocol context runs inline waits on a remote party; a
//! failure of the detached half abandons the generation it reserved.

use std::future::Future;
use std::sync::Arc;

use super::super::SwarmTransport;
use super::CounterOffer;
use super::Handshake;
use super::OfferOwner;
use super::OfferReply;
use super::PendingConnectionAttempt;
use super::PendingTransportConnection;
use crate::dht::Did;
use crate::error::DetachedWork;
use crate::error::Error;
use crate::error::Result;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageRelay;
use crate::message::PayloadSender;
use crate::message::SignalingPath;
use crate::swarm::callback::SwarmTransportCallback;
use crate::swarm::detached::spawn_detached;
use crate::swarm::detached::DetachedTask;

impl SwarmTransport {
    /// Connect a given Did. If the did is already connected, return Err,
    /// else try prepare offer and establish connection by dht.
    ///
    /// The application boundary: the offer is created here, waiting on ICE gathering, and sent
    /// under the application discipline, so this returns once the offer's first frame is
    /// admitted. Only the peer's own handshake taking the slot (`ConnectionAttemptSuperseded`)
    /// or an admitted connection satisfies the request.
    pub(in crate::swarm) async fn connect(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
    ) -> Result<()> {
        let Some(pending) = self.open_offer(peer, callback).await? else {
            return Ok(());
        };
        let attempt = pending.attempt();
        let sent = match self.create_offer(&pending).await {
            Ok(offer) => {
                self.send_connection_offer(attempt, offer, &self.application_egress())
                    .await
            }
            Err(error) => Err(error),
        };
        match sent {
            Ok(_) => Ok(()),
            Err(error) => self.connect_failed(peer, error).await,
        }
    }

    /// Connect from the protocol context: the generation is reserved and its connection created
    /// now, so a finger proof waiting for it has an owner at once; the offer, whose ICE
    /// gathering waits on STUN or TURN servers, is created and queued through the protocol
    /// egress on a detached task (`Law (inbound locality)`).
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
                .send_connection_offer(attempt, offer, &transport.protocol_egress())
                .await
                .map(|_| ())
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
                DetachedWork::ProtocolHandshake,
            ));
        }
        Ok(())
    }

    /// Reserve an overlay generation for `peer` and create its connection, both local. `None`
    /// when a concurrent handshake already satisfied the request.
    async fn open_offer(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
    ) -> Result<Option<PendingTransportConnection>> {
        let handshake = Handshake::offering(OfferOwner::Overlay);
        match self
            .open_pending_connection(peer, callback, handshake)
            .await
        {
            Ok(pending) => Ok(Some(pending)),
            Err(error) => self.connect_failed(peer, error).await.map(|()| None),
        }
    }

    /// The outcome of a connect that did not send its offer: satisfied when the peer's own
    /// handshake took the slot or an admitted connection exists, refused as `AlreadyConnected`,
    /// and otherwise a failure charged to the peer.
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

    /// Send `offer`, created for `attempt`, to its peer over the DHT through `sender` under a
    /// carrier that records its path. The attempt is marked `Sent` under the offer's
    /// transaction before the offer leaves, and must still own its slot when it leaves;
    /// otherwise the error says why. A failure to send abandons the generation.
    async fn send_connection_offer<S>(
        &self,
        attempt: PendingConnectionAttempt,
        offer: ConnectNodeSend,
        sender: &S,
    ) -> Result<uuid::Uuid>
    where
        S: PayloadSender + rings_runtime::MaybeSendSync + ?Sized,
    {
        let peer = attempt.peer;
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            sdp_bytes = offer.sdp.len(),
            "connection offer send start"
        );
        let mut payload = sender
            .originate(Message::ConnectNodeSend(offer.clone()), peer, None)
            .await?;
        payload.relay = payload.relay.with_signaling_path(SignalingPath::outbound());
        let tx_id = payload.transaction.tx_id;
        self.mark_offer_sent(attempt, &offer, tx_id)?;
        let send = async {
            sender.send_payload(payload).await.inspect_err(|error| {
                tracing::warn!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation = attempt.generation,
                    error = ?error,
                    "connection offer send failed"
                );
            })
        };
        self.settle_handshake_step(attempt, "sending connection offer", send)
            .await?;
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            tx_id = %tx_id,
            "connection offer send complete"
        );
        Ok(tx_id)
    }

    /// The carrier of the reply to the connect-signaling `request`: a return along the path
    /// the request recorded, whose first hop must be a link of this node.
    fn signaling_reply_carrier(&self, request: &MessagePayload) -> Result<MessageRelay> {
        let carrier = request
            .relay
            .reply_along_path(self.dht.did, request.transaction.origin())?;
        if self.get_connection(carrier.next_hop).is_some() {
            Ok(carrier)
        } else {
            Err(Error::RelayPathHopUnlinked {
                hop: carrier.next_hop,
            })
        }
    }

    /// Reply, from the protocol context, to the offer carried by `request`: answer it, or send
    /// back this node's prevailing offer, retracing the request's path.
    ///
    /// The reply carrier is decided, and the slot reconciled and the answering generation
    /// opened, before anything waits; so an offer whose path cannot be retraced reserves
    /// nothing. The answer, which waits on ICE gathering, is created and sent through the
    /// protocol egress on a detached task (`Law (inbound locality)`). An answer that cannot be
    /// handed to its carrier abandons its generation, so the peer's next offer is not refused
    /// for the pending timeout.
    pub(crate) async fn answer_signaled_offer(
        self: &Arc<Self>,
        callback: SwarmTransportCallback,
        request: &MessagePayload,
        offer: &ConnectNodeSend,
    ) -> Result<()> {
        let carrier = self.signaling_reply_carrier(request)?;
        let peer = request.transaction.origin();
        match self.open_answer(peer, callback, offer).await? {
            OfferReply::CounterOffer(CounterOffer { offer, tx_id }) => {
                self.protocol_egress()
                    .send_reply(request, carrier, tx_id, Message::ConnectNodeSend(offer))
                    .await
            }
            OfferReply::Answer((pending, offer)) => {
                let attempt = pending.attempt();
                let (transport, request) = (Arc::clone(self), request.clone());
                self.negotiate_detached(attempt, "answer", async move {
                    let report = transport.create_answer(&pending, offer).await?;
                    transport
                        .send_signaled_answer(attempt, &request, carrier, report)
                        .await
                })
                .await
            }
        }
    }

    /// Send the answer of `attempt` to the offer `request` carried along `carrier`; a failure
    /// abandons the answering generation.
    async fn send_signaled_answer(
        &self,
        attempt: PendingConnectionAttempt,
        request: &MessagePayload,
        carrier: MessageRelay,
        report: ConnectNodeReport,
    ) -> Result<()> {
        let tx_id = request.transaction.tx_id;
        let reply = Message::ConnectNodeReport(report);
        self.settle_handshake_step(
            attempt,
            "sending connection answer",
            self.protocol_egress()
                .send_reply(request, carrier, tx_id, reply),
        )
        .await
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
