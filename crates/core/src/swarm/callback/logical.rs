use std::sync::Arc;

use super::inbound;
use super::LocalDelivery;
use super::LogicalInbound;
use crate::dht::Did;
use crate::measure::Authentication;
use crate::message::with_message_variants;
use crate::message::CustomMessage;
use crate::message::EdgeRelation;
use crate::message::HandleMsg;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessageKind;
use crate::message::MessagePayload;
use crate::message::OriginQuotaCharge;
use crate::swarm::application::ApplicationMessage;
use crate::swarm::application::Delivered;
use crate::swarm::application::InboxRoom;
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

    /// Deliver one payload.
    ///
    /// Pre: `payload.transaction.destination` is this node and the payload was verified by the
    /// caller (the inbox witness verifies it as of its hold instant).
    pub(crate) async fn deliver(
        &self,
        payload: &MessagePayload,
    ) -> crate::error::Result<inbound::LocalDelivered> {
        inbound::deliver_local(&self.pipeline, payload).await
    }
}

/// Relate the connection that delivered `payload` to its origin account.
///
/// Only a handshake-authenticated connection names a neighbour; an unauthenticated or unknown
/// peer is never [`EdgeRelation::Neighbour`], whatever DID it claims.
pub(super) fn edge_relation(
    peer: Option<Did>,
    authentication: Authentication,
    payload: &MessagePayload,
) -> EdgeRelation {
    let authenticated = matches!(authentication, Authentication::Authenticated);
    EdgeRelation::of(peer.filter(|_| authenticated), payload.transaction.origin())
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

    /// Dispatch `payload` to its handler and record the observation; returns the decoded body.
    pub(super) async fn handle_payload(
        &self,
        payload: &MessagePayload,
        prepared_message: Option<Message>,
    ) -> crate::error::Result<Message> {
        let message = match prepared_message {
            Some(message) => message,
            None => payload.transaction.data()?,
        };
        let message_kind = MessageKind::from_message(&message);
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
                self.message_handler.handle(payload, $msg.as_ref()).await
            };
            (ProbeAcknowledgement, $msg:expr) => {
                self.message_handler.handle(payload, $msg.as_ref()).await
            };
            ($variant:ident, $msg:expr) => {
                self.message_handler.handle(payload, $msg).await
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

        let outcome = if result.is_ok() {
            ObservationOutcome::Succeeded
        } else {
            ObservationOutcome::Failed
        };
        self.transport.observe_message(MessageObservation {
            activity: MessageActivity::Received,
            category: message_kind.class(),
            message_class: message_kind.as_str(),
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

        // A handler that errored must not then be handed to the application as a successful
        // inbound message: surface the error and deliver nothing.
        if let Err(e) = result {
            tracing::error!("Failed to handle_payload: {e:?}");
            return Err(e);
        }

        Ok(message)
    }

    pub(super) fn is_local_destination(&self, payload: &MessagePayload) -> bool {
        payload.transaction.destination == self.transport.dht.did
    }

    /// Commit replay and origin-quota admission of a transaction addressed to this node.
    ///
    /// The charge is the message's class record, with its message limit skipped only when
    /// `edge` is [`EdgeRelation::Neighbour`] and the namespace delegated its admission (see
    /// [`OriginQuotaCharge::select`]). Transactions for other destinations are not admitted here.
    pub(super) async fn admit_final_transaction(
        &self,
        payload: &MessagePayload,
        message: &Message,
        lane: crate::swarm::callback::InboundLane,
        edge: EdgeRelation,
    ) -> crate::error::Result<()> {
        if self.is_local_destination(payload) {
            let class = lane
                .class()
                .ok_or(crate::error::Error::InboundActorInvariantViolation)?;
            let charge =
                OriginQuotaCharge::select(class, edge, || self.delegates_admission(message));
            self.transport
                .admit_final_transaction(&payload.transaction, charge)
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

    /// Test builds: show `payload`, about to enter the logical stage, to the inbound tap.
    #[cfg(test)]
    pub(super) fn observe_for_test(&self, payload: &MessagePayload) {
        self.transport.inbound_tap_for_test().observe(payload);
    }

    /// Outside test builds there is no inbound tap: nothing to observe.
    #[cfg(not(test))]
    pub(super) fn observe_for_test(&self, _payload: &MessagePayload) {}

    /// Test builds: wait while a test holds `lane` (see the inbound tap).
    #[cfg(test)]
    pub(super) async fn pass_lane_for_test(&self, lane: crate::swarm::callback::InboundLane) {
        self.transport.inbound_tap_for_test().pass_lane(lane).await;
    }

    /// Outside test builds no lane is held.
    #[cfg(not(test))]
    pub(super) async fn pass_lane_for_test(&self, _lane: crate::swarm::callback::InboundLane) {}

    /// Room for `message` in the application inbox, if it has room now. Never waits.
    pub(super) fn reserve_room(&self, message: &ApplicationMessage) -> Option<InboxRoom> {
        self.transport.application().outlet.try_reserve(message)
    }

    /// Hand `message` to the application inbox, queued or, when full, dropped and counted;
    /// never waits (#913 R10).
    pub(super) fn deliver(&self, message: ApplicationMessage) -> Delivered {
        self.transport.application().outlet.deliver(message)
    }
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
        let builder = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local);
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

    #[tokio::test]
    async fn test_a_final_destination_persists_before_delivery_and_rejects_a_duplicate() {
        let (swarm, inbox) = claimed_swarm(None);
        let delivery = LocalDelivery::new(swarm.transport.clone());
        let payload = payload(swarm.did(), 0).expect("payload");

        assert!(matches!(
            delivery.deliver(&payload).await.expect("first delivery"),
            inbound::LocalDelivered::Queued(_)
        ));
        assert!(matches!(
            delivery.deliver(&payload).await,
            Err(Error::TransactionReplay { .. })
        ));
        assert!(matches!(
            inbox.try_next(),
            Some(crate::swarm::application::ApplicationDelivery::Message(_))
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
            inbound::LocalDelivered::Deferred
        ));
        assert!(inbox.try_next().is_some());
        assert!(matches!(
            delivery.deliver(&first).await.expect("delivered"),
            inbound::LocalDelivered::Queued(_)
        ));
        assert!(matches!(
            delivery.deliver(&second).await,
            Err(Error::OriginQuota(
                OriginQuotaError::MessageRateExhausted { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn test_an_intermediate_relay_creates_no_origin_replay_state() {
        let local = DelegateeKey::new_with_seckey(&SecretKey::random()).expect("local session");
        let swarm =
            SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local).build();
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
                    EdgeRelation::Remote,
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
