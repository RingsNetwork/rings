use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;

use super::delivery::terminate_accepted_connection;
use super::delivery::ChunkSendPermit;
use super::delivery::SendCompletionOutcome;
use super::outbound::ChunkFrames;
use super::outbound::DetachedAdmission;
use super::outbound::DetachedAdmissionCancel;
use super::outbound::OutboundCompletion;
use super::outbound::OutboundMessageMeta;
use super::outbound::OutboundPeerHandle;
use super::outbound::OutboundTransfer;
use super::outbound::OutboundTransferRoute;
use super::outbound::TransferCapacityPermit;
use super::timeouts::OUTBOUND_PAYLOAD_CLEANUP_GRACE;
use super::timeouts::TRACKED_PAYLOAD_COMPLETION_BOUND;
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
const DETACHED_FIRST_FRAME_TIMEOUT: Duration = TRANSPORT_TIMEOUT_PROFILE.first_frame_admission;

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

struct StopOnDrop {
    source: StopSource,
    handle: Option<OutboundPeerHandle>,
    authority: Option<()>,
}

async fn await_bounded_cleanup<F, T>(future: F, cleanup_grace: Duration) -> Option<T>
where F: Future<Output = T> {
    let future = future.fuse();
    let timeout = sleep(cleanup_grace).fuse();
    pin_mut!(future, timeout);
    select! {
        result = future => Some(result),
        _ = timeout => None,
    }
}

impl StopOnDrop {
    fn new() -> Self {
        Self {
            source: StopSource::new(),
            handle: None,
            authority: Some(()),
        }
    }

    fn token(&self) -> StopToken {
        self.source.token()
    }

    fn bind_handle(&mut self, handle: OutboundPeerHandle) {
        self.handle = Some(handle);
    }

    fn request_stop(&self) {
        self.source.request_stop();
        if let Some(handle) = &self.handle {
            handle.cancel_stopped();
        }
    }

    fn disarm(&mut self) {
        self.authority.take();
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if self.authority.take().is_some() {
            self.request_stop();
        }
    }
}

struct DetachedAdmissionOnDrop {
    admission: DetachedAdmission,
    handle: OutboundPeerHandle,
    authority: Option<()>,
}

impl DetachedAdmissionOnDrop {
    fn new(admission: DetachedAdmission, handle: OutboundPeerHandle) -> Self {
        Self {
            admission,
            handle,
            authority: Some(()),
        }
    }

    fn cancel(&self) -> DetachedAdmissionCancel {
        let decision = self.admission.cancel();
        if decision == DetachedAdmissionCancel::Cancelled {
            self.handle.cancel_stopped();
        }
        decision
    }

    fn disarm(&mut self) {
        self.authority.take();
    }
}

impl Drop for DetachedAdmissionOnDrop {
    fn drop(&mut self) {
        if self.authority.take().is_some() {
            self.cancel();
        }
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

fn validate_payload_size(metadata: OversizedPayloadLog) -> Result<()> {
    if metadata.bytes <= metadata.max_bytes {
        return Ok(());
    }
    let bytes = metadata.bytes;
    log_oversized_payload(metadata);
    Err(Error::MessageTooLarge(bytes))
}

fn outbound_memory_reservation(wire_bytes: usize) -> usize {
    // Preparation owns the payload bytes plus the serialized transport frame.
    // TransportMessage shares the payload's Bytes allocation, so weighting by
    // two covers the whole-message peak without another wire-sized body copy.
    crate::utils::retained_wire_bytes(wire_bytes).max(1)
}

fn resolve_scheduler_loss(
    admitted: &AdmittedConnection,
    local_error: Error,
) -> Result<SendCompletionOutcome> {
    match admitted.ensure_current() {
        Ok(()) => Err(local_error),
        Err(Error::ConnectionAttemptSuperseded { .. }) => Ok(SendCompletionOutcome::Cancelled),
        Err(error) => Err(error),
    }
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
        self.send_payload_tracked_with_timeouts(
            payload,
            TRACKED_PAYLOAD_TIMEOUT,
            OUTBOUND_PAYLOAD_CLEANUP_GRACE,
            TRACKED_PAYLOAD_COMPLETION_BOUND,
        )
        .await
    }

    async fn send_payload_tracked_with_timeouts(
        &self,
        payload: MessagePayload,
        tracked_timeout: Duration,
        cleanup_grace: Duration,
        completion_bound: Duration,
    ) -> Result<SendCompletionOutcome> {
        let did = payload.relay.next_hop;
        let mut stop = StopOnDrop::new();
        let timeout = sleep(tracked_timeout).fuse();
        pin_mut!(timeout);
        let prepared = {
            let prepare = self
                .prepare_outbound_transfer(
                    payload.relay.next_hop,
                    payload,
                    OutboundCompletion::Tracked,
                    stop.token(),
                    None,
                )
                .fuse();
            pin_mut!(prepare);
            select! {
                result = prepare => result?,
                _ = timeout => {
                    self.log_tracked_payload_timeout(
                        did,
                        tracked_timeout,
                        cleanup_grace,
                        completion_bound,
                    );
                    return Ok(SendCompletionOutcome::Cancelled);
                }
            }
        };
        let Some(prepared) = prepared else {
            return Ok(SendCompletionOutcome::Cancelled);
        };
        let cleanup_connection = prepared.admitted.clone();
        stop.bind_handle(prepared.handle.clone());
        let send = self.submit_prepared_outbound_transfer(prepared).fuse();
        pin_mut!(send);

        let result = select! {
            result = send => result,
            _ = timeout => {
                stop.request_stop();
                self.log_tracked_payload_timeout(
                    did,
                    tracked_timeout,
                    cleanup_grace,
                    completion_bound,
                );
                match await_bounded_cleanup(send, cleanup_grace).await {
                    Some(result) => result.map(|_| SendCompletionOutcome::Cancelled),
                    None => {
                        terminate_accepted_connection(
                            &cleanup_connection,
                            "tracked_payload_cleanup_timeout",
                        )
                        .await;
                        Err(Error::TrackedPayloadCleanupTimeout {
                            peer: did,
                            timeout_ms: cleanup_grace.as_millis(),
                        })
                    }
                }
            }
        };
        stop.disarm();
        result
    }

    fn log_tracked_payload_timeout(
        &self,
        did: Did,
        tracked_timeout: Duration,
        cleanup_grace: Duration,
        completion_bound: Duration,
    ) {
        tracing::warn!(
            target: "rings_core::transport::tracked_send",
            local = %self.dht.did,
            peer = %did,
            timeout_ms = tracked_timeout.as_millis(),
            cleanup_grace_ms = cleanup_grace.as_millis(),
            completion_bound_ms = completion_bound.as_millis(),
            "tracked payload admission deadline elapsed; transfer stop and bounded cleanup were requested"
        );
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn send_payload_tracked_with_matching_delivery_deadline_for_test(
        &self,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        let tracked_timeout = TRANSPORT_TIMEOUT_PROFILE.delivery;
        let completion_bound = tracked_timeout.saturating_add(OUTBOUND_PAYLOAD_CLEANUP_GRACE);
        self.send_payload_tracked_with_timeouts(
            payload,
            tracked_timeout,
            OUTBOUND_PAYLOAD_CLEANUP_GRACE,
            completion_bound,
        )
        .await
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn send_payload_tracked_with_shutdown_deadline_for_test(
        &self,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        let tracked_timeout = Duration::from_secs(5);
        self.send_payload_tracked_with_timeouts(
            payload,
            tracked_timeout,
            OUTBOUND_PAYLOAD_CLEANUP_GRACE,
            tracked_timeout.saturating_add(OUTBOUND_PAYLOAD_CLEANUP_GRACE),
        )
        .await
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
            return resolve_scheduler_loss(admitted, error);
        }
        match receiver.await {
            Ok(result) => result,
            Err(_) => resolve_scheduler_loss(
                admitted,
                Error::ChannelRecvMessageFailed("outbound scheduler stopped".into()),
            ),
        }
    }

    async fn do_send_payload_detached(
        &self,
        did: Did,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        self.do_send_payload_detached_until(
            did,
            payload,
            DETACHED_FIRST_FRAME_TIMEOUT,
            sleep(DETACHED_FIRST_FRAME_TIMEOUT),
        )
        .await
    }

    async fn do_send_payload_detached_until(
        &self,
        did: Did,
        payload: MessagePayload,
        timeout_budget: Duration,
        deadline: impl Future<Output = ()>,
    ) -> Result<SendCompletionOutcome> {
        let admission = DetachedAdmission::new();
        let timeout_error = || Error::OutboundFirstFrameAdmissionTimeout {
            peer: did,
            timeout_ms: timeout_budget.as_millis(),
        };
        let timeout = deadline.fuse();
        pin_mut!(timeout);
        let prepared = {
            let prepare = self
                .prepare_outbound_transfer(
                    did,
                    payload,
                    OutboundCompletion::Detached,
                    admission.stop_token(),
                    Some(admission.clone()),
                )
                .fuse();
            pin_mut!(prepare);
            select! {
                result = prepare => result?,
                _ = timeout => {
                    admission.cancel();
                    return Err(timeout_error());
                },
            }
        };
        let Some(prepared) = prepared else {
            return Ok(SendCompletionOutcome::Cancelled);
        };
        let mut cancel_on_drop =
            DetachedAdmissionOnDrop::new(admission.clone(), prepared.handle.clone());
        let cleanup_connection = prepared.admitted.clone();
        let send = self.submit_prepared_outbound_transfer(prepared);
        let send = send.fuse();
        pin_mut!(send);
        let result = select! {
            result = send => result,
            _ = timeout => {
                let cancellation = cancel_on_drop.cancel();
                match await_bounded_cleanup(send, OUTBOUND_PAYLOAD_CLEANUP_GRACE).await {
                    Some(result) if cancellation == DetachedAdmissionCancel::MustAwait => result,
                    Some(result) => {
                        match result? {
                            SendCompletionOutcome::Succeeded => {
                                Err(Error::CancelledDetachedAdmissionPublishedSuccess)
                            }
                            SendCompletionOutcome::Cancelled => Err(timeout_error()),
                        }
                    }
                    None => {
                        terminate_accepted_connection(
                            &cleanup_connection,
                            "detached_payload_cleanup_timeout",
                        )
                        .await;
                        Err(Error::DetachedPayloadCleanupTimeout {
                            peer: did,
                            timeout_ms: OUTBOUND_PAYLOAD_CLEANUP_GRACE.as_millis(),
                        })
                    }
                }
            },
        };
        cancel_on_drop.disarm();
        result
    }

    async fn prepare_outbound_transfer(
        &self,
        did: Did,
        payload: MessagePayload,
        completion: OutboundCompletion,
        stop: StopToken,
        detached_admission: Option<DetachedAdmission>,
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
        validate_payload_size(OversizedPayloadLog {
            local: self.dht.did,
            next_hop,
            destination,
            relay_destination,
            tx_id: tx_id.to_string(),
            message_kind,
            bytes: wire_bytes,
            max_bytes: TRANSPORT_MAX_SIZE,
        })?;
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
            detached_admission,
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
        detached_admission: Option<DetachedAdmission>,
        framing: Framing,
    ) -> FramedOutboundTransfer {
        let (transfer, receiver) = match framing {
            Framing::Whole => {
                OutboundTransfer::whole(route, data, completion, stop, detached_admission)
            }
            Framing::Chunked { chunk_size } => {
                let chunks: ChunkFrames = Box::new(ChunkList::stream(data, chunk_size));
                OutboundTransfer::chunked(
                    route,
                    self.session_sk.clone(),
                    chunks,
                    completion,
                    stop,
                    detached_admission,
                )
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
    pub(crate) async fn send_payload_detached_until_for_test(
        &self,
        payload: MessagePayload,
        timeout_budget: Duration,
        deadline: impl Future<Output = ()>,
    ) -> Result<SendCompletionOutcome> {
        self.do_send_payload_detached_until(
            payload.relay.next_hop,
            payload,
            timeout_budget,
            deadline,
        )
        .await
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn send_payload_detached_observing_scheduler_submit_for_test(
        &self,
        payload: MessagePayload,
        observe_before_scheduler_submit: impl FnOnce(),
    ) -> Result<SendCompletionOutcome> {
        let admission = DetachedAdmission::new();
        let prepared = self
            .prepare_outbound_transfer(
                payload.relay.next_hop,
                payload,
                OutboundCompletion::Detached,
                admission.stop_token(),
                Some(admission),
            )
            .await?;
        let Some(prepared) = prepared else {
            return Ok(SendCompletionOutcome::Cancelled);
        };
        observe_before_scheduler_submit();
        self.submit_prepared_outbound_transfer(prepared).await
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn send_payload_detached_cancel_before_submit_for_test(
        &self,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        let admission = DetachedAdmission::new();
        let prepared = self
            .prepare_outbound_transfer(
                payload.relay.next_hop,
                payload,
                OutboundCompletion::Detached,
                admission.stop_token(),
                Some(admission.clone()),
            )
            .await?;
        let Some(prepared) = prepared else {
            return Ok(SendCompletionOutcome::Cancelled);
        };
        let handle = prepared.handle.clone();
        admission.cancel();
        handle.cancel_stopped();
        self.submit_prepared_outbound_transfer(prepared).await
    }

    pub(super) async fn send_payload_detached_with_outcome(
        &self,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        self.do_send_payload_detached(payload.relay.next_hop, payload)
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
        self.do_send_payload_detached(did, payload)
            .await
            .map(|_| ())
    }
}
