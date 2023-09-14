//! This module contains the [ConnectionRef] struct.

use std::sync::Arc;
use std::sync::Weak;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::core::transport::ConnectionInterface;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::error::Error;
use crate::error::Result;

/// The[ConnectionRef] is a weak reference to a connection and implements the `ConnectionInterface` trait.
/// When the connection is dropped, it returns an error called [Error::ConnectionReleased].
/// It serves as the return value for the `get_connection` method of [Transport](crate::Transport).
pub struct ConnectionRef<C> {
    cid: String,
    conn: Weak<C>,
}

impl<C> Clone for ConnectionRef<C> {
    fn clone(&self) -> Self {
        Self {
            cid: self.cid.clone(),
            conn: self.conn.clone(),
        }
    }
}

impl<C> ConnectionRef<C> {
    /// Create a new connection reference.
    pub fn new(cid: &str, conn: &Arc<C>) -> Self {
        Self {
            cid: cid.to_string(),
            conn: Arc::downgrade(conn),
        }
    }

    pub(crate) fn upgrade(&self) -> Result<Arc<C>> {
        match self.conn.upgrade() {
            Some(conn) => Ok(conn),
            None => Err(Error::ConnectionReleased(self.cid.clone())),
        }
    }
}

#[cfg(feature = "web-sys-webrtc")]
#[async_trait(?Send)]
impl<C, S> ConnectionInterface for ConnectionRef<C>
where
    C: ConnectionInterface<Error = Error, Sdp = S>,
    for<'async_trait> S: Serialize + DeserializeOwned + Send + Sync + 'async_trait,
{
    type Sdp = C::Sdp;
    type Error = C::Error;

    async fn send_message(&self, msg: TransportMessage) -> Result<()> {
        self.upgrade()?.send_message(msg).await
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.upgrade()
            .map(|c| c.webrtc_connection_state())
            .unwrap_or(WebrtcConnectionState::Closed)
    }

    async fn webrtc_create_offer(&self) -> Result<Self::Sdp> {
        self.upgrade()?.webrtc_create_offer().await
    }

    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp> {
        self.upgrade()?.webrtc_answer_offer(offer).await
    }

    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<()> {
        self.upgrade()?.webrtc_accept_answer(answer).await
    }

    async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
        self.upgrade()?.webrtc_wait_for_data_channel_open().await
    }

    async fn close(&self) -> Result<()> {
        self.upgrade()?.close().await
    }
}

#[cfg(not(feature = "web-sys-webrtc"))]
#[async_trait]
impl<C, S> ConnectionInterface for ConnectionRef<C>
where
    C: ConnectionInterface<Error = Error, Sdp = S> + Send + Sync,
    for<'async_trait> S: Serialize + DeserializeOwned + Send + Sync + 'async_trait,
{
    type Sdp = C::Sdp;
    type Error = C::Error;

    async fn send_message(&self, msg: TransportMessage) -> Result<()> {
        self.upgrade()?.send_message(msg).await
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.upgrade()
            .map(|c| c.webrtc_connection_state())
            .unwrap_or(WebrtcConnectionState::Closed)
    }

    async fn webrtc_create_offer(&self) -> Result<Self::Sdp> {
        self.upgrade()?.webrtc_create_offer().await
    }

    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp> {
        self.upgrade()?.webrtc_answer_offer(offer).await
    }

    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<()> {
        self.upgrade()?.webrtc_accept_answer(answer).await
    }

    async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
        self.upgrade()?.webrtc_wait_for_data_channel_open().await
    }

    async fn close(&self) -> Result<()> {
        self.upgrade()?.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::callback::Callback;
    use crate::core::transport::ConnectionCreation;
    use crate::Transport;

    struct TestCallback;

    #[cfg_attr(feature = "web-sys-webrtc", async_trait(?Send))]
    #[cfg_attr(not(feature = "web-sys-webrtc"), async_trait)]
    impl Callback for TestCallback {
        async fn on_message(
            &self,
            _cid: &str,
            _msg: &[u8],
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        async fn on_peer_connection_state_change(
            &self,
            _cid: &str,
            _state: WebrtcConnectionState,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_connection_ref_clonable() {
        let trans = Transport::new("stun://stun.l.google.com:19302", None);
        let callback = Arc::new(TestCallback.boxed());
        trans.new_connection("test", callback).await.unwrap();
        #[allow(clippy::redundant_clone)]
        let _ = trans.get_connection("test").unwrap().clone();
    }
}
