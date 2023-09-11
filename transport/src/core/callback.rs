use async_trait::async_trait;

use crate::core::transport::WebrtcConnectionState;

type CallbackError = Box<dyn std::error::Error>;

#[cfg_attr(feature = "web-sys-webrtc", async_trait(?Send))]
#[cfg_attr(not(feature = "web-sys-webrtc"), async_trait)]
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

#[cfg(feature = "web-sys-webrtc")]
pub type BoxedCallback = Box<dyn Callback>;
#[cfg(not(feature = "web-sys-webrtc"))]
pub type BoxedCallback = Box<dyn Callback + Send + Sync>;
