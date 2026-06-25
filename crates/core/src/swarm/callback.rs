use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use futures::lock::Mutex as FuturesMutex;
use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::WebrtcConnectionState;

use crate::chunk::MessageReassembler;
use crate::dht::Did;
use crate::message::HandleMsg;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::swarm::transport::SwarmTransport;

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
    reassembler: FuturesMutex<MessageReassembler>,
}

impl InnerSwarmCallback {
    /// Create a new [InnerSwarmCallback] with the provided transport and callback.
    pub fn new(transport: Arc<SwarmTransport>, callback: SharedSwarmCallback) -> Self {
        let message_handler = MessageHandler::new(transport.clone(), callback.clone());
        Self {
            transport,
            message_handler,
            callback,
            reassembler: Default::default(),
        }
    }

    async fn handle_payload(
        &self,
        cid: &str,
        payload: &MessagePayload,
    ) -> Result<(), CallbackError> {
        let message: Message = payload.transaction.data()?;

        let result = match &message {
            Message::ConnectNodeSend(ref msg) => self.message_handler.handle(payload, msg).await,
            Message::ConnectNodeReport(ref msg) => self.message_handler.handle(payload, msg).await,
            Message::FindSuccessorSend(ref msg) => self.message_handler.handle(payload, msg).await,
            Message::FindSuccessorReport(ref msg) => {
                self.message_handler.handle(payload, msg).await
            }
            Message::NotifyPredecessorSend(ref msg) => {
                self.message_handler.handle(payload, msg).await
            }
            Message::NotifyPredecessorReport(ref msg) => {
                self.message_handler.handle(payload, msg).await
            }
            Message::SearchEntry(ref msg) => self.message_handler.handle(payload, msg).await,
            Message::FoundEntry(ref msg) => self.message_handler.handle(payload, msg).await,
            Message::SyncEntriesWithSuccessor(ref msg) => {
                self.message_handler.handle(payload, msg).await
            }
            Message::SyncEntriesWithSuccessorReport(ref msg) => {
                self.message_handler.handle(payload, msg).await
            }
            Message::OperateEntry(ref msg) => self.message_handler.handle(payload, msg).await,
            Message::CustomMessage(ref msg) => self.message_handler.handle(payload, msg).await,
            Message::QueryForTopoInfoSend(ref msg) => {
                self.message_handler.handle(payload, msg).await
            }
            Message::QueryForTopoInfoReport(ref msg) => {
                self.message_handler.handle(payload, msg).await
            }
            Message::Chunk(ref msg) => {
                // A chunk is an internal framing envelope, never an application message. When it
                // completes a payload, re-enter with the reassembled bytes; when it does not, there
                // is nothing to deliver. Either way we return here so the raw chunk envelope is
                // *never* passed to `on_inbound` (the app only ever sees reassembled messages).
                if let Some(data) = self.reassembler.lock().await.handle(msg.clone()) {
                    return self.on_message(cid, &data).await;
                }
                return Ok(());
            }
        };

        // A handler that errored must not then be reported to the application as a successful
        // inbound message: surface the error and do not run `on_inbound` for it.
        if let Err(e) = result {
            tracing::error!("Failed to handle_payload: {e:?}");
            return Err(e.into());
        }

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
            // `Failed` and `Closed` are terminal states, so we remove the peer
            // from the DHT here.
            WebrtcConnectionState::Failed | WebrtcConnectionState::Closed => {
                self.message_handler.leave_dht(did).await?;
            }
            // `Disconnected` is a transient ICE state that frequently recovers
            // back to `Connected` on its own (e.g. a brief network blip or ICE
            // consent refresh). Tearing the connection down here would kill a
            // link that WebRTC could have healed, and drop the peer from the DHT
            // with no reconnect path. We leave it alone: it will either recover,
            // or degrade to `Failed`, which is handled above.
            WebrtcConnectionState::Disconnected => {
                tracing::info!("Connection to {did} is disconnected, waiting for recovery");
            }
            _ => {}
        };

        // Should use the `on_data_channel_open` function to notify the Connected state.
        // It prevents users from blocking the channel creation while
        // waiting for data channel opening in send_message.
        if s != WebrtcConnectionState::Connected {
            self.callback
                .on_event(&SwarmEvent::ConnectionStateChange {
                    peer: did,
                    state: s,
                })
                .await?
        }

        Ok(())
    }

    async fn on_data_channel_open(&self, cid: &str) -> Result<(), CallbackError> {
        let Ok(did) = Did::from_str(cid) else {
            tracing::warn!("on_data_channel_open parse did failed: {}", cid);
            return Ok(());
        };

        self.message_handler.join_dht(did).await?;

        // Notify Connected state here instead of on_peer_connection_state_change.
        // It prevents users from blocking the channel creation while
        // waiting for data channel opening in send_message.
        self.callback
            .on_event(&SwarmEvent::ConnectionStateChange {
                peer: did,
                state: WebrtcConnectionState::Connected,
            })
            .await
    }

    async fn on_data_channel_close(&self, cid: &str) -> Result<(), CallbackError> {
        let Ok(did) = Did::from_str(cid) else {
            tracing::warn!("on_data_channel_close parse did failed: {}", cid);
            return Ok(());
        };

        // The data channel closing is a reliable signal that the peer is gone
        // (e.g. it closed the connection), so tear the connection down now
        // instead of waiting for the ICE state to reach `Failed`. This is the
        // graceful counterpart to a local `disconnect()`: the remote learns of
        // it promptly without relying on the transient `Disconnected` state.
        self.message_handler.leave_dht(did).await?;
        Ok(())
    }
}
