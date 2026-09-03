//! Outbound transfer ownership and completion model.
//!
//! One transfer owns its payload source, stop authorities, exact connection
//! generation, and completion sender. Detached completion publishes after the
//! first accepted frame; tracked completion publishes only at terminal delivery.
//! Shutdown batches retain all sources until their capacity permits are dropped,
//! then publish collected results.

use bytes::Bytes;
use futures::channel::oneshot;

use super::frame_chunk;
use super::AdmittedConnection;
use super::ChunkSendPermit;
use super::DetachedAdmission;
use super::OutboundCompletion;
use super::SendCompletionOutcome;
use super::TransferClass;
use super::TransferStop;
use crate::chunk::Chunk;
use crate::dht::Did;
use crate::error::Result;
use crate::lifecycle::StopToken;
use crate::message::MessageSigner;
use crate::session::SessionSk;

type TransferResultSender = oneshot::Sender<Result<SendCompletionOutcome>>;

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(in crate::swarm::transport) type ChunkFrames = Box<dyn Iterator<Item = Chunk> + Send>;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(in crate::swarm::transport) type ChunkFrames = Box<dyn Iterator<Item = Chunk>>;

enum FrameSource {
    Whole(Option<Bytes>),
    Chunked {
        signer: MessageSigner<SessionSk>,
        chunks: ChunkFrames,
    },
}

impl FrameSource {
    fn next_frame(&mut self, did: Did) -> Result<Option<(Bytes, &'static str)>> {
        match self {
            Self::Whole(frame) => Ok(frame.take().map(|bytes| (bytes, "whole_message"))),
            Self::Chunked { signer, chunks } => {
                let Some(chunk) = chunks.next() else {
                    return Ok(None);
                };
                let [position, _total] = chunk.chunk;
                let context = if position == 0 {
                    "chunked_first"
                } else {
                    "chunked_tail"
                };
                frame_chunk(signer.by_ref(), did, chunk).map(|bytes| Some((bytes, context)))
            }
        }
    }
}

pub(in crate::swarm::transport) struct OutboundTransfer {
    class: TransferClass,
    pub(super) did: Did,
    pub(super) admitted: AdmittedConnection,
    pub(super) permit: ChunkSendPermit,
    source: FrameSource,
    useful_bytes: u64,
    completion: TransferCompletion,
    first_frame_admitted: bool,
    pub(super) stop: TransferStop,
    pub(super) detached_admission: Option<DetachedAdmission>,
}

struct TransferCompletion {
    policy: OutboundCompletion,
    sender: Option<TransferResultSender>,
}

pub(super) struct FinalTransferResult {
    sender: TransferResultSender,
    result: Result<SendCompletionOutcome>,
}

/// Owns every scheduler source until all capacity permits have been released.
pub(super) struct ShutdownBatch<T> {
    active: Vec<T>,
    ready: Vec<T>,
    buffered: Vec<T>,
}

impl<T> ShutdownBatch<T> {
    pub(super) fn new(active: Vec<T>, ready: Vec<T>, buffered: Vec<T>) -> Self {
        Self {
            active,
            ready,
            buffered,
        }
    }

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
    fn take_first_admission(&mut self) -> Option<FinalTransferResult> {
        if self.policy == OutboundCompletion::Detached {
            return self.take_final(Ok(SendCompletionOutcome::Succeeded));
        }
        None
    }

    fn take_final(&mut self, result: Result<SendCompletionOutcome>) -> Option<FinalTransferResult> {
        self.sender
            .take()
            .map(|sender| FinalTransferResult { sender, result })
    }
}

pub(in crate::swarm::transport) struct OutboundTransferRoute {
    class: TransferClass,
    did: Did,
    admitted: AdmittedConnection,
    permit: ChunkSendPermit,
}

impl OutboundTransferRoute {
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
    pub(in crate::swarm::transport) fn whole(
        route: OutboundTransferRoute,
        data: Bytes,
        useful_bytes: u64,
        completion: OutboundCompletion,
        stop: StopToken,
        detached_admission: Option<DetachedAdmission>,
    ) -> (Self, oneshot::Receiver<Result<SendCompletionOutcome>>) {
        Self::new(
            route,
            FrameSource::Whole(Some(data)),
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
        signer: MessageSigner<&SessionSk>,
        chunks: ChunkFrames,
        useful_bytes: u64,
        completion: OutboundCompletion,
        stop: StopToken,
        detached_admission: Option<DetachedAdmission>,
    ) -> (Self, oneshot::Receiver<Result<SendCompletionOutcome>>) {
        Self::new(
            route,
            FrameSource::Chunked {
                signer: signer.to_owned(),
                chunks,
            },
            useful_bytes,
            completion,
            stop,
            detached_admission,
        )
    }

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

    pub(super) fn class(&self) -> TransferClass {
        self.class
    }

    /// Useful bytes in the original logical payload, independent of framing.
    pub(super) const fn useful_bytes(&self) -> u64 {
        self.useful_bytes
    }

    pub(super) fn next_frame(&mut self) -> Result<Option<(Bytes, &'static str)>> {
        self.source.next_frame(self.did)
    }

    pub(super) fn is_before_first_frame(&self) -> bool {
        !self.first_frame_admitted
    }

    pub(super) fn is_stopped(&self) -> bool {
        self.stop.should_stop()
    }

    pub(super) fn bind_scheduler_stop(&mut self, stop: StopToken) {
        self.stop.bind_scheduler(stop);
    }

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

    pub(super) fn take_final(
        &mut self,
        result: Result<SendCompletionOutcome>,
    ) -> Option<FinalTransferResult> {
        self.completion.take_final(result)
    }
}
