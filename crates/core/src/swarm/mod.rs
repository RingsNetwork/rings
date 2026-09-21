#![deny(missing_docs)]

//! This mod is the main entrance of swarm.

mod builder;
/// Callback interface for swarm
pub mod callback;
mod detached;
mod inbox;
pub(crate) mod session_link;
pub(crate) mod transport;

use std::num::NonZeroUsize;
use std::sync::Arc;

pub use builder::SwarmBuilder;

use self::callback::InnerSwarmCallback;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::Stabilizer;
use crate::ecc::PublicKey;
use crate::ecc::VerificationPublicKey;
use crate::error::Error;
use crate::error::Result;
use crate::inspect::ConnectionInspect;
use crate::inspect::SwarmInspect;
use crate::measure::PeerMeasurement;
use crate::measure::PeerMeasurementPage;
use crate::message::DhtProtocolMode;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorSend;
use crate::message::FindSuccessorThen;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::OriginQuotaCounters;
use crate::message::OriginQuotaLane;
use crate::message::PayloadSender;
use crate::message::ReplayCounters;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::inbox::SwarmInboxDelivery;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;

/// How a successor lookup was decided: from the local topology, or routed under a
/// transaction id whose report answers it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuccessorLookup {
    /// The successor of the key as the local topology knows it.
    Local(Did),
    /// The lookup was routed through the hop the local step chose; its report returns under
    /// this transaction id.
    Routed(uuid::Uuid),
}

/// An opaque handle to one connection generation this node reserved by offering; accepted or
/// cancelled only through the swarm that issued it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionAttempt(PendingConnectionAttempt);

impl ConnectionAttempt {
    /// The peer this generation was reserved for.
    pub fn peer(&self) -> Did {
        self.0.peer()
    }
}

/// The transport and dht management.
pub struct Swarm {
    /// Reference of DHT.
    pub(crate) dht: Arc<PeerRing>,
    /// Swarm transport.
    pub(crate) transport: Arc<SwarmTransport>,
}

impl Swarm {
    /// Get did of self.
    pub fn did(&self) -> Did {
        self.dht.did
    }

    /// Get the local account public key used for E2E public-key negotiation.
    pub fn account_pubkey(&self) -> Result<PublicKey<33>> {
        self.transport.session().account_pubkey()
    }

    /// Get the typed account verification public key.
    pub fn account_verification_pubkey(&self) -> Result<VerificationPublicKey> {
        self.transport.session().account_verification_pubkey()
    }

    /// Get this swarm's network id.
    pub fn network_id(&self) -> u32 {
        self.transport.network_id
    }

    /// Get the storage redundancy for this swarm's DHT protocol mode.
    pub fn storage_redundancy(&self) -> u16 {
        self.transport.storage_redundancy()
    }

    /// Get the storage virtual-node positions for this swarm's DHT protocol mode.
    pub fn dht_virtual_nodes(&self) -> u16 {
        self.transport.dht_virtual_nodes()
    }

    /// Get this swarm's full DHT protocol mode descriptor.
    pub fn dht_protocol_mode(&self) -> DhtProtocolMode {
        self.transport.dht_protocol_mode()
    }

    /// Get DHT(Distributed Hash Table) of self.
    pub fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
    }

    fn callback(&self) -> Result<SharedSwarmCallback> {
        self.transport.callback_slot().current()
    }

    fn inner_callback(&self) -> Result<InnerSwarmCallback> {
        Ok(InnerSwarmCallback::new(
            self.transport.clone(),
            self.callback()?,
        ))
    }

    /// Set callback for swarm.
    pub fn set_callback(&self, callback: SharedSwarmCallback) -> Result<()> {
        self.transport.callback_slot().replace(callback)
    }

    /// Create [Stabilizer] for swarm; its inbox-delivery intent is interpreted here, toward
    /// whichever callback is set when it delivers.
    pub fn stabilizer(&self) -> Stabilizer {
        Stabilizer::new(
            self.transport.clone(),
            Arc::new(SwarmInboxDelivery::new(self.transport.clone())),
        )
    }

    /// Disconnect a connection. There are three steps:
    /// 1) remove from DHT;
    /// 2) remove from Transport;
    /// 3) close the connection;
    pub async fn disconnect(&self, peer: Did) -> Result<()> {
        self.transport.disconnect(peer).await
    }

    /// Start a non-routable handshake with a peer.
    ///
    /// The peer becomes visible through the connection inspection APIs only
    /// after its data channel opens and the swarm admits it to the DHT.
    pub async fn connect(&self, peer: Did) -> Result<()> {
        if peer == self.did() {
            return Err(Error::ShouldNotConnectSelf);
        }
        self.transport.connect(peer, self.inner_callback()?).await
    }

    /// Send [Message] to peer.
    pub async fn send_message(&self, msg: Message, destination: Did) -> Result<uuid::Uuid> {
        self.transport.send_message(msg, destination).await
    }

    /// Send a message directly to an already connected peer, without a Chord lookup.
    ///
    /// This preserves an application protocol's explicit next-hop selection. Callers must
    /// ensure the destination has an active direct transport connection.
    pub async fn send_direct_message(&self, msg: Message, destination: Did) -> Result<uuid::Uuid> {
        self.transport.send_direct_message(msg, destination).await
    }

    /// List active, routable peers and their connection status.
    pub fn peers(&self) -> Vec<ConnectionInspect> {
        self.transport
            .get_connections()
            .iter()
            .map(|(did, c)| ConnectionInspect {
                did: did.to_string(),
                state: format!("{:?}", c.webrtc_connection_state()),
            })
            .collect()
    }

    /// List DIDs with active, routable transport connections.
    pub fn peer_dids(&self) -> Vec<Did> {
        self.transport.get_connection_ids()
    }

    /// List DIDs whose direct WebRTC transport connection is active.
    pub fn connected_peer_dids(&self) -> Vec<Did> {
        self.transport.get_connection_ids()
    }

    /// Whether the transport holds an unadmitted handshake to `peer` (pending or admitting),
    /// whichever side started it. A new offer to such a peer is refused as `AlreadyConnected`
    /// while the handshake is inside the pending timeout; a stale one is expired by the
    /// reservation instead.
    pub fn has_unadmitted_connection(&self, peer: Did) -> Result<bool> {
        Ok(self.transport.unadmitted_attempt(peer)?.is_some())
    }

    /// Whether `peer` is admitted as the application sees it: its connection record is active
    /// and the admission was announced as `ConnectionStateChange { Connected }`. Ready or
    /// recovering, its transport is the swarm's to heal or retire. Law:
    /// `is_peer_admitted(p) ⟹` the record's retirement is delivered as
    /// [`SwarmEvent::PeerRetired`](callback::SwarmEvent::PeerRetired). A record admitted but
    /// not yet announced is not counted, since its retirement would be silent.
    pub fn is_peer_admitted(&self, peer: Did) -> Result<bool> {
        Ok(self.transport.announced_attempt(peer)?.is_some())
    }

    /// Cancel the handshake `attempt` iff it is still pending, that is, no data channel has
    /// opened on it; one that is admitting, admitted or superseded meanwhile is left alone.
    /// Returns whether it was cancelled.
    pub async fn cancel_pending_connection_attempt(
        &self,
        attempt: ConnectionAttempt,
    ) -> Result<bool> {
        self.transport.cancel_pending_connection(attempt.0).await
    }

    /// Take the local step of a successor lookup for `key`, and route the lookup when the
    /// local topology cannot decide it.
    ///
    /// Law: in a ring every present node is the successor of its own identifier, so a lookup
    /// for `key` answers `key` exactly when `key` is in the overlay, as far as the answering
    /// node's successor list is current. The local step decides `Local(head)` when `key` lies
    /// in the local successor interval `(n, head]`, and `Local(n)` when this node has no
    /// successor; otherwise the request `FindSuccessorSend { did: key, strict: false }` is sent
    /// to the hop that same step chose, never to `key` itself even when `key` is a direct peer
    /// (`key` would answer with its own successor), and the answer returns as a
    /// `FindSuccessorReport` under the returned transaction id, recognisable by
    /// [`FindSuccessorReport::is_application_lookup`](crate::message::FindSuccessorReport::is_application_lookup).
    /// The step and the hop come from one topology snapshot.
    pub async fn lookup_successor(&self, key: Did) -> Result<SuccessorLookup> {
        match self.dht.find_successor(key)? {
            PeerRingAction::Some(successor) => Ok(SuccessorLookup::Local(successor)),
            PeerRingAction::RemoteAction(next, _) => {
                let request = Message::FindSuccessorSend(FindSuccessorSend {
                    did: key,
                    strict: false,
                    then: FindSuccessorThen::Report(FindSuccessorReportHandler::None),
                });
                self.transport
                    .send_message_by_hop(request, key, next)
                    .await
                    .map(SuccessorLookup::Routed)
            }
            action => Err(Error::PeerRingUnexpectedAction(Box::new(action))),
        }
    }

    /// Return local measurement counters for `peer`, if observed.
    pub async fn peer_measurement(&self, peer: Did) -> Option<PeerMeasurement> {
        self.transport.peer_measurement(peer).await
    }

    /// Return every retained local peer measurement.
    pub async fn peer_measurements(&self) -> Vec<PeerMeasurement> {
        self.transport.peer_measurements().await
    }

    /// Return one bounded page of retained local peer measurements.
    pub async fn peer_measurements_page(
        &self,
        after: Option<Did>,
        limit: NonZeroUsize,
    ) -> PeerMeasurementPage {
        self.transport.peer_measurements_page(after, limit).await
    }

    /// Check the status of swarm
    pub async fn inspect(&self) -> SwarmInspect {
        SwarmInspect::inspect(self).await
    }

    /// Return destination-scoped transaction replay counters.
    pub fn transaction_replay_counters(&self) -> ReplayCounters {
        self.transport.replay_counters()
    }

    /// Return aggregate final-destination origin-quota drop counters by logical lane and reason.
    pub fn origin_quota_counters(&self) -> OriginQuotaCounters {
        self.transport.origin_quota_counters()
    }
}

impl Swarm {
    /// Create new connection and its answer. This function will wrap the offer inside a payload
    /// with verification.
    pub async fn create_offer(&self, peer: Did) -> Result<MessagePayload> {
        self.offer_connection(peer)
            .await
            .map(|(_, payload)| payload)
    }

    /// Reserve a connection generation for `peer` and create its offer, returning the attempt
    /// that names the generation so the caller can accept the answer for it or cancel it
    /// without touching a handshake the peer started meanwhile.
    pub async fn offer_connection(&self, peer: Did) -> Result<(ConnectionAttempt, MessagePayload)> {
        let (attempt, offer_msg) = self
            .transport
            .prepare_connection_offer_with_attempt(peer, self.inner_callback()?)
            .await?;

        // This payload has fake next_hop.
        // The invoker should fix it before sending.
        let payload = self
            .transport
            .signed_payload(Message::ConnectNodeSend(offer_msg), self.did(), peer)
            .await?;

        Ok((ConnectionAttempt(attempt), payload))
    }

    /// Answer the offer of remote connection. This function will verify the answer payload and
    /// will wrap the answer inside a payload with verification.
    pub async fn answer_offer(&self, offer_payload: MessagePayload) -> Result<MessagePayload> {
        if !offer_payload.verify_transaction_and_payload(self.network_id()) {
            return Err(Error::VerifySignatureFailed);
        }

        let Message::ConnectNodeSend(msg) = offer_payload.transaction.data()? else {
            return Err(Error::InvalidMessage(
                "Should be ConnectNodeSend".to_string(),
            ));
        };
        if offer_payload.transaction.destination != self.did() {
            return Err(Error::InvalidMessage(
                "ConnectNodeSend destination does not match this node".to_string(),
            ));
        }
        self.transport
            .admit_final_transaction(&offer_payload.transaction, OriginQuotaLane::DhtControl)
            .await?;

        let peer = offer_payload.transaction.origin();
        let answer_msg = self
            .transport
            .answer_remote_connection(peer, self.inner_callback()?, &msg)
            .await?;

        // This payload has fake next_hop.
        // The invoker should fix it before sending.
        let answer_payload = self
            .transport
            .signed_payload(Message::ConnectNodeReport(answer_msg), self.did(), peer)
            .await?;

        Ok(answer_payload)
    }

    /// Accept the answer of remote connection. This function will verify the answer payload and
    /// will return its did with the connection.
    pub async fn accept_answer(&self, answer_payload: MessagePayload) -> Result<()> {
        self.accept_answer_with(None, answer_payload).await
    }

    /// Accept the answer of remote connection for the generation `attempt` reserved by
    /// [`Swarm::offer_connection`]; refused as `ConnectionAttemptSuperseded` when the peer's slot
    /// now holds another generation, so the answer is never applied to a handshake the peer
    /// started.
    pub async fn accept_answer_for(
        &self,
        attempt: ConnectionAttempt,
        answer_payload: MessagePayload,
    ) -> Result<()> {
        self.accept_answer_with(Some(attempt.0), answer_payload)
            .await
    }

    /// Verify the answer payload and apply it to the pending generation, or to `expected` only.
    async fn accept_answer_with(
        &self,
        expected: Option<PendingConnectionAttempt>,
        answer_payload: MessagePayload,
    ) -> Result<()> {
        if !answer_payload.verify_transaction_and_payload(self.network_id()) {
            return Err(Error::VerifySignatureFailed);
        }

        let Message::ConnectNodeReport(ref msg) = answer_payload.transaction.data()? else {
            return Err(Error::InvalidMessage(
                "Should be ConnectNodeReport".to_string(),
            ));
        };
        if answer_payload.transaction.destination != self.did() {
            return Err(Error::InvalidMessage(
                "ConnectNodeReport destination does not match this node".to_string(),
            ));
        }
        self.transport
            .admit_final_transaction(&answer_payload.transaction, OriginQuotaLane::DhtControl)
            .await?;

        let peer = answer_payload.transaction.signer();
        self.transport
            .accept_remote_connection(peer, msg, expected)
            .await
    }
}
