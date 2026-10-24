#![deny(missing_docs)]

//! This mod is the main entrance of swarm.

pub mod application;
mod builder;
/// The transport callback of a connection: its inbound pipeline and lifecycle handling.
pub(crate) mod callback;
mod detached;
pub(crate) mod inbox;
/// Non-blocking observation hooks for operator telemetry.
pub mod observer;
pub(crate) mod session_link;
/// The Swarm's storage API, at the application boundary.
pub mod storage_api;
pub(crate) mod transport;

use std::num::NonZeroUsize;
use std::sync::Arc;

pub use builder::SwarmBuilder;
/// The read-only view of one lane's send credit that [`Swarm::application_credit`] returns: the
/// transport's type, re-exported so an application reads it through core alone.
pub use rings_transport::core::transport::LaneCreditView;

use self::callback::SwarmTransportCallback;
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
use crate::inspect::MailboxStorageInspect;
use crate::inspect::SwarmInspect;
use crate::measure::PeerMeasurement;
use crate::measure::PeerMeasurementPage;
use crate::message::DhtProtocolMode;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorSend;
use crate::message::FindSuccessorThen;
use crate::message::Message;
use crate::message::MessageCategory;
use crate::message::MessageLimit;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::OriginQuotaCounters;
use crate::message::PayloadSender;
use crate::message::ReplayCounters;
use crate::swarm::application::ApplicationData;
use crate::swarm::application::ApplicationInbox;
use crate::swarm::application::SuccessorReport;
use crate::swarm::inbox::SwarmInboxDelivery;
use crate::swarm::observer::LookupCorrelation;
use crate::swarm::observer::LookupKind;
use crate::swarm::observer::LookupOutcome;
use crate::swarm::transport::CounterOffer;
use crate::swarm::transport::CreatedAnswer;
use crate::swarm::transport::OfferOwner;
pub use crate::swarm::transport::OfferReply;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;

/// How a successor lookup was decided: from the local topology, or routed, with the handle its
/// report resolves.
#[derive(Debug)]
pub enum SuccessorLookup {
    /// The successor of the key as the local topology knows it.
    Local(Did),
    /// The lookup was routed through the hop the local step chose; its report resolves this
    /// handle.
    Routed(SuccessorReport),
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

/// The node's storage answer clock as a fetcher read it before its fetch
/// ([`Swarm::storage_fetch_mark`]). Marks are ordered by when they were read.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StorageFetchMark(u64);

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
    pub fn delegator_pubkey(&self) -> Result<PublicKey<33>> {
        self.transport.delegation().delegator_pubkey()
    }

    /// Get the typed account verification public key.
    pub fn delegator_verification_pubkey(&self) -> Result<VerificationPublicKey> {
        self.transport.delegation().delegator_verification_pubkey()
    }

    /// Get this swarm's network id.
    pub fn network_id(&self) -> u32 {
        self.transport.network_id
    }

    /// Get the storage redundancy for this swarm's DHT protocol mode.
    pub fn storage_redundancy(&self) -> u16 {
        self.transport.storage_redundancy()
    }

    /// The mark a fetcher reads before `storage_fetch`, to learn from
    /// [`Self::storage_fetch_answered_since`] when its fetch was answered.
    pub fn storage_fetch_mark(&self) -> StorageFetchMark {
        StorageFetchMark(self.transport.storage_lookup_mark())
    }

    /// Whether `storage_fetch` of `entry_key` cached a found entry the cache then serves,
    /// locally or from a reply, after `mark` was read. A fetch of the key that starts meanwhile does not reset it. A
    /// late reply to an earlier fetch also counts, and the entry it caches is still a reply
    /// received after the mark.
    ///
    /// This is the fetch's reply marker. A reader cannot tell a reply by comparing cached values,
    /// since a cached value is projected at the clock of each read and changes, with no reply,
    /// when one of its elements crosses its horizon.
    ///
    /// Post: `false` once no fetch of `entry_key` is retained.
    pub fn storage_fetch_answered_since(
        &self,
        entry_key: Did,
        mark: StorageFetchMark,
    ) -> Result<bool> {
        self.transport.storage_lookup_answered_since(
            entry_key,
            self.transport.storage_redundancy(),
            mark.0,
        )
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

    /// The transport callback of a connection this swarm starts: its inbound pipeline and
    /// lifecycle handling.
    fn transport_callback(&self) -> SwarmTransportCallback {
        SwarmTransportCallback::new(self.transport.clone())
    }

    /// Claim the application inbox: everything core hands the application, in order (#935).
    ///
    /// Claimed once; a second claim is refused as `ApplicationInboxClaimed`. Until it is
    /// claimed, application data addressed to this node is discarded and peer facts are kept;
    /// once it is dropped, everything is discarded. Core never waits on the reader except for
    /// room on the application data lanes.
    pub fn application_inbox(&self) -> Result<ApplicationInbox> {
        self.transport.application().outlet.claim()
    }

    /// Record the message limit of application payloads starting with `namespace_prefix`:
    /// [`MessageLimit::Delegated`] to the namespace, or [`MessageLimit::Enforced`] by core.
    ///
    /// The application registers its namespaces in advance and core matches the prefix on its
    /// admission path, without calling the application. The match is exact when the
    /// application's namespace encoding is prefix-free. Delegation is trusted local
    /// configuration: a delegating namespace must enforce its own per-neighbour admission.
    pub fn set_admission_delegation(&self, namespace_prefix: Vec<u8>, limit: MessageLimit) {
        self.transport
            .application()
            .delegations
            .set(namespace_prefix, limit);
    }

    /// Whether an application payload belongs to a namespace registered as delegating through
    /// [`Swarm::set_admission_delegation`].
    pub fn delegates_admission(&self, application_payload: &[u8]) -> bool {
        self.transport
            .application()
            .delegations
            .delegates(application_payload)
    }

    /// Create [Stabilizer] for swarm; its inbox-delivery intent is interpreted here, toward
    /// the application inbox.
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
        self.transport
            .connect(peer, self.transport_callback())
            .await
    }

    /// Send application `data` to `destination`, routed by Chord.
    ///
    /// Only [`ApplicationData`] crosses the boundary outbound, as inbound: an application cannot
    /// originate core's own protocol messages. `data` is sent as `Message::from(data)`, which
    /// the receiving core projects back to `data` (`project ∘ from = Some`).
    pub async fn send_message(
        &self,
        data: ApplicationData,
        destination: Did,
    ) -> Result<uuid::Uuid> {
        self.transport
            .application_egress()
            .send_message(Message::from(data), destination)
            .await
    }

    /// Send application `data` directly to an already connected peer, without a Chord lookup.
    ///
    /// This preserves an application protocol's explicit next-hop selection. Callers must
    /// ensure the destination has an active direct transport connection.
    pub async fn send_direct_message(
        &self,
        data: ApplicationData,
        destination: Did,
    ) -> Result<uuid::Uuid> {
        self.transport
            .application_egress()
            .send_direct_message(Message::from(data), destination)
            .await
    }

    /// Test builds: send core's own `message` under the application discipline, routed by
    /// Chord, as no application can. Witnesses that drive a protocol message end to end use it.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) async fn send_core_message_for_test(
        &self,
        message: Message,
        destination: Did,
    ) -> Result<uuid::Uuid> {
        self.transport
            .application_egress()
            .send_message(message, destination)
            .await
    }

    /// Test builds: [`Self::send_core_message_for_test`] directly to a connected peer.
    #[cfg(test)]
    pub(crate) async fn send_direct_core_message_for_test(
        &self,
        message: Message,
        destination: Did,
    ) -> Result<uuid::Uuid> {
        self.transport
            .application_egress()
            .send_direct_message(message, destination)
            .await
    }

    /// List active, routable peers and their connection status.
    ///
    /// The status is the connection's raw WebRTC state. A peer is routable once its data
    /// channel is open in `Connecting` or `Connected`, so a listed peer may still read
    /// `Connecting`: a browser can open the data channel before it reports `connected`.
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

    /// Whether the transport holds an unadmitted handshake to `peer` (pending or admitting),
    /// whichever side started it. A new offer to such a peer is refused as `AlreadyConnected`
    /// while the handshake is inside the pending timeout; a stale one is expired by the
    /// reservation instead.
    pub fn has_unadmitted_connection(&self, peer: Did) -> Result<bool> {
        Ok(self.transport.unadmitted_attempt(peer)?.is_some())
    }

    /// Whether `peer` is admitted: its connection record is active. Ready or recovering, its
    /// transport is the swarm's to heal or retire. Law: an admission is announced as
    /// [`SwarmEvent::PeerAdmitted`](application::SwarmEvent::PeerAdmitted) in the transition
    /// that activates the record, so `is_peer_admitted(p) ⟹` that admission was announced and
    /// the record's retirement will be, as
    /// [`SwarmEvent::PeerRetired`](application::SwarmEvent::PeerRetired).
    pub fn is_peer_admitted(&self, peer: Did) -> Result<bool> {
        Ok(self.transport.active_attempt(peer)?.is_some())
    }

    /// A read-only view of the send credit of the Application lane of `peer`'s sendable
    /// connection generation, or `None` without one (#913). Application data travels on that
    /// lane, so an application that shapes its own emission paces it on this view: the view
    /// reserves nothing and holds the connection weakly, so reading it never changes what core
    /// may send.
    pub fn application_credit(&self, peer: Did) -> Option<LaneCreditView> {
        self.transport.get_connection(peer).map(|connection| {
            connection.send_credit_view(MessageCategory::Application.channel_lane())
        })
    }

    /// `𝓡`, the capacity of the connection registry (#723): the most connection records, in
    /// any phase, the swarm holds at once, `2 × (finger slots + successors + 1)`. It bounds the
    /// admitted peers, so an application keying state by admitted link can size it by `𝓡`.
    pub fn connection_registry_capacity(&self) -> Result<usize> {
        self.transport.connection_registry_capacity()
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
    /// (`key` would answer with its own successor), and the answer, a `FindSuccessorReport`
    /// under the request's transaction, resolves the returned [`SuccessorReport`]. The handle is
    /// registered before the request is sent, so its report always finds it; dropping it
    /// withdraws the lookup. The step and the hop come from one topology snapshot.
    pub async fn lookup_successor(&self, key: Did) -> Result<SuccessorLookup> {
        match self.dht.find_successor(key)? {
            PeerRingAction::Some(successor) => {
                let correlation = LookupCorrelation::StorageResource(key);
                self.transport
                    .observer()
                    .lookup_started(LookupKind::Successor, correlation);
                self.transport.observer().lookup_finished(
                    LookupKind::Successor,
                    correlation,
                    LookupOutcome::Succeeded,
                );
                Ok(SuccessorLookup::Local(successor))
            }
            PeerRingAction::RemoteAction(next, _) => {
                let request = Message::FindSuccessorSend(FindSuccessorSend {
                    did: key,
                    strict: false,
                    then: FindSuccessorThen::Report(FindSuccessorReportHandler::None),
                });
                let egress = self.transport.application_egress();
                let payload = egress.originate(request, key, Some(next)).await?;
                let report = self
                    .transport
                    .application()
                    .lookups
                    .register(payload.transaction.tx_id);
                egress.send_payload(payload).await?;
                Ok(SuccessorLookup::Routed(report))
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

    /// Return aggregate live relay-inbox state without identifiers or payloads.
    pub async fn mailbox_storage_inspect(&self) -> Result<MailboxStorageInspect> {
        MailboxStorageInspect::inspect(&self.dht).await
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
    ///
    /// The caller owns the offer and carries it out of band, so the attempt never sends it
    /// over the DHT as a counter-offer (#947). It prevails over a crossing overlay offer, which
    /// is refused; of a crossing out-of-band offer, one from a larger DID is refused and one from
    /// a smaller DID takes the slot (`Law (Tie-break)`).
    pub async fn offer_connection(&self, peer: Did) -> Result<(ConnectionAttempt, MessagePayload)> {
        let (attempt, offer_msg) = self
            .transport
            .prepare_connection_offer_owned_by(
                peer,
                self.transport_callback(),
                OfferOwner::External,
            )
            .await?;

        // This payload has fake next_hop.
        // The invoker should fix it before sending.
        let payload = self
            .transport
            .settle_handshake_step(
                attempt,
                "wrapping connection offer",
                self.transport.application_egress().originate(
                    Message::ConnectNodeSend(offer_msg.clone()),
                    peer,
                    Some(self.did()),
                ),
            )
            .await?;
        self.transport
            .mark_offer_sent(attempt, &offer_msg, payload.transaction.tx_id)?;

        Ok((ConnectionAttempt(attempt), payload))
    }

    /// Answer the offer of remote connection. This function will verify the answer payload and
    /// will wrap the answer inside a payload with verification.
    ///
    /// The offer is carried out of band, so it prevails over this node's own sent overlay offer
    /// to the same peer, which is released (`Law (Tie-break)`), and the reply is
    /// [`OfferReply::Answer`], a `ConnectNodeReport` for the peer's `accept_answer`: a
    /// counter-offer would leave the peer to deliver its answer through this node's
    /// `accept_answer`, which a node's public HTTP API does not serve. Nodes of 0.34.2 and
    /// earlier reply with [`OfferReply::CounterOffer`] when their sent overlay offer prevails on
    /// the DID tie-break (#947): that offer, a `ConnectNodeSend`, which the caller hands to the
    /// peer's `answer_offer`; the peer abandons its own offer and answers this one, and that
    /// answer goes to the replying node's `accept_answer`.
    pub async fn answer_offer(
        &self,
        offer_payload: MessagePayload,
    ) -> Result<OfferReply<MessagePayload, MessagePayload>> {
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
            .admit_final_transaction(
                &offer_payload.transaction,
                MessageCategory::DhtControl.into(),
            )
            .await?;

        let peer = offer_payload.transaction.origin();
        // The reply payload has a fake next_hop; the invoker fixes it before sending.
        match self
            .transport
            .answer_remote_connection(peer, self.transport_callback(), &msg)
            .await?
        {
            OfferReply::Answer(CreatedAnswer { attempt, report }) => self
                .transport
                .settle_handshake_step(
                    attempt,
                    "wrapping connection answer",
                    self.transport.application_egress().originate(
                        Message::ConnectNodeReport(report),
                        peer,
                        Some(self.did()),
                    ),
                )
                .await
                .map(OfferReply::Answer),
            OfferReply::CounterOffer(CounterOffer { offer, .. }) => self
                .transport
                .application_egress()
                .originate(Message::ConnectNodeSend(offer), peer, Some(self.did()))
                .await
                .map(OfferReply::CounterOffer),
        }
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
            .admit_final_transaction(
                &answer_payload.transaction,
                MessageCategory::DhtControl.into(),
            )
            .await?;

        let peer = answer_payload.transaction.signer();
        self.transport
            .accept_remote_connection(peer, msg, expected, None)
            .await
    }
}
