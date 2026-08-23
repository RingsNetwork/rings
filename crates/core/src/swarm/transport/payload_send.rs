use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;

use super::delivery::ChunkSendPermit;
use super::delivery::SendCompletionOutcome;
use super::outbound::ChunkFrames;
use super::outbound::OutboundCompletion;
use super::outbound::OutboundMessageMeta;
use super::outbound::OutboundPeerHandle;
use super::outbound::OutboundTransfer;
use super::outbound::OutboundTransferRoute;
use super::outbound::TransferCapacityPermit;
use super::AdmittedConnection;
use super::SwarmTransport;
use super::DATA_CHANNEL_SEND_ACCEPT_BUDGET;
use super::TRANSPORT_TIMEOUT_PROFILE;
use crate::chunk::ChunkList;
use crate::chunk::Framing;
use crate::chunk::WireReserves;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::lifecycle::StopSource;
use crate::lifecycle::StopToken;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::session::SessionSk;
use crate::utils::sleep;

const TRACKED_PAYLOAD_TIMEOUT: Duration = TRANSPORT_TIMEOUT_PROFILE.tracked_payload;

struct OversizedPayloadLog {
    local: Did,
    next_hop: Did,
    destination: Did,
    relay_destination: Did,
    tx_id: String,
    message_kind: &'static str,
    bytes: usize,
    max_bytes: usize,
}

struct OutboundSendLog {
    next_hop: Did,
    destination: Did,
    relay_destination: Did,
    tx_id: String,
    message_kind: &'static str,
    completion: OutboundCompletion,
}

struct PreparedOutboundTransfer {
    admitted: AdmittedConnection,
    handle: OutboundPeerHandle,
    transfer: OutboundTransfer,
    capacity_permit: TransferCapacityPermit,
    receiver: futures::channel::oneshot::Receiver<Result<SendCompletionOutcome>>,
    log: OutboundSendLog,
}

struct FramedOutboundTransfer {
    transfer: OutboundTransfer,
    receiver: futures::channel::oneshot::Receiver<Result<SendCompletionOutcome>>,
}

struct StopOnDrop(StopSource);

impl StopOnDrop {
    fn new() -> Self {
        Self(StopSource::new())
    }

    fn token(&self) -> StopToken {
        self.0.token()
    }

    fn request_stop(&self) {
        self.0.request_stop();
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.request_stop();
    }
}

fn log_oversized_payload(metadata: OversizedPayloadLog) {
    tracing::error!(
        local = %metadata.local,
        next_hop = %metadata.next_hop,
        destination = %metadata.destination,
        relay_destination = %metadata.relay_destination,
        tx_id = %metadata.tx_id,
        message_kind = metadata.message_kind,
        bytes = metadata.bytes,
        max_bytes = metadata.max_bytes,
        "message payload is too large"
    );
}

fn outbound_memory_reservation(wire_bytes: usize) -> usize {
    // Preparation owns the payload bytes plus the serialized transport frame.
    // TransportMessage shares the payload's Bytes allocation, so weighting by
    // two covers the whole-message peak without another wire-sized body copy.
    wire_bytes.saturating_mul(2).max(1)
}

impl SwarmTransport {
    async fn reserve_outbound_capacity(
        &self,
        peer: Did,
        metadata: OutboundMessageMeta,
        bytes: usize,
        completion: OutboundCompletion,
    ) -> Result<TransferCapacityPermit> {
        let reserve = self
            .outbound_schedulers
            .reserve(peer, metadata.class(), bytes)
            .fuse();
        if completion == OutboundCompletion::Tracked {
            return reserve.await;
        }
        let timeout = sleep(DATA_CHANNEL_SEND_ACCEPT_BUDGET).fuse();
        pin_mut!(reserve, timeout);
        select! {
            result = reserve => result,
            _ = timeout => Err(Error::OutboundTransferAdmissionTimeout {
                peer,
                timeout_ms: DATA_CHANNEL_SEND_ACCEPT_BUDGET.as_millis(),
            }),
        }
    }

    /// Send a maintenance payload and return only after all of its frames stop.
    ///
    /// This is a network-only cancellation boundary. Dropping the send future
    /// requests transfer stop without cancelling storage work, and the deadline
    /// bounds a delivery future that never observes buffered-amount recovery.
    pub(crate) async fn send_payload_tracked(
        &self,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        let did = payload.relay.next_hop;
        let stop = StopOnDrop::new();
        let timeout = sleep(TRACKED_PAYLOAD_TIMEOUT).fuse();
        pin_mut!(timeout);
        let prepared = {
            let prepare = self
                .prepare_outbound_transfer(
                    payload.relay.next_hop,
                    payload,
                    OutboundCompletion::Tracked,
                    stop.token(),
                )
                .fuse();
            pin_mut!(prepare);
            select! {
                result = prepare => result?,
                _ = timeout => {
                    self.log_tracked_payload_timeout(did);
                    return Ok(SendCompletionOutcome::Cancelled);
                }
            }
        };
        let Some(prepared) = prepared else {
            return Ok(SendCompletionOutcome::Cancelled);
        };
        let send = self.submit_prepared_outbound_transfer(prepared).fuse();
        pin_mut!(send);

        select! {
            result = send => result,
            _ = timeout => {
                stop.request_stop();
                let terminal_result = send.await;
                self.log_tracked_payload_timeout(did);
                terminal_result.map(|_| SendCompletionOutcome::Cancelled)
            }
        }
    }

    fn log_tracked_payload_timeout(&self, did: Did) {
        tracing::warn!(
            target: "rings_core::transport::tracked_send",
            local = %self.dht.did,
            peer = %did,
            timeout_ms = TRACKED_PAYLOAD_TIMEOUT.as_millis(),
            "tracked payload deadline elapsed and was deferred"
        );
    }

    async fn connection_for_send(
        &self,
        did: Did,
        completion: OutboundCompletion,
        records_missing_connection_failure: bool,
    ) -> Result<AdmittedConnection> {
        match completion {
            OutboundCompletion::Detached => {
                let Some(connection) = self.get_and_check_send_connection(did).await else {
                    if records_missing_connection_failure {
                        self.record_peer_message_send_failed(did).await;
                    }
                    return Err(Error::SwarmMissDidInTable(did));
                };
                Ok(connection)
            }
            OutboundCompletion::Tracked => {
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

    async fn submit_outbound_transfer(
        &self,
        admitted: &AdmittedConnection,
        handle: OutboundPeerHandle,
        transfer: OutboundTransfer,
        capacity_permit: TransferCapacityPermit,
        receiver: futures::channel::oneshot::Receiver<Result<SendCompletionOutcome>>,
    ) -> Result<SendCompletionOutcome> {
        if let Err(error) = handle.submit(transfer, capacity_permit) {
            return match admitted.ensure_current() {
                Ok(()) => Err(error),
                Err(Error::ConnectionAttemptSuperseded { .. }) => {
                    Ok(SendCompletionOutcome::Cancelled)
                }
                Err(current_error) => Err(current_error),
            };
        }
        match receiver.await {
            Ok(result) => result,
            Err(_) => match admitted.ensure_current() {
                Ok(()) => Err(Error::ChannelRecvMessageFailed(
                    "outbound scheduler stopped".into(),
                )),
                Err(Error::ConnectionAttemptSuperseded { .. }) => {
                    Ok(SendCompletionOutcome::Cancelled)
                }
                Err(error) => Err(error),
            },
        }
    }

    async fn do_send_payload_with_completion(
        &self,
        did: Did,
        payload: MessagePayload,
        completion: OutboundCompletion,
        stop: StopToken,
    ) -> Result<SendCompletionOutcome> {
        let Some(prepared) = self
            .prepare_outbound_transfer(did, payload, completion, stop)
            .await?
        else {
            return Ok(SendCompletionOutcome::Cancelled);
        };
        self.submit_prepared_outbound_transfer(prepared).await
    }

    async fn prepare_outbound_transfer(
        &self,
        did: Did,
        payload: MessagePayload,
        completion: OutboundCompletion,
        stop: StopToken,
    ) -> Result<Option<PreparedOutboundTransfer>> {
        let message_metadata = OutboundMessageMeta::from_wire(&payload.transaction.data)?;
        let wire_bytes = payload.wire_size()?;
        let message_kind = message_metadata.kind().as_str();
        let records_missing_connection_failure = completion == OutboundCompletion::Detached
            && message_metadata.records_missing_connection_failure();
        let tx_id = payload.transaction.tx_id;
        let destination = payload.transaction.destination;
        let relay_destination = payload.relay.destination;
        let next_hop = payload.relay.next_hop;
        if wire_bytes > TRANSPORT_MAX_SIZE {
            log_oversized_payload(OversizedPayloadLog {
                local: self.dht.did,
                next_hop,
                destination,
                relay_destination,
                tx_id: tx_id.to_string(),
                message_kind,
                bytes: wire_bytes,
                max_bytes: TRANSPORT_MAX_SIZE,
            });
            return Err(Error::MessageTooLarge(wire_bytes));
        }
        if self.admitted_send_connection(did)?.is_none() {
            if records_missing_connection_failure {
                self.record_peer_message_send_failed(did).await;
            }
            return Err(Error::SwarmMissDidInTable(did));
        }
        let capacity_permit = self
            .reserve_outbound_capacity(
                did,
                message_metadata,
                outbound_memory_reservation(wire_bytes),
                completion,
            )
            .await?;
        let permit = {
            let message = payload.transaction.data::<Message>()?;
            ChunkSendPermit::for_message(self.dht.clone(), did, &message)
        };
        let admitted = self
            .connection_for_send(did, completion, records_missing_connection_failure)
            .await?;
        let Some(handle) =
            admitted.with_current_connection(|_| self.outbound_schedulers.handle(did))?
        else {
            return Ok(None);
        };
        let handle = handle?;
        let max_message_size = admitted.connection().max_message_size();
        let Some(plan) = WireReserves::PRODUCTION.plan(wire_bytes, max_message_size) else {
            self.record_peer_message_send_failed(did).await;
            return Err(Error::PeerMaxMessageSizeTooSmall(max_message_size));
        };
        admitted.ensure_current()?;
        let data = payload.to_wire()?;
        tracing::debug!(
            local = %self.dht.did,
            next_hop = %next_hop,
            destination = %destination,
            relay_destination = %relay_destination,
            tx_id = %tx_id,
            message_kind,
            bytes = wire_bytes,
            max_message_size,
            framing = ?plan,
            "send payload start"
        );
        let framed = self.frame_outbound_transfer(
            OutboundTransferRoute::new(message_metadata.class(), did, admitted.clone(), permit),
            data,
            completion,
            stop,
            plan,
        );
        Ok(Some(PreparedOutboundTransfer {
            admitted,
            handle,
            transfer: framed.transfer,
            capacity_permit,
            receiver: framed.receiver,
            log: OutboundSendLog {
                next_hop,
                destination,
                relay_destination,
                tx_id: tx_id.to_string(),
                message_kind,
                completion,
            },
        }))
    }

    fn frame_outbound_transfer(
        &self,
        route: OutboundTransferRoute,
        data: bytes::Bytes,
        completion: OutboundCompletion,
        stop: StopToken,
        framing: Framing,
    ) -> FramedOutboundTransfer {
        let (transfer, receiver) = match framing {
            Framing::Whole => OutboundTransfer::whole(route, data, completion, stop),
            Framing::Chunked { chunk_size } => {
                let chunks: ChunkFrames = Box::new(ChunkList::stream(data, chunk_size));
                OutboundTransfer::chunked(route, self.session_sk.clone(), chunks, completion, stop)
            }
        };
        FramedOutboundTransfer { transfer, receiver }
    }

    async fn submit_prepared_outbound_transfer(
        &self,
        prepared: PreparedOutboundTransfer,
    ) -> Result<SendCompletionOutcome> {
        let PreparedOutboundTransfer {
            admitted,
            handle,
            transfer,
            capacity_permit,
            receiver,
            log,
        } = prepared;
        let outcome = self
            .submit_outbound_transfer(&admitted, handle, transfer, capacity_permit, receiver)
            .await?;

        tracing::debug!(
            local = %self.dht.did,
            next_hop = %log.next_hop,
            destination = %log.destination,
            relay_destination = %log.relay_destination,
            tx_id = %log.tx_id,
            message_kind = log.message_kind,
            tracked = matches!(log.completion, OutboundCompletion::Tracked),
            succeeded = matches!(outcome, SendCompletionOutcome::Succeeded),
            "send payload accepted"
        );
        Ok(outcome)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn send_payload_detached_observing_scheduler_submit_for_test(
        &self,
        payload: MessagePayload,
        observe_before_scheduler_submit: impl FnOnce(),
    ) -> Result<SendCompletionOutcome> {
        let prepared = self
            .prepare_outbound_transfer(
                payload.relay.next_hop,
                payload,
                OutboundCompletion::Detached,
                StopToken::never(),
            )
            .await?;
        let Some(prepared) = prepared else {
            return Ok(SendCompletionOutcome::Cancelled);
        };
        observe_before_scheduler_submit();
        self.submit_prepared_outbound_transfer(prepared).await
    }

    pub(super) async fn send_payload_detached_with_outcome(
        &self,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        self.do_send_payload_with_completion(
            payload.relay.next_hop,
            payload,
            OutboundCompletion::Detached,
            StopToken::never(),
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
        self.do_send_payload_with_completion(
            did,
            payload,
            OutboundCompletion::Detached,
            StopToken::never(),
        )
        .await
        .map(|_| ())
    }
}
