use std::sync::Arc;

use super::inbound;
use super::CallbackError;
use super::LocalDelivery;
use super::LogicalInbound;
use super::SharedSwarmCallback;
use crate::message::with_message_variants;
use crate::message::HandleMsg;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::swarm::transport::SwarmTransport;

impl LocalDelivery {
    /// Deliver to `callback` through `transport`'s handlers.
    pub(crate) fn new(transport: Arc<SwarmTransport>, callback: SharedSwarmCallback) -> Self {
        Self {
            pipeline: LogicalInbound::new(transport, callback),
        }
    }

    /// Deliver one payload.
    ///
    /// Pre: `payload.transaction.destination` is this node and the payload was verified by the
    /// caller (the inbox witness verifies it as of its hold instant).
    pub(crate) async fn deliver(&self, payload: &MessagePayload) -> crate::error::Result<()> {
        inbound::deliver_local(&self.pipeline, payload).await
    }
}

impl LogicalInbound {
    pub(super) fn new(transport: Arc<SwarmTransport>, callback: SharedSwarmCallback) -> Self {
        let message_handler = MessageHandler::new(transport.clone(), callback.clone());
        Self {
            transport,
            message_handler,
            callback,
        }
    }

    pub(super) async fn handle_payload(
        &self,
        payload: &MessagePayload,
        prepared_message: Option<Message>,
    ) -> crate::error::Result<()> {
        let message = match prepared_message {
            Some(message) => message,
            None => payload.transaction.data()?,
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

        // A handler that errored must not then be reported to the application as a successful
        // inbound message: surface the error and do not run `on_inbound` for it.
        if let Err(e) = result {
            tracing::error!("Failed to handle_payload: {e:?}");
            return Err(e);
        }

        Ok(())
    }

    pub(super) fn is_local_destination(&self, payload: &MessagePayload) -> bool {
        payload.transaction.destination == self.transport.dht.did
    }

    pub(super) async fn admit_final_transaction(
        &self,
        payload: &MessagePayload,
        lane: crate::swarm::callback::InboundLane,
    ) -> crate::error::Result<()> {
        if self.is_local_destination(payload) {
            let quota_lane = lane
                .origin_quota_lane()
                .ok_or(crate::error::Error::InboundActorInvariantViolation)?;
            self.transport
                .admit_final_transaction(&payload.transaction, quota_lane)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn on_inbound(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), CallbackError> {
        self.callback.on_inbound(payload).await
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use super::*;
    use crate::ecc::SecretKey;
    use crate::error::Error;
    use crate::message::Message;
    use crate::message::MessageSigner;
    use crate::message::OriginQuotaConfig;
    use crate::message::OriginQuotaError;
    use crate::message::OriginQuotaLaneConfig;
    use crate::session::SessionSk;
    use crate::storage::MemStorage;
    use crate::swarm::SwarmBuilder;
    use crate::tests::TEST_NETWORK_ID;

    #[derive(Default)]
    struct ObservedCallback {
        validations: AtomicUsize,
        inbounds: AtomicUsize,
    }

    #[derive(Default)]
    struct RejectingValidationCallback {
        validations: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::swarm::callback::SwarmCallback for RejectingValidationCallback {
        async fn on_validate(
            &self,
            _payload: &MessagePayload,
        ) -> std::result::Result<(), CallbackError> {
            self.validations.fetch_add(1, Ordering::Relaxed);
            Err(std::io::Error::other("rejected by application").into())
        }
    }

    #[async_trait::async_trait]
    impl crate::swarm::callback::SwarmCallback for ObservedCallback {
        async fn on_validate(
            &self,
            _payload: &MessagePayload,
        ) -> std::result::Result<(), CallbackError> {
            self.validations.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn on_inbound(
            &self,
            _payload: &MessagePayload,
        ) -> std::result::Result<(), CallbackError> {
            self.inbounds.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn payload(
        destination: crate::dht::Did,
        sequence: u64,
    ) -> crate::error::Result<MessagePayload> {
        let sender = SessionSk::new_with_seckey(&SecretKey::random())?;
        MessagePayload::new_send_with_sequence(
            Message::custom(b"replay boundary")?,
            MessageSigner::new(&sender, TEST_NETWORK_ID),
            destination,
            destination,
            sequence,
        )
    }

    fn payload_from(
        sender: &SessionSk,
        destination: crate::dht::Did,
        sequence: u64,
    ) -> crate::error::Result<MessagePayload> {
        MessagePayload::new_send_with_sequence(
            Message::custom(b"quota before validation")?,
            MessageSigner::new(sender, TEST_NETWORK_ID),
            destination,
            destination,
            sequence,
        )
    }

    #[tokio::test]
    async fn final_destination_persists_before_validation_and_rejects_duplicate_dispatch() {
        let local = SessionSk::new_with_seckey(&SecretKey::random()).expect("local session");
        let callback = Arc::new(ObservedCallback::default());
        let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
            .callback(callback.clone())
            .build();
        let delivery = LocalDelivery::new(swarm.transport.clone(), callback.clone());
        let payload = payload(swarm.did(), 0).expect("payload");

        delivery.deliver(&payload).await.expect("first delivery");
        assert!(matches!(
            delivery.deliver(&payload).await,
            Err(Error::TransactionReplay { .. })
        ));
        assert_eq!(callback.validations.load(Ordering::Relaxed), 1);
        assert_eq!(callback.inbounds.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn application_validation_failure_still_consumes_committed_quota() {
        let local = SessionSk::new_with_seckey(&SecretKey::random()).expect("local session");
        let sender = SessionSk::new_with_seckey(&SecretKey::random()).expect("sender session");
        let callback = Arc::new(RejectingValidationCallback::default());
        let lane =
            OriginQuotaLaneConfig::new(1, 1, 1024, 1024, 8).expect("test quota configuration");
        let quota = OriginQuotaConfig::new(lane, lane, lane, lane);
        let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
            .origin_quota(quota)
            .callback(callback.clone())
            .build();
        let delivery = LocalDelivery::new(swarm.transport.clone(), callback.clone());
        let first = payload_from(&sender, swarm.did(), 0).expect("first payload");
        let second = payload_from(&sender, swarm.did(), 1).expect("second payload");

        assert!(delivery.deliver(&first).await.is_err());
        assert!(matches!(
            delivery.deliver(&second).await,
            Err(Error::OriginQuota(
                OriginQuotaError::MessageRateExhausted { .. }
            ))
        ));
        assert_eq!(callback.validations.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn intermediate_relay_does_not_create_origin_replay_state() {
        let local = SessionSk::new_with_seckey(&SecretKey::random()).expect("local session");
        let callback = Arc::new(ObservedCallback::default());
        let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
            .callback(callback.clone())
            .build();
        let logical = LogicalInbound::new(swarm.transport.clone(), callback);
        let remote_destination = SecretKey::random().address().into();
        let payload = payload(remote_destination, 0).expect("payload");

        logical
            .admit_final_transaction(&payload, crate::swarm::callback::InboundLane::Application)
            .await
            .expect("first relay pass");
        logical
            .admit_final_transaction(&payload, crate::swarm::callback::InboundLane::Application)
            .await
            .expect("second relay pass");
        assert_eq!(swarm.transaction_replay_counters(), Default::default());
        assert_eq!(swarm.origin_quota_counters(), Default::default());
        assert_eq!(
            swarm.transport.origin_quota_record_count_for_test().await,
            0
        );
    }
}
