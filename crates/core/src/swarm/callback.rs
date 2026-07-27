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
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;

type CallbackError = Box<dyn std::error::Error>;

/// The [InnerSwarmCallback] will accept shared [SwarmCallback] trait object.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub type SharedSwarmCallback = Arc<dyn SwarmCallback>;

/// The [InnerSwarmCallback] will accept shared [SwarmCallback] trait object.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
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
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
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
    pending_attempt: Option<PendingConnectionAttempt>,
}

impl InnerSwarmCallback {
    /// Create a new [InnerSwarmCallback] with the provided transport and callback.
    pub fn new(transport: Arc<SwarmTransport>, callback: SharedSwarmCallback) -> Self {
        let message_handler = MessageHandler::new(transport.clone(), callback.clone());
        let reassembler = MessageReassembler::with_limits(transport.reassembly_limits());
        Self {
            transport,
            message_handler,
            callback,
            reassembler: FuturesMutex::new(reassembler),
            pending_attempt: None,
        }
    }

    /// Bind this callback to the pending handshake that created its transport.
    pub(crate) fn with_pending_connection_attempt(
        mut self,
        pending_attempt: PendingConnectionAttempt,
    ) -> Self {
        self.pending_attempt = Some(pending_attempt);
        self
    }

    async fn admit_pending_connection(&self, did: Did) -> Result<bool, CallbackError> {
        let Some(attempt) = self.pending_attempt else {
            return Ok(false);
        };
        if attempt.peer() != did {
            tracing::warn!(
                "ignoring data-channel open for {did}; pending attempt belongs to {}",
                attempt.peer()
            );
            self.transport.cancel_pending_connection(attempt).await?;
            return Ok(false);
        }
        if !self.transport.promote_pending_connection(attempt)? {
            return Ok(false);
        }

        self.transport.record_peer_connected(did).await;
        if !self.transport.is_admitted_connection_attempt(attempt) {
            tracing::debug!(
                "aborting data-channel admission for {did}; connection was retired while recording connect"
            );
            return Ok(false);
        }
        self.message_handler.join_dht(did).await?;
        if !self.transport.is_admitted_connection_attempt(attempt) {
            tracing::debug!(
                "undoing DHT admission for {did}; connection was retired while joining DHT"
            );
            self.message_handler.leave_dht(did).await?;
            return Ok(false);
        }
        for index in self.transport.take_pending_finger_updates(attempt)? {
            self.transport.dht.apply_fixed_finger(index, did)?;
        }
        if !self.transport.is_admitted_connection_attempt(attempt) {
            tracing::debug!(
                "aborting connected event for {did}; connection was retired after DHT admission"
            );
            self.message_handler.leave_dht(did).await?;
            return Ok(false);
        }
        self.emit_connected_event_for_attempt(did, attempt).await
    }

    async fn emit_connected_event_for_attempt(
        &self,
        did: Did,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool, CallbackError> {
        let delivery = self.transport.swarm_event_delivery_lock(did);
        let result = async {
            let _delivery = delivery.lock().await;
            if !self.transport.is_admitted_connection_attempt(attempt) {
                tracing::debug!("suppressing connected event for {did}; connection was retired before event delivery");
                return Ok(false);
            }
            self.emit_connection_state_change_unlocked(did, WebrtcConnectionState::Connected)
                .await?;
            Ok(true)
        }
        .await;
        self.transport
            .prune_swarm_event_delivery_lock(did, &delivery);
        result
    }

    async fn emit_connection_state_change(
        &self,
        did: Did,
        state: WebrtcConnectionState,
    ) -> Result<(), CallbackError> {
        let delivery = self.transport.swarm_event_delivery_lock(did);
        let result = async {
            let _delivery = delivery.lock().await;
            self.emit_connection_state_change_unlocked(did, state).await
        }
        .await;
        self.transport
            .prune_swarm_event_delivery_lock(did, &delivery);
        result
    }

    async fn emit_connection_state_change_unlocked(
        &self,
        did: Did,
        state: WebrtcConnectionState,
    ) -> Result<(), CallbackError> {
        self.callback
            .on_event(&SwarmEvent::ConnectionStateChange { peer: did, state })
            .await
    }

    fn pending_disconnected_before_admission(&self, did: Did) -> bool {
        let Some(attempt) = self.pending_attempt else {
            return false;
        };
        attempt.peer() == did && !self.transport.is_admitted_connection_attempt(attempt)
    }

    fn is_local_did_event(&self, did: Did, operation: &str) -> bool {
        if did != self.transport.dht.did {
            return false;
        }
        tracing::warn!("ignoring {operation} for local DID {did}");
        true
    }

    async fn cancel_mismatched_pending_connection(
        &self,
        did: Did,
        operation: &str,
    ) -> Result<bool, CallbackError> {
        let Some(attempt) = self.pending_attempt else {
            return Ok(false);
        };
        if attempt.peer() == did {
            return Ok(false);
        }
        tracing::warn!(
            "ignoring {operation} for {did}; pending attempt belongs to {}",
            attempt.peer()
        );
        if self.transport.cancel_pending_connection(attempt).await? {
            self.transport
                .record_peer_disconnected(attempt.peer())
                .await;
        }
        Ok(true)
    }

    async fn pending_connection_allows_message(
        &self,
        peer: Option<Did>,
    ) -> Result<bool, CallbackError> {
        let Some(attempt) = self.pending_attempt else {
            return Ok(true);
        };
        let Some(peer) = peer else {
            tracing::warn!(
                "ignoring message from unparsable peer; pending attempt belongs to {}",
                attempt.peer()
            );
            return Ok(false);
        };
        if attempt.peer() != peer {
            tracing::warn!(
                "ignoring message from {peer}; pending attempt belongs to {}",
                attempt.peer()
            );
            self.transport.cancel_pending_connection(attempt).await?;
            return Ok(false);
        }
        if !self.transport.is_admitted_connection_attempt(attempt) {
            tracing::debug!("ignoring message from {peer}; pending connection is not admitted yet");
            return Ok(false);
        }
        Ok(true)
    }

    async fn handle_pending_terminal_event(
        &self,
        did: Did,
        operation: &str,
    ) -> Result<bool, CallbackError> {
        let Some(attempt) = self.pending_attempt else {
            return Ok(false);
        };
        if self.transport.cancel_pending_connection(attempt).await? {
            self.transport.record_peer_disconnected(did).await;
            return Ok(true);
        }
        if self.transport.is_admitted_connection_attempt(attempt) {
            return Ok(false);
        }
        tracing::debug!(
            "ignoring late {operation} for {did}; pending attempt belongs to generation already superseded"
        );
        Ok(true)
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
            Message::PeerLivenessProbe(ref msg) => self.message_handler.handle(payload, msg).await,
            Message::PeerLivenessReport(ref msg) => self.message_handler.handle(payload, msg).await,
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
            Message::E2eHandshakeRequest(ref msg) => {
                self.message_handler.handle(payload, msg).await
            }
            Message::E2eHandshakeResponse(ref msg) => {
                self.message_handler.handle(payload, msg).await
            }
            Message::E2eStreamFrame(ref msg) => self.message_handler.handle(payload, msg).await,
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

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl TransportCallback for InnerSwarmCallback {
    async fn on_message(&self, cid: &str, msg: &[u8]) -> Result<(), CallbackError> {
        let peer = Did::from_str(cid).ok();
        if !self.pending_connection_allows_message(peer).await? {
            return Ok(());
        }
        let payload = match MessagePayload::from_bincode(msg) {
            Ok(payload) => payload,
            Err(e) => {
                if let Some(peer) = peer {
                    self.transport
                        .record_peer_message_receive_failed(peer)
                        .await;
                }
                return Err(e.into());
            }
        };
        if !(payload.verify() && payload.transaction.verify()) {
            tracing::error!("Cannot verify msg or it's expired: {:?}", payload);
            if let Some(peer) = peer {
                self.transport
                    .record_peer_message_receive_failed(peer)
                    .await;
            }
            return Err("Cannot verify msg or it's expired".into());
        }
        if let Some(peer) = peer {
            self.transport.record_peer_message_received(peer).await;
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
        if self
            .cancel_mismatched_pending_connection(did, "connection state change")
            .await?
        {
            return Ok(());
        }
        if self.is_local_did_event(did, "connection state change") {
            return Ok(());
        }

        match s {
            // ICE `Connected` is not enough for routing: admission happens only
            // after the data channel opens, because that is the first point at
            // which payload transport is actually usable.
            WebrtcConnectionState::Connected => {}
            // `Failed` and `Closed` are terminal states. Pending handshakes are
            // discarded without touching the DHT; active peers leave it.
            WebrtcConnectionState::Failed | WebrtcConnectionState::Closed => {
                if self
                    .handle_pending_terminal_event(did, "connection terminal state")
                    .await?
                {
                    return Ok(());
                }
                if !self.transport.is_admitted_connection(did) {
                    return Ok(());
                }
                self.transport.record_peer_disconnected(did).await;
                self.message_handler.leave_dht(did).await?;
            }
            // `Disconnected` is a transient ICE state that frequently recovers
            // back to `Connected` on its own (e.g. a brief network blip or ICE
            // consent refresh). Tearing the connection down here would kill a
            // link that WebRTC could have healed, and drop the peer from the DHT
            // with no reconnect path. We leave it alone: it will either recover,
            // or degrade to `Failed`, which is handled above.
            WebrtcConnectionState::Disconnected => {
                if self.pending_disconnected_before_admission(did) {
                    tracing::debug!(
                        "ignoring pre-admission disconnected state for pending connection {did}"
                    );
                    return Ok(());
                }
                self.transport.record_peer_disconnected(did).await;
                tracing::info!("Connection to {did} is disconnected, waiting for recovery");
            }
            _ => {}
        };

        // Data-channel admission emits the application-level Connected event.
        // Other state changes are passed through directly.
        if s != WebrtcConnectionState::Connected {
            self.emit_connection_state_change(did, s).await?
        }

        Ok(())
    }

    async fn on_data_channel_open(&self, cid: &str) -> Result<(), CallbackError> {
        let Ok(did) = Did::from_str(cid) else {
            tracing::warn!("on_data_channel_open parse did failed: {}", cid);
            return Ok(());
        };
        if self
            .cancel_mismatched_pending_connection(did, "data-channel open")
            .await?
        {
            return Ok(());
        }
        if self.is_local_did_event(did, "data-channel open") {
            return Ok(());
        }

        if !self.admit_pending_connection(did).await? && !self.transport.is_admitted_connection(did)
        {
            tracing::debug!("ignoring late data-channel open for {did}");
        }
        Ok(())
    }

    async fn on_data_channel_close(&self, cid: &str) -> Result<(), CallbackError> {
        let Ok(did) = Did::from_str(cid) else {
            tracing::warn!("on_data_channel_close parse did failed: {}", cid);
            return Ok(());
        };
        if self
            .cancel_mismatched_pending_connection(did, "data-channel close")
            .await?
        {
            return Ok(());
        }
        if self.is_local_did_event(did, "data-channel close") {
            return Ok(());
        }

        // The data channel closing is a reliable signal that the peer is gone
        // (e.g. it closed the connection), so tear the connection down now
        // instead of waiting for the ICE state to reach `Failed`. This is the
        // graceful counterpart to a local `disconnect()`: the remote learns of
        // it promptly without relying on the transient `Disconnected` state.
        if self
            .handle_pending_terminal_event(did, "data-channel close")
            .await?
        {
            return Ok(());
        }
        if !self.transport.is_admitted_connection(did) {
            return Ok(());
        }
        self.transport.record_peer_disconnected(did).await;
        self.message_handler.leave_dht(did).await?;
        Ok(())
    }
}
