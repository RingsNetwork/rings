use async_trait::async_trait;

use crate::message::CustomMessage;
use crate::message::Message;
use crate::message::MessageHandlerEvent;
use crate::message::MessagePayload;
use crate::prelude::RTCIceConnectionState;

#[macro_export]
macro_rules! boxed_type {
    // Type define.
    ($t:ident, $bt:ident) => {
        #[cfg(not(feature = "wasm"))]
        pub type $bt = Box<dyn $t + Send + Sync>;
        #[cfg(feature = "wasm")]
        pub type $bt = Box<dyn $t>;
    };
    // Inject a `boxed` method to trait.
    ($bt:ty) => {
        #[cfg(not(feature = "wasm"))]
        fn boxed(self) -> $bt
        where Self: Sized + Send + Sync + 'static {
            Box::new(self)
        }
        #[cfg(feature = "wasm")]
        fn boxed(self) -> $bt
        where Self: Sized + 'static {
            Box::new(self)
        }
    };
}

/// Trait of message callback.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TransportCallback {
    boxed_type!(BoxedTransportCallback);

    async fn on_ice_connection_state_change(&self, state: RTCIceConnectionState);
}

/// Trait of message callback.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageCallback {
    boxed_type!(BoxedMessageCallback);

    /// Message handler for custom message
    async fn custom_message(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &CustomMessage,
    ) -> Vec<MessageHandlerEvent>;
    /// Message handler for builtin message
    async fn builtin_message(&self, ctx: &MessagePayload<Message>) -> Vec<MessageHandlerEvent>;
}

/// Trait of message validator.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageValidator {
    boxed_type!(BoxedMessageValidator);

    /// Externality validator
    async fn validate(&self, ctx: &MessagePayload<Message>) -> Option<String>;
}

boxed_type!(TransportCallback, BoxedTransportCallback);
boxed_type!(MessageCallback, BoxedMessageCallback);
boxed_type!(MessageValidator, BoxedMessageValidator);
