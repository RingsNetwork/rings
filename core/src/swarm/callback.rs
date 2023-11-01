use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::WebrtcConnectionState;

use crate::channels::Channel;
use crate::dht::Did;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::TransportEvent;

type TransportEventSender = <Channel<TransportEvent> as ChannelTrait<TransportEvent>>::Sender;
type CallbackError = Box<dyn std::error::Error>;

/// The [InnerSwarmCallback] will accept shared [SwarmCallback] trait object.
#[cfg(feature = "wasm")]
pub type SharedSwarmCallback = Arc<dyn SwarmCallback>;

/// The [InnerSwarmCallback] will accept shared [SwarmCallback] trait object.
#[cfg(not(feature = "wasm"))]
pub type SharedSwarmCallback = Arc<dyn SwarmCallback + Send + Sync>;

/// Used to notify the application of events that occur in the swarm.
#[derive(Debug)]
#[non_exhaustive]
pub enum SwarmEvent {
    /// Indicates that the connection state of a peer has changed.
    ConnectionStateChange {
        /// The did of remote peer.
        peer: Did,
        /// The final state of the connection.
        state: WebrtcConnectionState,
    },
}

/// Any object that implements this trait can be used as a callback for the swarm.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait SwarmCallback {
    /// This method is invoked when a new message is received and before handling.
    async fn on_validate(&self, _payload: &MessagePayload) -> Result<(), CallbackError> {
        Ok(())
    }

    /// This method is invoked when a new message is received and after handling.
    /// Will not be invoked if the message is not for this node.
    async fn on_payload(&self, _payload: &MessagePayload) -> Result<(), CallbackError> {
        Ok(())
    }

    /// This method is invoked after the Swarm handling.
    async fn on_event(&self, _event: &SwarmEvent) -> Result<(), CallbackError> {
        Ok(())
    }
}

pub(crate) struct InnerSwarmCallback {
    did: Did,
    transport_event_sender: TransportEventSender,
    callback: SharedSwarmCallback,
}

impl InnerSwarmCallback {
    pub fn new(
        did: Did,
        transport_event_sender: TransportEventSender,
        callback: SharedSwarmCallback,
    ) -> Self {
        Self {
            did,
            transport_event_sender,
            callback,
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TransportCallback for InnerSwarmCallback {
    async fn on_message(&self, _cid: &str, msg: &[u8]) -> Result<(), CallbackError> {
        let payload = MessagePayload::from_bincode(msg)?;
        if !(payload.verify() && payload.transaction.verify()) {
            tracing::error!("Cannot verify msg or it's expired: {:?}", payload);
            return Err("Cannot verify msg or it's expired".into());
        }

        self.callback.on_validate(&payload).await?;

        Channel::send(
            &self.transport_event_sender,
            TransportEvent::DataChannelMessage(msg.into()),
        )
        .await
        .map_err(Box::new)?;

        if payload.transaction.destination == self.did {
            self.callback.on_payload(&payload).await?;
        }

        Ok(())
    }

    async fn on_peer_connection_state_change(
        &self,
        cid: &str,
        s: WebrtcConnectionState,
    ) -> Result<(), CallbackError> {
        let Ok(did) = Did::from_str(cid) else {
            tracing::warn!("on_peer_connection_state_change parse did failed: {}", cid);
            return Ok(());
        };

        match s {
            WebrtcConnectionState::Connected => {
                Channel::send(&self.transport_event_sender, TransportEvent::Connected(did)).await
            }
            WebrtcConnectionState::Failed
            | WebrtcConnectionState::Disconnected
            | WebrtcConnectionState::Closed => {
                Channel::send(&self.transport_event_sender, TransportEvent::Closed(did)).await
            }
            _ => Ok(()),
        }
        .map_err(Box::new)?;

        self.callback
            .on_event(&SwarmEvent::ConnectionStateChange {
                peer: did,
                state: s,
            })
            .await
    }
}
