use async_trait::async_trait;

use crate::message::CustomMessage;
use crate::message::Message;
use crate::message::MessageHandlerEvent;
use crate::message::MessagePayload;

/// Trait of message callback.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageCallback {
    /// Message handler for custom message
    async fn custom_message(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &CustomMessage,
    ) -> Vec<MessageHandlerEvent>;
    /// Message handler for builtin message
    async fn builtin_message(&self, ctx: &MessagePayload<Message>) -> Vec<MessageHandlerEvent>;

    #[cfg(not(feature = "wasm"))]
    fn boxed(self) -> BoxedMessageCallback
    where Self: Sized + Send + Sync + 'static {
        Box::new(self)
    }
    #[cfg(feature = "wasm")]
    fn boxed(self) -> BoxedValidator
    where Self: Sized + 'static {
        Box::new(self)
    }
}

/// Trait of message validator.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageValidator {
    /// Externality validator
    async fn validate(&self, ctx: &MessagePayload<Message>) -> Option<String>;

    #[cfg(not(feature = "wasm"))]
    fn boxed(self) -> BoxedMessageValidator
    where Self: Sized + Send + Sync + 'static {
        Box::new(self)
    }
    #[cfg(feature = "wasm")]
    fn boxed(self) -> BoxedMessageValidator
    where Self: Sized + 'static {
        Box::new(self)
    }
}

/// Boxed Callback, for non-wasm, it should be Sized, Send and Sync.
#[cfg(not(feature = "wasm"))]
pub type BoxedMessageCallback = Box<dyn MessageCallback + Send + Sync>;

/// Boxed Callback
#[cfg(feature = "wasm")]
pub type BoxedMessageCallback = Box<dyn MessageCallback>;

/// Boxed Validator
#[cfg(not(feature = "wasm"))]
pub type BoxedMessageValidator = Box<dyn MessageValidator + Send + Sync>;

/// Boxed Validator, for non-wasm, it should be Sized, Send and Sync.
#[cfg(feature = "wasm")]
pub type BoxedMessageValidator = Box<dyn MessageValidator>;
