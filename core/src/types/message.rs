use std::sync::Arc;

use async_trait::async_trait;

/// MessageListener trait implement `listen` method, use for MessageHandler and wait message.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageListener {
    async fn listen(self: Arc<Self>);
}
