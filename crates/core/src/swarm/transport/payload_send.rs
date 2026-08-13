use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use rings_transport::delivery::DeliveryFuture;

use super::delivery::await_tracked_delivery;
use super::delivery::frame_chunk;
use super::delivery::run_chunked_send;
use super::delivery::send_data_with_timeout;
use super::delivery::spawn_chunked_send;
use super::delivery::spawn_delivery;
use super::delivery::ChunkSendPermit;
use super::delivery::ChunkSendProgress;
use super::delivery::SendCompletionOutcome;
use super::AdmittedConnection;
use super::SwarmTransport;
use crate::chunk::ChunkList;
use crate::chunk::Framing;
use crate::chunk::WireReserves;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::session::SessionSk;
use crate::utils::sleep;

#[cfg(test)]
const TRACKED_PAYLOAD_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const TRACKED_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone, Copy)]
enum SendCompletion {
    Detached,
    Tracked,
}

fn accepted_initial_delivery(
    progress: ChunkSendProgress<Result<DeliveryFuture>>,
) -> Result<Option<DeliveryFuture>> {
    match progress {
        ChunkSendProgress::Ready(result) => result.map(Some),
        ChunkSendProgress::Cancelled(reason) => {
            reason.resolve_initial()?;
            Ok(None)
        }
    }
}

fn message_kind(message: &Message) -> &'static str {
    match message {
        Message::ConnectNodeSend(_) => "ConnectNodeSend",
        Message::ConnectNodeReport(_) => "ConnectNodeReport",
        Message::FindSuccessorSend(_) => "FindSuccessorSend",
        Message::FindSuccessorReport(_) => "FindSuccessorReport",
        Message::NotifyPredecessorSend(_) => "NotifyPredecessorSend",
        Message::NotifyPredecessorReport(_) => "NotifyPredecessorReport",
        Message::PeerLivenessProbe(_) => "PeerLivenessProbe",
        Message::PeerLivenessReport(_) => "PeerLivenessReport",
        Message::SearchEntry(_) => "SearchEntry",
        Message::FoundEntry(_) => "FoundEntry",
        Message::OperateEntry(_) => "OperateEntry",
        Message::SyncEntriesWithSuccessor(_) => "SyncEntriesWithSuccessor",
        Message::SyncEntriesWithSuccessorReport(_) => "SyncEntriesWithSuccessorReport",
        Message::CustomMessage(_) => "CustomMessage",
        Message::E2eHandshakeRequest(_) => "E2eHandshakeRequest",
        Message::E2eHandshakeResponse(_) => "E2eHandshakeResponse",
        Message::E2eStreamFrame(_) => "E2eStreamFrame",
        Message::QueryForTopoInfoSend(_) => "QueryForTopoInfoSend",
        Message::QueryForTopoInfoReport(_) => "QueryForTopoInfoReport",
        Message::Chunk(_) => "Chunk",
    }
}

impl SwarmTransport {
    /// Send a maintenance payload and return only after all of its frames stop.
    ///
    /// This is a network-only cancellation boundary. Dropping the send future
    /// cannot cancel storage work, and bounds a delivery future that never
    /// observes buffered-amount recovery.
    pub(crate) async fn send_payload_tracked(
        &self,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        let did = payload.relay.next_hop;
        let send = self
            .do_send_payload_with_completion(
                payload.relay.next_hop,
                payload,
                SendCompletion::Tracked,
            )
            .fuse();
        let timeout = sleep(TRACKED_PAYLOAD_TIMEOUT).fuse();
        pin_mut!(send, timeout);

        select! {
            result = send => result,
            _ = timeout => {
                tracing::warn!(
                    target: "rings_core::transport::tracked_send",
                    local = %self.dht.did,
                    peer = %did,
                    timeout_ms = TRACKED_PAYLOAD_TIMEOUT.as_millis(),
                    "tracked payload delivery timed out and was deferred"
                );
                Ok(SendCompletionOutcome::Cancelled)
            }
        }
    }

    async fn connection_for_send(
        &self,
        did: Did,
        completion: SendCompletion,
        records_missing_connection_failure: bool,
    ) -> Result<AdmittedConnection> {
        match completion {
            SendCompletion::Detached => {
                let Some(connection) = self.get_and_check_send_connection(did).await else {
                    if records_missing_connection_failure {
                        self.record_peer_message_send_failed(did).await;
                    }
                    return Err(Error::SwarmMissDidInTable(did));
                };
                Ok(connection)
            }
            SendCompletion::Tracked => {
                let Some(connection) = self.admitted_send_connection(did)? else {
                    return Err(Error::SwarmMissDidInTable(did));
                };
                connection
                    .connection()
                    .readiness()
                    .ensure_can_make_progress()?;
                Ok(connection)
            }
        }
    }

    async fn send_whole(
        &self,
        admitted: AdmittedConnection,
        data: bytes::Bytes,
        did: Did,
        permit: ChunkSendPermit,
        completion: SendCompletion,
    ) -> Result<SendCompletionOutcome> {
        let Some(delivery) = accepted_initial_delivery(
            self.send_frame(&admitted, data, &permit, did, "whole_message")
                .await,
        )?
        else {
            return Ok(SendCompletionOutcome::Cancelled);
        };
        match completion {
            SendCompletion::Detached => {
                spawn_delivery(delivery, admitted, permit, did, self.measure.clone());
                Ok(SendCompletionOutcome::Succeeded)
            }
            SendCompletion::Tracked => {
                await_tracked_delivery(delivery, &admitted, &permit, did, self.measure.clone())
                    .await
            }
        }
    }

    async fn send_frame(
        &self,
        admitted: &AdmittedConnection,
        data: bytes::Bytes,
        permit: &ChunkSendPermit,
        did: Did,
        context: &'static str,
    ) -> ChunkSendProgress<Result<DeliveryFuture>> {
        match send_data_with_timeout(admitted, data, permit, did, context).await {
            ChunkSendProgress::Ready(Err(error)) => {
                if error.records_peer_send_failure() {
                    self.record_peer_message_send_failed(did).await;
                }
                ChunkSendProgress::Ready(Err(error))
            }
            ChunkSendProgress::Cancelled(reason) => {
                if reason.records_peer_failure() {
                    self.record_peer_message_send_failed(did).await;
                }
                ChunkSendProgress::Cancelled(reason)
            }
            ready => ready,
        }
    }

    async fn send_chunked(
        &self,
        admitted: AdmittedConnection,
        data: bytes::Bytes,
        did: Did,
        chunk_size: usize,
        permit: ChunkSendPermit,
        completion: SendCompletion,
    ) -> Result<SendCompletionOutcome> {
        let mut chunks = ChunkList::stream(data, chunk_size);
        let Some(first) = chunks.next() else {
            return Ok(SendCompletionOutcome::Succeeded);
        };
        let first = frame_chunk(&self.session_sk, did, first)?;
        let Some(first_delivery) = accepted_initial_delivery(
            self.send_frame(&admitted, first, &permit, did, "chunked_first")
                .await,
        )?
        else {
            return Ok(SendCompletionOutcome::Cancelled);
        };
        match completion {
            SendCompletion::Detached => {
                spawn_chunked_send(
                    admitted,
                    Box::new(chunks),
                    first_delivery,
                    self.session_sk.clone(),
                    did,
                    permit,
                    self.measure.clone(),
                );
                Ok(SendCompletionOutcome::Succeeded)
            }
            SendCompletion::Tracked => {
                run_chunked_send(
                    admitted,
                    Box::new(chunks),
                    first_delivery,
                    self.session_sk.clone(),
                    did,
                    permit,
                    self.measure.clone(),
                )
                .await
            }
        }
    }

    async fn do_send_payload_with_completion(
        &self,
        did: Did,
        payload: MessagePayload,
        completion: SendCompletion,
    ) -> Result<SendCompletionOutcome> {
        let (permit, message_kind) = {
            let message = payload.transaction.data::<Message>()?;
            (
                ChunkSendPermit::for_message(self.dht.clone(), did, &message),
                message_kind(&message),
            )
        };
        let admitted = self
            .connection_for_send(did, completion, permit.records_missing_connection_failure())
            .await?;
        let tx_id = payload.transaction.tx_id;
        let destination = payload.transaction.destination;
        let relay_destination = payload.relay.destination;
        let next_hop = payload.relay.next_hop;
        let data = payload.to_bincode()?;
        if data.len() > TRANSPORT_MAX_SIZE {
            tracing::error!(
                local = %self.dht.did,
                next_hop = %next_hop,
                destination = %destination,
                relay_destination = %relay_destination,
                tx_id = %tx_id,
                message_kind,
                bytes = data.len(),
                max_bytes = TRANSPORT_MAX_SIZE,
                "message payload is too large"
            );
            return Err(Error::MessageTooLarge(data.len()));
        }

        let max_message_size = admitted.connection().max_message_size();
        let Some(plan) = WireReserves::PRODUCTION.plan(data.len(), max_message_size) else {
            self.record_peer_message_send_failed(did).await;
            return Err(Error::PeerMaxMessageSizeTooSmall(max_message_size));
        };
        tracing::debug!(
            local = %self.dht.did,
            next_hop = %next_hop,
            destination = %destination,
            relay_destination = %relay_destination,
            tx_id = %tx_id,
            message_kind,
            bytes = data.len(),
            max_message_size,
            framing = ?plan,
            "send payload start"
        );
        let outcome = match plan {
            Framing::Whole => {
                self.send_whole(admitted, data, did, permit, completion)
                    .await
            }
            Framing::Chunked { chunk_size } => {
                self.send_chunked(admitted, data, did, chunk_size, permit, completion)
                    .await
            }
        };
        let outcome = outcome?;

        tracing::debug!(
            local = %self.dht.did,
            next_hop = %next_hop,
            destination = %destination,
            relay_destination = %relay_destination,
            tx_id = %tx_id,
            message_kind,
            tracked = matches!(completion, SendCompletion::Tracked),
            succeeded = matches!(outcome, SendCompletionOutcome::Succeeded),
            "send payload accepted"
        );
        Ok(outcome)
    }

    pub(super) async fn send_payload_detached_with_outcome(
        &self,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        self.do_send_payload_with_completion(
            payload.relay.next_hop,
            payload,
            SendCompletion::Detached,
        )
        .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl PayloadSender for SwarmTransport {
    fn session_sk(&self) -> &SessionSk {
        &self.session_sk
    }

    fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
    }

    fn is_connected(&self, did: Did) -> bool {
        self.get_connection(did).is_some()
    }

    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()> {
        self.do_send_payload_with_completion(did, payload, SendCompletion::Detached)
            .await
            .map(|_| ())
    }
}
