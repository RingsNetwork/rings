use async_trait::async_trait;
use std::sync::Arc;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageListener {
    async fn listen(self: Arc<Self>);
}
