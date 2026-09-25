use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use rings_transport::core::drop_guard::ArmedDropGuard;

use super::delivery::terminate_accepted_connection;
use super::delivery::ChunkSendPermit;
use super::delivery::SendCompletionOutcome;
use super::outbound::ChunkFrames;
use super::outbound::ChunkedFrameSource;
use super::outbound::DetachedAdmission;
use super::outbound::DetachedAdmissionCancel;
use super::outbound::OutboundCompletion;
use super::outbound::OutboundMessageKind;
use super::outbound::OutboundPeerHandle;
use super::outbound::OutboundTransfer;
use super::outbound::OutboundTransferRoute;
use super::outbound::TransferCapacityPermit;
use super::outbound::TransferDemand;
use super::timeouts::OUTBOUND_PAYLOAD_CLEANUP_GRACE;
use super::timeouts::TRACKED_PAYLOAD_COMPLETION_BOUND;
use super::AdmittedConnection;
use super::SwarmTransport;
use super::DATA_CHANNEL_SEND_ACCEPT_BUDGET;
use super::TRANSPORT_TIMEOUT_PROFILE;
use crate::chunk::Chunk;
use crate::chunk::Framing;
use crate::chunk::WireReserves;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::delegation::DelegateeKey;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::lifecycle::StopSource;
use crate::lifecycle::StopToken;
use crate::message::Message;
use crate::message::MessageCategory;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::PayloadSender;
use crate::swarm::observer::LookupCorrelation;
use crate::swarm::observer::LookupKind;
use crate::swarm::observer::LookupOutcome;
use crate::swarm::observer::MessageActivity;
use crate::swarm::observer::MessageObservation;
use crate::swarm::observer::ObservationOutcome;
use crate::utils::sleep;

const TRACKED_PAYLOAD_TIMEOUT: Duration = TRANSPORT_TIMEOUT_PROFILE.tracked_payload;
const DETACHED_FIRST_FRAME_TIMEOUT: Duration = TRANSPORT_TIMEOUT_PROFILE.first_frame_admission;

struct OversizedPayloadLog {
    local: Did,
    next_hop: Did,
    destination: Did,
    relay_destination: Did,
    tx_id: uuid::Uuid,
    message_kind: &'static str,
    bytes: usize,
    max_bytes: usize,
}

struct OutboundSendLog {
    /// Account DID that originated the logical message.
    origin: Did,
    next_hop: Did,
    destination: Did,
    relay_destination: Did,
    tx_id: uuid::Uuid,
    message_kind: &'static str,
    /// Finite scheduling class used for bounded aggregation.
    category: MessageCategory,
    /// Whether this locally originated send begins a successor lookup round.
    starts_successor_lookup: bool,
    completion: OutboundCompletion,
}

struct OutboundPreparation {
    message_kind: OutboundMessageKind,
    wire_bytes: usize,
    useful_bytes: u64,
    records_missing_connection_failure: bool,
    tx_id: uuid::Uuid,
    /// Account DID that originated the logical message.
    origin: Did,
    destination: Did,
    relay_destination: Did,
    next_hop: Did,
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

struct OutboundTransferProperties {
    logical_sequence: u64,
    useful_bytes: u64,
    completion: OutboundCompletion,
    stop: StopToken,
    detached_admission: Option<DetachedAdmission>,
}

struct StopOnDrop {
    cleanup: ArmedDropGuard<StopCleanup, fn(StopCleanup)>,
}

struct StopCleanup {
    source: StopSource,
    handle: Option<OutboundPeerHandle>,
}

impl StopCleanup {
    fn request_stop(&self) {
        self.source.request_stop();
        if let Some(handle) = &self.handle {
            handle.cancel_stopped();
        }
    }
}

fn stop_outbound_transfer(cleanup: StopCleanup) {
    cleanup.request_stop();
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
    fn new() -> (Self, StopToken) {
        let source = StopSource::new();
        let token = source.token();
        (
            Self {
                cleanup: ArmedDropGuard::new(
                    StopCleanup {
                        source,
                        handle: None,
                    },
                    stop_outbound_transfer,
                ),
            },
            token,
        )
    }

    fn bind_handle(&mut self, handle: OutboundPeerHandle) {
        if let Some(cleanup) = self.cleanup.value_mut() {
            cleanup.handle = Some(handle);
        }
    }

    fn request_stop(&self) {
        if let Some(cleanup) = self.cleanup.value() {
            cleanup.request_stop();
        }
    }

    fn disarm(&mut self) {
        self.cleanup.disarm();
    }
}

struct DetachedAdmissionOnDrop {
    cleanup: ArmedDropGuard<DetachedAdmissionCleanup, fn(DetachedAdmissionCleanup)>,
}

struct DetachedAdmissionCleanup {
    admission: DetachedAdmission,
    handle: OutboundPeerHandle,
}

impl DetachedAdmissionCleanup {
    fn cancel(&self) -> DetachedAdmissionCancel {
        let decision = self.admission.cancel();
        if decision == DetachedAdmissionCancel::Cancelled {
            self.handle.cancel_stopped();
        }
        decision
    }
}

fn cancel_detached_admission(cleanup: DetachedAdmissionCleanup) {
    cleanup.cancel();
}

impl DetachedAdmissionOnDrop {
    fn new(admission: DetachedAdmission, handle: OutboundPeerHandle) -> Self {
        Self {
            cleanup: ArmedDropGuard::new(
                DetachedAdmissionCleanup { admission, handle },
                cancel_detached_admission,
            ),
        }
    }

    fn cancel(&self) -> DetachedAdmissionCancel {
        self.cleanup.value().map_or(
            DetachedAdmissionCancel::MustAwait,
            DetachedAdmissionCleanup::cancel,
        )
    }

    fn disarm(&mut self) {
        self.cleanup.disarm();
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
    crate::fair_admission::retained_wire_bytes(wire_bytes).max(1)
}

/// The capacity demand of a transfer of `kind` whose wire form is `wire_bytes` long.
fn transfer_demand(kind: OutboundMessageKind, wire_bytes: usize) -> TransferDemand {
    TransferDemand::new(kind.class(), outbound_memory_reservation(wire_bytes))
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
    /// The capacity demand of `payload`, as `prepare_outbound_transfer` reserves it.
    pub(super) fn payload_demand(payload: &MessagePayload) -> Result<TransferDemand> {
        Ok(transfer_demand(
            OutboundMessageKind::from_wire(&payload.transaction.data)?,
            payload.wire_size()?,
        ))
    }

    async fn reserve_outbound_capacity(
        &self,
        peer: Did,
        demand: TransferDemand,
        completion: OutboundCompletion,
    ) -> Result<TransferCapacityPermit> {
        let reserve = self.outbound_schedulers.reserve(peer, demand).fuse();
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
        let (mut stop, stop_token) = StopOnDrop::new();
        let timeout = sleep(tracked_timeout).fuse();
        pin_mut!(timeout);
        let prepared = {
            let prepare = self
                .prepare_outbound_transfer(
                    payload.relay.next_hop,
                    payload,
                    OutboundCompletion::Tracked,
                    stop_token,
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
                        self.record_peer_message_send_failed(
                            did,
                            crate::measure::Authentication::LocallyAddressed,
                        )
                        .await;
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
            return admission.settle_cancelled(did);
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
        // Every detached result reaches the caller here: a `Cancelled` from any path (worker
        // drop or panic, scheduler loss, a stopped submit) stands only while the admission can
        // still be cancelled, never after a claim won.
        match result {
            Ok(SendCompletionOutcome::Cancelled) => admission.settle_cancelled(did),
            result => result,
        }
    }

    async fn prepare_outbound_transfer(
        &self,
        did: Did,
        payload: MessagePayload,
        completion: OutboundCompletion,
        stop: StopToken,
        detached_admission: Option<DetachedAdmission>,
    ) -> Result<Option<PreparedOutboundTransfer>> {
        let preparation = self
            .inspect_outbound_preparation(did, &payload, completion)
            .await?;
        let message_kind = preparation.message_kind;
        let wire_bytes = preparation.wire_bytes;
        let useful_bytes = preparation.useful_bytes;
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        crate::simulation::record_outbound_submission(preparation.tx_id);
        let capacity_permit = self
            .reserve_outbound_capacity(did, transfer_demand(message_kind, wire_bytes), completion)
            .await?;
        let permit = if message_kind.requires_storage_route() {
            let message = payload.transaction.data::<Message>()?;
            ChunkSendPermit::for_message(self.dht.clone(), did, &message)
        } else {
            ChunkSendPermit::Always
        };
        let admitted = self
            .connection_for_send(
                did,
                completion,
                preparation.records_missing_connection_failure,
            )
            .await?;
        let Some(handle) =
            admitted.with_current_connection(|_| self.outbound_schedulers.handle(did))?
        else {
            return Ok(None);
        };
        let handle = handle?;
        let max_message_size = admitted.connection().max_message_size();
        let Some(plan) = WireReserves::PRODUCTION.plan(wire_bytes, max_message_size) else {
            self.record_peer_message_send_failed(
                did,
                crate::measure::Authentication::Authenticated,
            )
            .await;
            return Err(Error::PeerMaxMessageSizeTooSmall(max_message_size));
        };
        admitted.ensure_current()?;
        tracing::debug!(
            local = %self.dht.did,
            next_hop = %preparation.next_hop,
            destination = %preparation.destination,
            relay_destination = %preparation.relay_destination,
            tx_id = %preparation.tx_id,
            message_kind = message_kind.as_str(),
            bytes = wire_bytes,
            max_message_size,
            framing = ?plan,
            "send payload start"
        );
        let logical_sequence = payload.transaction.sequence;
        let framed = self.frame_outbound_transfer(
            OutboundTransferRoute::new(message_kind.class(), did, admitted.clone(), permit),
            payload,
            plan,
            OutboundTransferProperties {
                logical_sequence,
                useful_bytes,
                completion,
                stop,
                detached_admission,
            },
        )?;
        Ok(Some(PreparedOutboundTransfer {
            admitted,
            handle,
            transfer: framed.transfer,
            capacity_permit,
            receiver: framed.receiver,
            log: OutboundSendLog {
                origin: preparation.origin,
                next_hop: preparation.next_hop,
                destination: preparation.destination,
                relay_destination: preparation.relay_destination,
                tx_id: preparation.tx_id,
                message_kind: message_kind.as_str(),
                category: message_kind.class(),
                starts_successor_lookup: matches!(
                    message_kind,
                    OutboundMessageKind::FindSuccessorSend
                ) && preparation.origin == self.dht.did,
                completion,
            },
        }))
    }

    async fn inspect_outbound_preparation(
        &self,
        did: Did,
        payload: &MessagePayload,
        completion: OutboundCompletion,
    ) -> Result<OutboundPreparation> {
        let message_kind = OutboundMessageKind::from_wire(&payload.transaction.data)?;
        let wire_bytes = payload.wire_size()?;
        let preparation = OutboundPreparation {
            message_kind,
            wire_bytes,
            useful_bytes: u64::try_from(payload.transaction.data.len())
                .map_err(|_| Error::MessageSizeOverflow)?,
            records_missing_connection_failure: completion == OutboundCompletion::Detached
                && message_kind.records_missing_connection_failure(),
            tx_id: payload.transaction.tx_id,
            origin: payload.transaction.origin(),
            destination: payload.transaction.destination,
            relay_destination: payload.relay.destination,
            next_hop: payload.relay.next_hop,
        };
        if wire_bytes > TRANSPORT_MAX_SIZE {
            log_oversized_payload(OversizedPayloadLog {
                local: self.dht.did,
                next_hop: preparation.next_hop,
                destination: preparation.destination,
                relay_destination: preparation.relay_destination,
                tx_id: preparation.tx_id,
                message_kind: message_kind.as_str(),
                bytes: wire_bytes,
                max_bytes: TRANSPORT_MAX_SIZE,
            });
            return Err(Error::MessageTooLarge(wire_bytes));
        }
        if self.admitted_send_connection(did)?.is_none() {
            if preparation.records_missing_connection_failure {
                self.record_peer_message_send_failed(
                    did,
                    crate::measure::Authentication::LocallyAddressed,
                )
                .await;
            }
            return Err(Error::SwarmMissDidInTable(did));
        }
        Ok(preparation)
    }

    /// Build the transfer that carries `payload` under `framing`.
    ///
    /// A whole payload travels as one link frame, its delegation slots encoded by the worker. A
    /// chunked payload is cut from its self-contained encoding, because the receiver decodes it
    /// after reassembly, outside the order of the link; the chunk frames that carry it are link
    /// frames like any other.
    fn frame_outbound_transfer(
        &self,
        route: OutboundTransferRoute,
        payload: MessagePayload,
        framing: Framing,
        properties: OutboundTransferProperties,
    ) -> Result<FramedOutboundTransfer> {
        let OutboundTransferProperties {
            logical_sequence,
            useful_bytes,
            completion,
            stop,
            detached_admission,
        } = properties;
        let (transfer, receiver) = match framing {
            Framing::Whole => OutboundTransfer::whole(
                route,
                payload,
                useful_bytes,
                completion,
                stop,
                detached_admission,
            ),
            Framing::Chunked { chunk_size } => {
                let chunks: ChunkFrames = Box::new(Chunk::stream(payload.to_wire()?, chunk_size));
                OutboundTransfer::chunked(
                    route,
                    ChunkedFrameSource::new(self.message_signer(), chunks, logical_sequence),
                    useful_bytes,
                    completion,
                    stop,
                    detached_admission,
                )
            }
        };
        Ok(FramedOutboundTransfer { transfer, receiver })
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
        let successor_lookup = log
            .starts_successor_lookup
            .then_some(LookupCorrelation::Transaction(log.tx_id));
        if let Some(correlation) = successor_lookup {
            self.observer()
                .lookup_started(LookupKind::Successor, correlation);
        }
        let result = self
            .submit_outbound_transfer(&admitted, handle, transfer, capacity_permit, receiver)
            .await;

        let observation_outcome = match &result {
            Ok(SendCompletionOutcome::Succeeded) => ObservationOutcome::Succeeded,
            Ok(SendCompletionOutcome::Cancelled) | Err(_) => ObservationOutcome::Failed,
        };
        let activity = if log.origin == self.dht.did {
            MessageActivity::Sent
        } else {
            MessageActivity::Forwarded
        };
        self.observe_message(MessageObservation {
            activity,
            category: log.category,
            message_class: log.message_kind,
            outcome: observation_outcome,
        });
        if let Some(correlation) = successor_lookup {
            if matches!(observation_outcome, ObservationOutcome::Failed) {
                self.observer().lookup_finished(
                    LookupKind::Successor,
                    correlation,
                    LookupOutcome::Failed,
                );
            }
        }

        let outcome = result?;

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
    fn message_signer(&self) -> MessageSigner<&DelegateeKey> {
        MessageSigner::new(&self.delegatee_key, self.network_id)
    }

    fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
    }

    fn is_connected(&self, did: Did) -> bool {
        self.get_connection(did).is_some()
    }

    async fn reserve_transaction_sequences(
        &self,
        destination: Did,
        count: std::num::NonZeroU64,
    ) -> Result<std::ops::RangeInclusive<u64>> {
        SwarmTransport::reserve_transaction_sequences(self, destination, count).await
    }

    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()> {
        self.do_send_payload_detached(did, payload)
            .await
            .map(|_| ())
    }
}
