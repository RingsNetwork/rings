use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use futures::lock::Mutex as FuturesMutex;
use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::WebrtcConnectionState;

use crate::chunk::ChunkList;
use crate::chunk::ChunkManager;
use crate::consts::TRANSPORT_MTU;
use crate::dht::Did;
use crate::message::HandleMsg;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::transport::SwarmTransport;

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
    /// Will be invoked no matter if the message is for this node or not.
    async fn on_relay(&self, _payload: &MessagePayload) -> Result<(), CallbackError> {
        Ok(())
    }

    /// This method is invoked when a new message is received and after handling.
    /// Will not be invoked if the message is not for this node.
    async fn on_inbound(&self, _payload: &MessagePayload) -> Result<(), CallbackError> {
        Ok(())
    }

    /// This method is invoked after the Swarm handling.
    async fn on_event(&self, _event: &SwarmEvent) -> Result<(), CallbackError> {
        Ok(())
    }
}

/// [InnerSwarmCallback] wraps [SharedSwarmCallback] with inner handling for a specific connection.
pub struct InnerSwarmCallback {
    transport: Arc<SwarmTransport>,
    message_handler: MessageHandler,
    callback: SharedSwarmCallback,
    chunk_list: FuturesMutex<ChunkList<TRANSPORT_MTU>>,
}

impl InnerSwarmCallback {
    /// Create a new [InnerSwarmCallback] with the provided transport and callback.
    pub fn new(transport: Arc<SwarmTransport>, callback: SharedSwarmCallback) -> Self {
        let message_handler = MessageHandler::new(transport.clone(), callback.clone());
        Self {
            transport,
            message_handler,
            callback,
            chunk_list: Default::default(),
        }
    }

    async fn handle_payload(
        &self,
        cid: &str,
        payload: &MessagePayload,
    ) -> Result<(), CallbackError> {
        let message: Message = payload.transaction.data()?;

        match &message {
            Message::ConnectNodeSend(ref msg) => self.message_handler.handle(payload, msg).await?,
            Message::ConnectNodeReport(ref msg) => {
                self.message_handler.handle(payload, msg).await?
            }
            Message::FindSuccessorSend(ref msg) => {
                self.message_handler.handle(payload, msg).await?
            }
            Message::FindSuccessorReport(ref msg) => {
                self.message_handler.handle(payload, msg).await?
            }
            Message::NotifyPredecessorSend(ref msg) => {
                self.message_handler.handle(payload, msg).await?
            }
            Message::NotifyPredecessorReport(ref msg) => {
                self.message_handler.handle(payload, msg).await?
            }
            Message::SearchVNode(ref msg) => self.message_handler.handle(payload, msg).await?,
            Message::FoundVNode(ref msg) => self.message_handler.handle(payload, msg).await?,
            Message::SyncVNodeWithSuccessor(ref msg) => {
                self.message_handler.handle(payload, msg).await?
            }
            Message::OperateVNode(ref msg) => self.message_handler.handle(payload, msg).await?,
            Message::CustomMessage(ref msg) => self.message_handler.handle(payload, msg).await?,
            Message::QueryForTopoInfoSend(ref msg) => {
                self.message_handler.handle(payload, msg).await?
            }
            Message::QueryForTopoInfoReport(ref msg) => {
                self.message_handler.handle(payload, msg).await?
            }
            Message::Chunk(ref msg) => {
                if let Some(data) = self.chunk_list.lock().await.handle(msg.clone()) {
                    return self.on_message(cid, &data).await;
                }
            }
        };

        self.callback.on_relay(payload).await?;

        if payload.transaction.destination == self.transport.dht.did {
            self.callback.on_inbound(payload).await?;
        }

        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TransportCallback for InnerSwarmCallback {
    async fn on_message(&self, cid: &str, msg: &[u8]) -> Result<(), CallbackError> {
        let payload = MessagePayload::from_bincode(msg)?;
        if !(payload.verify() && payload.transaction.verify()) {
            tracing::error!("Cannot verify msg or it's expired: {:?}", payload);
            return Err("Cannot verify msg or it's expired".into());
        }
        self.callback.on_validate(&payload).await?;
        self.handle_payload(cid, &payload).await
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
                self.message_handler.join_dht(did).await?;
            }
            WebrtcConnectionState::Failed
            | WebrtcConnectionState::Disconnected
            | WebrtcConnectionState::Closed => {
                self.message_handler.leave_dht(did).await?;
            }
            _ => {}
        };

        self.callback
            .on_event(&SwarmEvent::ConnectionStateChange {
                peer: did,
                state: s,
            })
            .await
    }
}
