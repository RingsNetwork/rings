//! Outbound transfer ownership and completion model.
//!
//! One transfer owns its payload source, stop authorities, exact connection
//! generation, and completion sender. Detached completion publishes after the
//! first accepted frame; tracked completion publishes only at terminal delivery.
//! Shutdown batches retain all sources until their capacity permits are dropped,
//! then publish collected results.

use futures::channel::oneshot;

use super::frame_chunk;
use super::AdmittedConnection;
use super::ChunkSendPermit;
use super::DetachedAdmission;
use super::FrameRemainder;
use super::OutboundCompletion;
use super::SendCompletionOutcome;
use super::TransferClass;
use super::TransferStop;
use crate::chunk::Chunk;
use crate::delegation::DelegateeKey;
use crate::dht::Did;
use crate::error::Result;
use crate::lifecycle::StopToken;
use crate::message::MessagePayload;
use crate::message::MessageSigner;

/// Oneshot sender delivering the terminal outcome of an outbound transfer.
type TransferResultSender = oneshot::Sender<Result<SendCompletionOutcome>>;

/// Boxed iterator yielding chunks for a chunked outbound transfer.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(in crate::swarm::transport) type ChunkFrames = Box<dyn Iterator<Item = Chunk> + Send>;
/// Boxed iterator yielding chunks for a chunked outbound transfer on wasm.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(in crate::swarm::transport) type ChunkFrames = Box<dyn Iterator<Item = Chunk>>;

/// Payload producer backing an [`OutboundTransfer`].
enum FrameSource {
    /// A single-frame payload consumed on the first emission.
    Whole(Option<Box<MessagePayload>>),
    /// A multi-chunk payload framed and signed one chunk at a time.
    Chunked(ChunkedFrameSource),
}

/// Chunked payload source holding the signer, chunk iterator, and sequence metadata.
pub(in crate::swarm::transport) struct ChunkedFrameSource {
    /// Signing authority used to sign each emitted chunk envelope.
    signer: MessageSigner<DelegateeKey>,
    /// Remaining chunks to wrap and send.
    chunks: ChunkFrames,
    /// Logical message sequence number shared across all chunks of this transfer.
    logical_sequence: u64,
    /// Whether the chunk marked last of its transfer (`position + 1 = total`) was emitted.
    emitted_last: bool,
}

impl ChunkedFrameSource {
    /// Create a chunked frame source from its signer, chunk iterator, and logical sequence.
    pub(in crate::swarm::transport) fn new(
        signer: MessageSigner<DelegateeKey>,
        chunks: ChunkFrames,
        logical_sequence: u64,
    ) -> Self {
        Self {
            signer,
            chunks,
            logical_sequence,
            emitted_last: false,
        }
    }
}

impl FrameSource {
    /// Whether frames remain after those already emitted. A chunked source answers from the
    /// chunk header, so the answer holds before the iterator is polled past its end.
    fn remainder(&self) -> FrameRemainder {
        let more = match self {
            Self::Whole(frame) => frame.is_some(),
            Self::Chunked(source) => !source.emitted_last,
        };
        if more {
            FrameRemainder::More
        } else {
            FrameRemainder::Final
        }
    }

    /// The next payload of this source; the worker encodes it for the link just before sending,
    /// so its delegation slots follow the order frames are accepted in.
    fn next_frame(&mut self, did: Did) -> Result<Option<(Box<MessagePayload>, &'static str)>> {
        match self {
            Self::Whole(frame) => Ok(frame.take().map(|payload| (payload, "whole_message"))),
            Self::Chunked(ChunkedFrameSource {
                signer,
                chunks,
                logical_sequence,
                emitted_last,
            }) => {
                let Some(chunk) = chunks.next() else {
                    return Ok(None);
                };
                let [position, total] = chunk.chunk;
                *emitted_last = position.saturating_add(1) >= total;
                let context = if position == 0 {
                    "chunked_first"
                } else {
                    "chunked_tail"
                };
                frame_chunk(signer.by_ref(), did, chunk, *logical_sequence)
                    .map(|payload| Some((Box::new(payload), context)))
            }
        }
    }
}

/// Owned state of a scheduled outbound transfer to a single peer connection.
pub(in crate::swarm::transport) struct OutboundTransfer {
    /// Scheduling class lane for this transfer.
    class: TransferClass,
    /// Destination peer DID.
    pub(super) did: Did,
    /// Specific admitted connection generation pinned to this transfer.
    pub(super) admitted: AdmittedConnection,
    /// Capacity permit held for the lifetime of this transfer.
    pub(super) permit: ChunkSendPermit,
    /// Source of outbound message frames for this transfer.
    source: FrameSource,
    /// Useful logical payload size in bytes for measurement attribution.
    useful_bytes: u64,
    /// Completion policy and result channel for the caller.
    completion: TransferCompletion,
    /// Whether the first frame of this transfer has been admitted by the backend.
    first_frame_admitted: bool,
    /// Combined caller and scheduler stop authority for this transfer.
    pub(super) stop: TransferStop,
    /// Optional detached first-frame admission coordinator.
    pub(super) detached_admission: Option<DetachedAdmission>,
}

/// Completion policy and optional result sender for an [`OutboundTransfer`].
struct TransferCompletion {
    /// Whether completion publishes after the first frame or after full delivery.
    policy: OutboundCompletion,
    /// Oneshot sender awaiting the transfer's outcome, taken once published.
    sender: Option<TransferResultSender>,
}

/// Deferred transfer result published only after its capacity permit is dropped.
pub(super) struct FinalTransferResult {
    /// Oneshot sender for the waiting caller.
    sender: TransferResultSender,
    /// Outcome to deliver to the caller.
    result: Result<SendCompletionOutcome>,
}

/// Owns every scheduler source until all capacity permits have been released.
pub(super) struct ShutdownBatch<T> {
    /// Transfers currently active in flight during shutdown.
    active: Vec<T>,
    /// Transfers ready to run in scheduler lanes during shutdown.
    ready: Vec<T>,
    /// Transfers still buffered in the worker mailbox during shutdown.
    buffered: Vec<T>,
}

impl<T> ShutdownBatch<T> {
    /// Bundle active, ready, and buffered transfers collected during shutdown.
    pub(super) fn new(active: Vec<T>, ready: Vec<T>, buffered: Vec<T>) -> Self {
        Self {
            active,
            ready,
            buffered,
        }
    }

    /// Finalize all transfers in the batch, dropping their permits and collecting results.
    pub(super) fn finalize<R>(self, finalize: impl FnMut(T) -> Option<R>) -> Vec<R> {
        self.active
            .into_iter()
            .chain(self.ready)
            .chain(self.buffered)
            .filter_map(finalize)
            .collect()
    }
}

impl FinalTransferResult {
    /// Publish only after the scheduled transfer has dropped its capacity permit.
    pub(super) fn publish(self) {
        let _ = self.sender.send(self.result);
    }
}

impl TransferCompletion {
    /// Take the completion result on first-frame admission if the policy is detached.
    fn take_first_admission(&mut self) -> Option<FinalTransferResult> {
        if self.policy == OutboundCompletion::Detached {
            return self.take_final(Ok(SendCompletionOutcome::Succeeded));
        }
        None
    }

    /// Take the completion sender, if still present, and pair it with `result`.
    fn take_final(&mut self, result: Result<SendCompletionOutcome>) -> Option<FinalTransferResult> {
        self.sender
            .take()
            .map(|sender| FinalTransferResult { sender, result })
    }
}

/// Routing and capacity context required to construct an [`OutboundTransfer`].
pub(in crate::swarm::transport) struct OutboundTransferRoute {
    /// Scheduling class lane for the transfer.
    class: TransferClass,
    /// Destination peer DID.
    did: Did,
    /// Admitted connection pinned to the transfer.
    admitted: AdmittedConnection,
    /// Capacity permit reserved for the transfer.
    permit: ChunkSendPermit,
}

impl OutboundTransferRoute {
    /// Create a route binding class, peer, admitted connection, and capacity permit.
    pub(in crate::swarm::transport) fn new(
        class: TransferClass,
        did: Did,
        admitted: AdmittedConnection,
        permit: ChunkSendPermit,
    ) -> Self {
        Self {
            class,
            did,
            admitted,
            permit,
        }
    }
}

impl OutboundTransfer {
    /// A transfer of one payload that fits one frame.
    pub(in crate::swarm::transport) fn whole(
        route: OutboundTransferRoute,
        payload: MessagePayload,
        useful_bytes: u64,
        completion: OutboundCompletion,
        stop: StopToken,
        detached_admission: Option<DetachedAdmission>,
    ) -> (Self, oneshot::Receiver<Result<SendCompletionOutcome>>) {
        Self::new(
            route,
            FrameSource::Whole(Some(Box::new(payload))),
            useful_bytes,
            completion,
            stop,
            detached_admission,
        )
    }

    /// A chunked transfer re-signs every chunk envelope, so it keeps its own copy of the
    /// signing authority for the whole transfer.
    pub(in crate::swarm::transport) fn chunked(
        route: OutboundTransferRoute,
        source: ChunkedFrameSource,
        useful_bytes: u64,
        completion: OutboundCompletion,
        stop: StopToken,
        detached_admission: Option<DetachedAdmission>,
    ) -> (Self, oneshot::Receiver<Result<SendCompletionOutcome>>) {
        Self::new(
            route,
            FrameSource::Chunked(source),
            useful_bytes,
            completion,
            stop,
            detached_admission,
        )
    }

    /// Construct an outbound transfer and its completion receiver from a route and frame source.
    fn new(
        route: OutboundTransferRoute,
        source: FrameSource,
        useful_bytes: u64,
        completion: OutboundCompletion,
        stop: StopToken,
        detached_admission: Option<DetachedAdmission>,
    ) -> (Self, oneshot::Receiver<Result<SendCompletionOutcome>>) {
        let (sender, receiver) = oneshot::channel();
        let completion = TransferCompletion {
            policy: completion,
            sender: Some(sender),
        };
        (
            Self {
                class: route.class,
                did: route.did,
                admitted: route.admitted,
                permit: route.permit,
                source,
                useful_bytes,
                completion,
                first_frame_admitted: false,
                stop: TransferStop::new(stop),
                detached_admission,
            },
            receiver,
        )
    }

    /// Return the scheduling class of this transfer.
    pub(super) fn class(&self) -> TransferClass {
        self.class
    }

    /// Useful bytes in the original logical payload, independent of framing.
    pub(super) const fn useful_bytes(&self) -> u64 {
        self.useful_bytes
    }

    /// Produce the next message payload frame and its logging context label.
    pub(super) fn next_frame(&mut self) -> Result<Option<(Box<MessagePayload>, &'static str)>> {
        self.source.next_frame(self.did)
    }

    /// Whether frames remain after those this transfer already emitted.
    pub(super) fn remainder(&self) -> FrameRemainder {
        self.source.remainder()
    }

    /// Return whether no frame of this transfer has been admitted yet.
    pub(super) fn is_before_first_frame(&self) -> bool {
        !self.first_frame_admitted
    }

    /// Return whether either caller or scheduler stop has been requested.
    pub(super) fn is_stopped(&self) -> bool {
        self.stop.should_stop()
    }

    /// Attach the peer worker's scheduler stop token to this transfer.
    pub(super) fn bind_scheduler_stop(&mut self, stop: StopToken) {
        self.stop.bind_scheduler(stop);
    }

    /// Record first-frame admission and return a detached completion result if applicable.
    pub(super) fn take_frame_admission_result(&mut self) -> Option<FinalTransferResult> {
        if !self.first_frame_admitted {
            if let Some(admission) = &self.detached_admission {
                if !admission.try_succeed() {
                    admission.enforce_cancelled_stop();
                    return None;
                }
            }
            self.first_frame_admitted = true;
            return self.completion.take_first_admission();
        }
        None
    }

    /// Take the final completion result for this transfer, if not already published.
    pub(super) fn take_final(
        &mut self,
        result: Result<SendCompletionOutcome>,
    ) -> Option<FinalTransferResult> {
        self.completion.take_final(result)
    }
}
