use async_trait::async_trait;

use crate::core::transport::WebrtcConnectionState;

#[async_trait]
pub trait Callback {
    type Error: std::error::Error + Send + Sync + 'static;

    fn boxed(self) -> BoxedCallback<Self::Error>
    where Self: Sized + Send + Sync + 'static {
        Box::new(self)
    }

    async fn on_message(&self, cid: &str, msg: &[u8]) -> Result<(), Self::Error>;
    async fn on_peer_connection_state_change(
        &self,
        cid: &str,
        state: WebrtcConnectionState,
    ) -> Result<(), Self::Error>;
}

pub type BoxedCallback<E> = Box<dyn Callback<Error = E> + Send + Sync>;
