use async_trait::async_trait;

use crate::core::transport::WebrtcConnectionState;

type CallbackError = Box<dyn std::error::Error>;

#[async_trait]
pub trait Callback {
    fn boxed(self) -> BoxedCallback
    where Self: Sized + Send + Sync + 'static {
        Box::new(self)
    }

    async fn on_message(&self, cid: &str, msg: &[u8]) -> Result<(), CallbackError>;
    async fn on_peer_connection_state_change(
        &self,
        cid: &str,
        state: WebrtcConnectionState,
    ) -> Result<(), CallbackError>;
}

pub type BoxedCallback = Box<dyn Callback + Send + Sync>;
