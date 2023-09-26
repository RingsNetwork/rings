//! This module contains the [InnerCallback] struct.

use std::sync::Arc;

use bytes::Bytes;

use crate::core::callback::BoxedCallback;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::notifier::Notifier;

/// [InnerCallback] wraps the [BoxedCallback] with inner handling for a specific connection.
pub struct InnerCallback {
    /// The id of the connection to which the current callback is assigned.
    pub cid: String,
    callback: Arc<BoxedCallback>,
    data_channel_open_notifier: Notifier,
}

impl InnerCallback {
    /// Create a new [InnerCallback].
    pub fn new(
        cid: &str,
        callback: Arc<BoxedCallback>,
        data_channel_open_notifier: Notifier,
    ) -> Self {
        Self {
            cid: cid.to_string(),
            callback,
            data_channel_open_notifier,
        }
    }

    /// Notify the data channel is open.
    pub fn on_data_channel_open(&self) {
        self.data_channel_open_notifier.set_result(true)
    }

    /// Notify the data channel is close.
    pub fn on_data_channel_close(&self) {
        self.data_channel_open_notifier.set_result(false)
    }

    /// This method is invoked on a binary message arrival over the data channel of webrtc.
    pub async fn on_message(&self, msg: &Bytes) {
        match bincode::deserialize(msg) {
            Ok(m) => self.handle_message(&m).await,
            Err(e) => {
                tracing::error!("Deserialize DataChannelMessage failed: {e:?}");
            }
        };
    }

    /// This method is invoked when the state of connection has changed.
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
