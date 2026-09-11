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

    pub(super) async fn on_inbound(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), CallbackError> {
        self.callback.on_inbound(payload).await
    }
}
