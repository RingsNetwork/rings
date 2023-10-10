use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::WebrtcConnectionState;

use crate::backend::RingsBackend;
use crate::channels::Channel;
use crate::dht::Did;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::TransportEvent;

type TransportEventSender = <Channel<TransportEvent> as ChannelTrait<TransportEvent>>::Sender;
type CallbackError = Box<dyn std::error::Error>;

pub enum SwarmEvent {
    ConnectionStateChange {
        peer: Did,
        state: WebrtcConnectionState,
    },
}

/// Any object that implements this trait can be used as a callback for the connection.
#[cfg_attr(feature = "swarm", async_trait(?Send))]
#[cfg_attr(not(feature = "swarm"), async_trait)]
pub trait SwarmCallback {
    /// Used to turn object into [BoxedTransportCallback] to be used
    /// in [ConnectionCreation](super::transport::ConnectionCreation)
    fn boxed(self) -> BoxedSwarmCallback
    where Self: Sized + Send + Sync + 'static {
        Box::new(self)
    }

    /// This method is invoked when a new message is received and before handling.
    async fn on_validate(&self, _payload: &MessagePayload<Message>) -> Result<(), CallbackError> {
        Ok(())
    }

    /// This method is invoked when a new message is received and after handling.
    async fn on_payload(&self, _payload: &MessagePayload<Message>) -> Result<(), CallbackError> {
        Ok(())
    }

    /// This method is invoked after the Swarm handling.
    async fn on_event(&self, _event: &SwarmEvent) -> Result<(), CallbackError> {
        Ok(())
    }
}

#[cfg(not(feature = "wasm"))]
pub type BoxedSwarmCallback = Box<dyn SwarmCallback + Send + Sync>;

#[cfg(feature = "wasm")]
pub type BoxedSwarmCallback = Box<dyn SwarmCallback>;

pub struct InnerSwarmCallback {
    transport_event_sender: TransportEventSender,
    backend: Arc<RingsBackend>,
    callback: BoxedSwarmCallback,
}

impl InnerSwarmCallback {
    pub fn new(
        transport_event_sender: TransportEventSender,
        backend: Arc<RingsBackend>,
        callback: BoxedSwarmCallback,
    ) -> Self {
        Self {
            transport_event_sender,
            backend,
            callback,
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TransportCallback for InnerSwarmCallback {
    async fn on_message(&self, _cid: &str, msg: &[u8]) -> Result<(), CallbackError> {
        let payload = MessagePayload::from_bincode(msg)?;

        if !payload.verify() {
            tracing::error!("Cannot verify msg or it's expired: {:?}", payload);
            return Err("Cannot verify msg or it's expired".into());
        }

        self.callback.on_validate(&payload).await?;

        Channel::send(
            &self.transport_event_sender,
            TransportEvent::DataChannelMessage(msg.into()),
        )
        .await?;

        self.callback.on_payload(&payload).await
    }

    async fn on_peer_connection_state_change(
        &self,
        cid: &str,
        state: WebrtcConnectionState,
    ) -> Result<(), CallbackError> {
        let Ok(peer) = Did::from_str(cid) else {
            tracing::warn!("on_peer_connection_state_change parse did failed: {}", cid);
            return Ok(());
        };

        match state {
            WebrtcConnectionState::Connected => {
                Channel::send(
                    &self.transport_event_sender,
                    TransportEvent::Connected(peer),
                )
                .await
            }
            WebrtcConnectionState::Failed
            | WebrtcConnectionState::Disconnected
            | WebrtcConnectionState::Closed => {
                Channel::send(&self.transport_event_sender, TransportEvent::Closed(peer)).await
            }
            _ => Ok(()),
        }
        .map_err(Box::new)?;

        self.callback
            .on_event(&SwarmEvent::ConnectionStateChange { peer, state })
            .await
    }
}
