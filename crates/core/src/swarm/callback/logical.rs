use std::sync::Arc;

use futures::lock::Mutex as FuturesMutex;

use super::InboundLane;
use super::LocalDelivery;
use super::LogicalInbound;
use crate::dht::Did;
use crate::fair_admission::SlotRoster;
use crate::measure::Authentication;
use crate::message::with_message_variants;
use crate::message::CustomMessage;
use crate::message::EdgeRelation;
use crate::message::HandleMsg;
use crate::message::HandlerContext;
use crate::message::LinkWindows;
use crate::message::Message;
use crate::message::MessageCategory;
use crate::message::MessageHandler;
use crate::message::MessageKind;
use crate::message::MessagePayload;
use crate::message::OriginQuotaCharge;
use crate::message::Principal;
use crate::message::RelayAdmission;
use crate::swarm::application::ApplicationData;
use crate::swarm::application::ApplicationMessage;
use crate::swarm::observer::LookupCorrelation;
use crate::swarm::observer::LookupKind;
use crate::swarm::observer::LookupOutcome;
use crate::swarm::observer::MessageActivity;
use crate::swarm::observer::MessageObservation;
use crate::swarm::observer::ObservationOutcome;
use crate::swarm::transport::SwarmTransport;

impl LocalDelivery {
    /// Deliver through `transport`'s handlers to its application inbox.
    pub(crate) fn new(transport: Arc<SwarmTransport>) -> Self {
        Self {
            pipeline: LogicalInbound::new(transport),
        }
    }

    /// Deliver one locally sourced payload: dispatch, then the application inbox, exactly as for
    /// a verified frame from a connection, but with no connection to gate on and never waiting.
    ///
    /// Room for application data is reserved, holding the message, before the transaction is
    /// admitted, so a payload this pass cannot hand over (no room, or no application reading)
    /// is left unadmitted for a later pass instead of being admitted, handled and lost to the
    /// replay window. The admission is a relay admission (the custody law of
    /// `message::replay`): the transaction enters the window and relay custody in one record
    /// write, and a payload already in custody (admitted before a restart, never taken) is
    /// handled and queued again with nothing admitted. Core handles the message the room holds by
    /// reference, so it is decoded once and never copied. A payload carrying application data is
    /// queued with a consumption signal, so its stored copy is retired only once the
    /// application takes it.
    ///
    /// Pre: `payload.transaction.destination` is this node and the payload was verified by the
    /// caller (the inbox witness verifies it as of its hold instant).
    pub(crate) async fn deliver(
        &self,
        payload: &MessagePayload,
    ) -> crate::error::Result<LocalDelivered> {
        let pipeline = &self.pipeline;
        let message = payload.transaction.data::<Message>()?;
        let lane = InboundLane::from_kind(MessageKind::from_message(&message));
        let Some(application) = pipeline.application_message(payload, message) else {
            // Core's own body. Stored mail is application data by its witness, so the inbox
            // drain never comes here; the body is decoded again for its handler.
            pipeline.tap_inbound(payload);
            pipeline.admit_local_transaction(payload, lane).await?;
            pipeline
                .handle_payload(payload, Principal::Local, None)
                .await?;
            return Ok(LocalDelivered::Handled);
        };
        let outlet = &pipeline.transport.application().outlet;
        let Some(room) = outlet.try_reserve(application) else {
            return Ok(LocalDelivered::Deferred);
        };
        pipeline.tap_inbound(payload);
        pipeline.admit_relayed_transaction(payload, lane).await?;
        pipeline
            .handle_application(payload, &room.message().data)
            .await?;
        Ok(LocalDelivered::Queued(room.fill()))
    }
}

/// What [`LocalDelivery::deliver`] did with a payload.
#[derive(Debug)]
pub(crate) enum LocalDelivered {
    /// Admitted and handled, with no application data to hand over.
    Handled,
    /// Admitted, handled, and its application data queued: the receiver resolves once the
    /// application takes it, and is cancelled if the inbox discards it first.
    Queued(futures::channel::oneshot::Receiver<()>),
    /// Its application data found no room or no reader: nothing was admitted or handled, and
    /// its stored copy stays.
    Deferred,
}

/// The connection a frame arrived on: its peer, and whether its handshake authenticated it.
#[derive(Clone, Copy, Debug)]
pub(super) struct Arrival {
    /// The connection's peer, if its name parses.
    pub(super) peer: Option<Did>,
    /// Whether the connection's handshake authenticated that peer.
    pub(super) authentication: Authentication,
}

impl Arrival {
    /// The peer iff the connection's handshake authenticated it (pure).
    fn authenticated_peer(self) -> Option<Did> {
        self.peer
            .filter(|_| matches!(self.authentication, Authentication::Authenticated))
    }

    /// Relate the connection to `payload`'s origin account: only a handshake-authenticated
    /// connection names a neighbour; an unauthenticated or unknown peer is never
    /// [`EdgeRelation::Neighbour`], whatever DID it claims.
    pub(super) fn edge_relation(self, payload: &MessagePayload) -> EdgeRelation {
        EdgeRelation::of(self.authenticated_peer(), payload.transaction.origin())
    }

    /// The principal that delivered the frame (load-bounds paper, Law 1): the slot of its edge
    /// in `slots` iff the connection's handshake authenticated it, else [`Principal::Unbound`],
    /// which no slot's floor covers. Every production connection is bound to its handshake and
    /// holds a slot while admitted, so a frame that reaches admission names its slot unless its
    /// edge was retired meanwhile.
    pub(super) fn introducer(self, slots: &SlotRoster) -> Principal {
        slots.principal_of(self.authenticated_peer())
    }
}

impl LogicalInbound {
    /// The logical stage over `transport`, with its message handler.
    pub(super) fn new(transport: Arc<SwarmTransport>) -> Self {
        let message_handler = MessageHandler::new(transport.clone());
        Self {
            transport,
            message_handler,
        }
    }

    /// Dispatch `payload`, delivered by `introducer`, to its handler and record the
    /// observation; returns the decoded body.
    pub(super) async fn handle_payload(
        &self,
        payload: &MessagePayload,
        introducer: Principal,
        prepared_message: Option<Message>,
    ) -> crate::error::Result<Message> {
        let context = HandlerContext::new(payload, introducer);
        let message = match prepared_message {
            Some(message) => message,
            None => payload.transaction.data()?,
        };
        let lookup_completion = match &message {
            Message::FindSuccessorReport(_) => Some((
                LookupKind::Successor,
                LookupCorrelation::Transaction(payload.transaction.tx_id),
            )),
            Message::FoundEntry(response) => Some((
                LookupKind::Storage,
                LookupCorrelation::StorageResource(response.resource),
            )),
            _ => None,
        };

        macro_rules! dispatch_message_body {
            (Chunk, $msg:expr) => {{
                let _ = $msg;
                Err(crate::error::Error::InboundActorInvariantViolation)
            }};
            (ProbeOffer, $msg:expr) => {
                self.message_handler.handle(&context, $msg.as_ref()).await
            };
            (ProbeAcknowledgement, $msg:expr) => {
                self.message_handler.handle(&context, $msg.as_ref()).await
            };
            ($variant:ident, $msg:expr) => {
                self.message_handler.handle(&context, $msg).await
            };
        }
        macro_rules! dispatch_message {
            ($( $(#[$docs:meta])* $index:literal => $variant:ident($body:ty): $class:ident, $storage_route:ident ),+ $(,)?) => {
                match message {
                    $(Message::$variant(ref msg) => dispatch_message_body!($variant, msg)),+
                }
            };
        }

        let result = with_message_variants!(dispatch_message);
        self.record_handling(
            payload,
            MessageKind::from_message(&message),
            lookup_completion,
            result,
        )?;
        Ok(message)
    }

    /// Dispatch application data `payload` carries to its handler, by reference, and record the
    /// observation: the data stays where the caller keeps it (the inbox room that hands it to
    /// the application). The payload is delivered out of this node's own relay inbox, so it is
    /// local.
    async fn handle_application(
        &self,
        payload: &MessagePayload,
        data: &ApplicationData,
    ) -> crate::error::Result<()> {
        let context = HandlerContext::new(payload, Principal::Local);
        let (kind, result) = match data {
            ApplicationData::Custom(message) => (
                MessageKind::CustomMessage,
                self.message_handler.handle(&context, message).await,
            ),
            ApplicationData::E2eHandshakeRequest(request) => (
                MessageKind::E2eHandshakeRequest,
                self.message_handler.handle(&context, request).await,
            ),
            ApplicationData::E2eHandshakeResponse(response) => (
                MessageKind::E2eHandshakeResponse,
                self.message_handler.handle(&context, response).await,
            ),
            ApplicationData::E2eStreamFrame(frame) => (
                MessageKind::E2eStreamFrame,
                self.message_handler.handle(&context, frame).await,
            ),
        };
        self.record_handling(payload, kind, None, result)
    }

    /// Record the observation of a handled message of `kind`, finish the lookup it completes,
    /// and surface a handler's error, so a message whose handler failed is never handed to
    /// the application as a successful inbound message.
    fn record_handling(
        &self,
        payload: &MessagePayload,
        kind: MessageKind,
        lookup_completion: Option<(LookupKind, LookupCorrelation)>,
        result: crate::error::Result<()>,
    ) -> crate::error::Result<()> {
        let outcome = if result.is_ok() {
            ObservationOutcome::Succeeded
        } else {
            ObservationOutcome::Failed
        };
        self.transport.observe_message(MessageObservation {
            activity: MessageActivity::Received,
            category: kind.class(),
            message_class: kind.as_str(),
            outcome,
        });
        if result.is_ok() && self.is_local_destination(payload) {
            if let Some((kind, correlation)) = lookup_completion {
                self.transport.observer().lookup_finished(
                    kind,
                    correlation,
                    LookupOutcome::Succeeded,
                );
            }
        }
        result.inspect_err(|error| tracing::error!(error = ?error, "failed to handle a payload"))
    }

    pub(super) fn is_local_destination(&self, payload: &MessagePayload) -> bool {
        payload.transaction.destination == self.transport.dht.did
    }

    /// Commit replay and origin-quota admission of a transaction addressed to this node.
    ///
    /// The charge is the message's class record, with its message limit skipped only when
    /// `edge` is [`EdgeRelation::Neighbour`] and the namespace delegated its admission (see
    /// [`OriginQuotaCharge::select`]); a new stream is introduced by the edge `arrival` names.
    /// Transactions for other destinations are not admitted here.
    pub(super) async fn admit_final_transaction(
        &self,
        payload: &MessagePayload,
        message: &Message,
        lane: crate::swarm::callback::InboundLane,
        arrival: Arrival,
    ) -> crate::error::Result<()> {
        let edge = arrival.edge_relation(payload);
        let introducer = arrival.introducer(self.transport.dht.edges());
        self.admit_charged(payload, lane, introducer, |class| {
            OriginQuotaCharge::select(class, edge, || self.delegates_admission(message))
        })
        .await
    }

    /// [`Self::admit_final_transaction`] for a locally sourced payload: it crossed no
    /// connection, so no neighbour can have its admission delegated and its class record's
    /// message limit is enforced, as for any remote origin; a new stream is local.
    async fn admit_local_transaction(
        &self,
        payload: &MessagePayload,
        lane: crate::swarm::callback::InboundLane,
    ) -> crate::error::Result<()> {
        self.admit_charged(payload, lane, Principal::Local, OriginQuotaCharge::from)
            .await
    }

    /// [`Self::admit_local_transaction`] for a payload a relay inbox delivered: the admission
    /// also enters relay custody, or finds the payload already in custody (the custody law of
    /// `message::replay`).
    async fn admit_relayed_transaction(
        &self,
        payload: &MessagePayload,
        lane: crate::swarm::callback::InboundLane,
    ) -> crate::error::Result<RelayAdmission> {
        let class = lane
            .class()
            .ok_or(crate::error::Error::InboundActorInvariantViolation)?;
        self.transport
            .admit_relayed_transaction(&payload.transaction, OriginQuotaCharge::from(class))
            .await
    }

    /// Commit admission of `payload`, delivered by `introducer`, under the charge `charge`
    /// selects for its class, if it is addressed to this node.
    async fn admit_charged(
        &self,
        payload: &MessagePayload,
        lane: crate::swarm::callback::InboundLane,
        introducer: Principal,
        charge: impl FnOnce(MessageCategory) -> OriginQuotaCharge,
    ) -> crate::error::Result<()> {
        if self.is_local_destination(payload) {
            let class = lane
                .class()
                .ok_or(crate::error::Error::InboundActorInvariantViolation)?;
            self.transport
                .admit_final_transaction(&payload.transaction, charge(class), introducer)
                .await?;
        }
        Ok(())
    }

    /// Whether the namespace of an application message delegated its admission, as the
    /// application registered it in advance; no application code runs here.
    fn delegates_admission(&self, message: &Message) -> bool {
        match message {
            Message::CustomMessage(CustomMessage(application_payload)) => self
                .transport
                .application()
                .delegations
                .delegates(application_payload.as_slice()),
            _ => false,
        }
    }

    /// Whether the namespace of an application message delegated its replay (#1081), as the
    /// application registered it in advance; no application code runs here.
    pub(super) fn delegates_replay(&self, message: &Message) -> bool {
        match message {
            Message::CustomMessage(CustomMessage(application_payload)) => self
                .transport
                .application()
                .replay_delegations
                .delegates(application_payload.as_slice()),
            _ => false,
        }
    }

    /// Commit the replay and origin-quota admission of a link-local transaction addressed to
    /// this node in `windows`, the volatile windows of the connection it arrived on (#1081;
    /// durable-records, Law Window by classification). Charged as
    /// [`Self::admit_final_transaction`] charges it; transactions for other destinations are not
    /// admitted here.
    pub(super) async fn admit_link_transaction(
        &self,
        windows: &FuturesMutex<LinkWindows>,
        payload: &MessagePayload,
        message: &Message,
        lane: crate::swarm::callback::InboundLane,
        edge: EdgeRelation,
    ) -> crate::error::Result<()> {
        if !self.is_local_destination(payload) {
            return Ok(());
        }
        let class = lane
            .class()
            .ok_or(crate::error::Error::InboundActorInvariantViolation)?;
        let charge = OriginQuotaCharge::select(class, edge, || self.delegates_admission(message));
        let mut windows = windows.lock().await;
        self.transport
            .admit_link_transaction(&mut windows, &payload.transaction, charge)
            .await
    }

    /// The application message `payload` carries for this node: `None` when it is addressed
    /// elsewhere or its body is core's own.
    pub(super) fn application_message(
        &self,
        payload: &MessagePayload,
        message: Message,
    ) -> Option<ApplicationMessage> {
        if !self.is_local_destination(payload) {
            return None;
        }
        ApplicationMessage::project(payload, message)
    }

    /// Show `payload`, about to enter the logical stage, to the inbound tap of test builds.
    #[cfg(test)]
    pub(super) fn tap_inbound(&self, payload: &MessagePayload) {
        self.transport.inbound_tap_for_test().observe(payload);
    }

    /// Outside test builds there is no inbound tap: nothing is shown.
    #[cfg(not(test))]
    pub(super) fn tap_inbound(&self, _payload: &MessagePayload) {}

    /// Wait while a test build holds `lane` (see the inbound tap).
    #[cfg(test)]
    pub(super) async fn pass_tapped_lane(&self, lane: crate::swarm::callback::InboundLane) {
        self.transport.inbound_tap_for_test().pass_lane(lane).await;
    }

    /// Outside test builds no lane is held: nothing waits.
    #[cfg(not(test))]
    pub(super) async fn pass_tapped_lane(&self, _lane: crate::swarm::callback::InboundLane) {}
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use crate::delegation::DelegateeKey;
    use crate::dht::delivery::NextHop;
    use crate::ecc::SecretKey;
    use crate::error::Error;
    use crate::message::Message;
    use crate::message::MessageSigner;
    use crate::message::OriginQuotaConfig;
    use crate::message::OriginQuotaError;
    use crate::message::OriginQuotaLaneConfig;
    use crate::storage::MemStorage;
    use crate::swarm::SwarmBuilder;
    use crate::tests::TEST_NETWORK_ID;

    /// A swarm with the given origin quota whose application inbox is claimed.
    fn claimed_swarm(
        quota: Option<OriginQuotaConfig>,
    ) -> (
        crate::swarm::Swarm,
        crate::swarm::application::ApplicationInbox,
    ) {
        let local = DelegateeKey::new_with_seckey(&SecretKey::random()).expect("local session");
        let builder = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
            .replay_storage(crate::tests::volatile_replay_storage());
        let swarm = match quota {
            Some(quota) => builder.origin_quota(quota),
            None => builder,
        }
        .build();
        let inbox = swarm.application_inbox().expect("unclaimed inbox");
        (swarm, inbox)
    }

    fn payload(
        destination: crate::dht::Did,
        sequence: u64,
    ) -> crate::error::Result<MessagePayload> {
        let sender = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        MessagePayload::new_send_with_sequence(
            Message::custom(b"replay boundary")?,
            MessageSigner::new(&sender, TEST_NETWORK_ID),
            NextHop::toward(destination),
            destination,
            sequence,
            None,
        )
    }

    fn payload_from(
        sender: &DelegateeKey,
        destination: crate::dht::Did,
        sequence: u64,
    ) -> crate::error::Result<MessagePayload> {
        MessagePayload::new_send_with_sequence(
            Message::custom(b"quota before validation")?,
            MessageSigner::new(sender, TEST_NETWORK_ID),
            NextHop::toward(destination),
            destination,
            sequence,
            None,
        )
    }

    /// A relay delivery is admitted into the window and relay custody before it is queued; one
    /// still in custody (never released) is queued again with nothing admitted, and one
    /// released after its take is refused as a duplicate.
    #[tokio::test]
    async fn test_a_final_destination_persists_before_delivery_and_rejects_a_duplicate() {
        let (swarm, inbox) = claimed_swarm(None);
        let delivery = LocalDelivery::new(swarm.transport.clone());
        let payload = payload(swarm.did(), 0).expect("payload");

        for pass in ["first delivery", "delivery still in custody"] {
            assert!(matches!(
                delivery.deliver(&payload).await.expect(pass),
                LocalDelivered::Queued(_)
            ));
        }
        for _ in 0..2 {
            assert!(matches!(
                inbox.try_next(),
                Some(crate::swarm::application::ApplicationDelivery::Message(_))
            ));
        }
        swarm
            .transport
            .release_relayed_transaction(&payload.transaction)
            .await
            .expect("release");
        assert!(matches!(
            delivery.deliver(&payload).await,
            Err(Error::TransactionReplay { .. })
        ));
        assert_eq!(inbox.try_next(), None);
    }

    /// The custody law across a restart (#936): relay mail admitted and queued, then lost with
    /// the process before the application took it, is delivered once by the next run over the
    /// same replay store, and refused once that run released it.
    #[tokio::test]
    async fn test_a_restart_between_queueing_and_taking_redelivers_exactly_once() {
        let local = DelegateeKey::new_with_seckey(&SecretKey::random()).expect("local session");
        let replay = std::sync::Arc::new(MemStorage::<crate::message::ReplayRecord>::new());
        let run = || {
            let swarm = SwarmBuilder::new(
                TEST_NETWORK_ID,
                "",
                Box::new(MemStorage::new()),
                local.clone(),
            )
            .replay_storage(Box::new(std::sync::Arc::clone(&replay)))
            .build();
            let inbox = swarm.application_inbox().expect("unclaimed inbox");
            (swarm, inbox)
        };
        let payload = {
            let (swarm, _untaken) = run();
            let payload = payload(swarm.did(), 0).expect("payload");
            let delivered = LocalDelivery::new(swarm.transport.clone())
                .deliver(&payload)
                .await
                .expect("first run delivers");
            assert!(matches!(delivered, LocalDelivered::Queued(_)));
            payload
        };

        let (swarm, inbox) = run();
        let delivery = LocalDelivery::new(swarm.transport.clone());
        assert!(matches!(
            delivery.deliver(&payload).await.expect("redelivered"),
            LocalDelivered::Queued(_)
        ));
        assert!(matches!(
            inbox.try_next(),
            Some(crate::swarm::application::ApplicationDelivery::Message(_))
        ));
        swarm
            .transport
            .release_relayed_transaction(&payload.transaction)
            .await
            .expect("release");
        drop((swarm, inbox));

        let (swarm, inbox) = run();
        assert!(matches!(
            LocalDelivery::new(swarm.transport.clone())
                .deliver(&payload)
                .await,
            Err(Error::TransactionReplay { .. })
        ));
        assert_eq!(inbox.try_next(), None);
    }

    /// Room in the application inbox is reserved before admission: a payload deferred for want
    /// of room is neither admitted nor charged, so the later pass that delivers it is not
    /// refused as a replay or by the quota.
    #[tokio::test]
    async fn test_a_deferred_local_delivery_commits_no_admission() {
        let sender = DelegateeKey::new_with_seckey(&SecretKey::random()).expect("sender session");
        let lane =
            OriginQuotaLaneConfig::new(1, 1, 1024, 1024, 8).expect("test quota configuration");
        let (swarm, inbox) = claimed_swarm(Some(OriginQuotaConfig::new(lane, lane, lane, lane)));
        let delivery = LocalDelivery::new(swarm.transport.clone());
        let filler = payload(swarm.did(), 0).expect("filler payload");
        let fill = crate::swarm::application::ApplicationMessage::project(
            &filler,
            Message::CustomMessage(crate::message::CustomMessage(vec![
                0;
                crate::swarm::application::APPLICATION_INBOX_WEIGHT
            ])),
        )
        .expect("application data");
        swarm.transport.application().outlet.deliver(fill);
        let first = payload_from(&sender, swarm.did(), 0).expect("first payload");
        let second = payload_from(&sender, swarm.did(), 1).expect("second payload");

        assert!(matches!(
            delivery.deliver(&first).await.expect("deferred"),
            LocalDelivered::Deferred
        ));
        assert!(inbox.try_next().is_some());
        assert!(matches!(
            delivery.deliver(&first).await.expect("delivered"),
            LocalDelivered::Queued(_)
        ));
        assert!(matches!(
            delivery.deliver(&second).await,
            Err(Error::OriginQuota(
                OriginQuotaError::MessageRateExhausted { .. }
            ))
        ));
    }

    /// Custody follows the carrier (#936): a drain pass that finds no carrier for this node, as
    /// after its mail expired untaken or on a node with no relay store, restricts custody to
    /// nothing, so a delivery in custody no longer outlives the mail it kept.
    #[tokio::test]
    async fn test_a_pass_without_a_carrier_empties_custody() {
        use crate::dht::InboxDelivery;

        let (swarm, inbox) = claimed_swarm(None);
        let delivery = LocalDelivery::new(swarm.transport.clone());
        let payload = payload(swarm.did(), 0).expect("payload");
        assert!(matches!(
            delivery.deliver(&payload).await.expect("in custody"),
            LocalDelivered::Queued(_)
        ));
        assert!(inbox.try_next().is_some());

        crate::swarm::inbox::SwarmInboxDelivery::new(swarm.transport.clone())
            .deliver_inbox()
            .await
            .expect("a pass with no carrier");
        assert!(matches!(
            delivery.deliver(&payload).await,
            Err(Error::TransactionReplay { .. })
        ));
    }

    #[tokio::test]
    async fn test_an_intermediate_relay_creates_no_origin_replay_state() {
        let local = DelegateeKey::new_with_seckey(&SecretKey::random()).expect("local session");
        let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
            .replay_storage(crate::tests::volatile_replay_storage())
            .build();
        let logical = LogicalInbound::new(swarm.transport.clone());
        let remote_destination = SecretKey::random().address().into();
        let payload = payload(remote_destination, 0).expect("payload");
        let message = payload.transaction.data::<Message>().expect("message");

        for pass in ["first relay pass", "second relay pass"] {
            logical
                .admit_final_transaction(
                    &payload,
                    &message,
                    crate::swarm::callback::InboundLane::Application,
                    Arrival {
                        peer: None,
                        authentication: Authentication::Unauthenticated,
                    },
                )
                .await
                .expect(pass);
        }
        assert_eq!(swarm.transaction_replay_counters(), Default::default());
        assert_eq!(swarm.origin_quota_counters(), Default::default());
        assert_eq!(
            swarm.transport.origin_quota_record_count_for_test().await,
            0
        );
    }
}
