use std::sync::Arc;

use bytes::Bytes;

use crate::core::callback::BoxedCallback;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;

pub(crate) struct InnerCallback {
    pub cid: String,
    callback: Arc<BoxedCallback>,
}

impl InnerCallback {
    pub fn new(cid: &str, callback: Arc<BoxedCallback>) -> Self {
        Self {
            cid: cid.to_string(),
            callback,
        }
    }

    pub async fn on_message(&self, msg: &Bytes) {
        match bincode::deserialize(msg) {
            Ok(m) => self.handle_message(&m).await,
            Err(e) => {
                tracing::error!("Deserialize DataChannelMessage failed: {e:?}");
            }
        };
    }

    pub async fn on_peer_connection_state_change(&self, s: WebrtcConnectionState) {
        if let Err(e) = self
            .callback
            .on_peer_connection_state_change(&self.cid, s)
            .await
        {
            tracing::error!("Callback on_peer_connection_state_change failed: {e:?}");
        }
    }

    async fn handle_message(&self, msg: &TransportMessage) {
        match msg {
            TransportMessage::Custom(bytes) => {
                if let Err(e) = self.callback.on_message(&self.cid, bytes).await {
                    tracing::error!("Callback on_message failed: {e:?}")
                }
            }
        }
    }
}
