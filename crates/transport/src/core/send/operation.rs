//! Queue-admission resources retained outside the send primitive's async stack.
//!
//! This future is always wrapped in `OwnedSend`. On error or panic its fields
//! remain owned until that wrapper fences the generation. In particular, an
//! async `?` must not release the channel lock ahead of retirement.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use super::model::end_offset;
use crate::core::transport::IrrevocableSendPermit;
use crate::core::transport::SendPermit;
use crate::error::Error;
use crate::error::Result;

/// Linear capabilities representing the queue adapter's actual admission state.
/// No pair of Options can express a contradictory permit/proof combination.
enum QueueAdmission {
    /// The first guarded poll has not attempted admission yet.
    Ready(SendPermit),
    /// Only this proof may commit queue acceptance.
    Sending(IrrevocableSendPermit),
    /// Admission was rejected, completed, or unwound; no second claim is possible.
    Finished,
}

impl QueueAdmission {
    /// Atomic admission is an adapter effect, governed by the shared pure permit model.
    fn claim(self) -> Self {
        match self {
            Self::Ready(permit) => permit
                .try_mark_irrevocable()
                .map(Self::Sending)
                .unwrap_or(Self::Finished),
            state => state,
        }
    }
}

/// Own the serial channel lease and commit acceptance only after successful enqueue.
struct QueueSend<F, L> {
    /// Primitive send, independently pinned without owning the channel lease.
    primitive: Pin<Box<F>>,
    /// One linear capability state; the primitive never owns its permit/proof.
    admission: QueueAdmission,
    /// Channel serialization lease; retained through the outer failure boundary.
    _channel: L,
    /// Successful queue-admission byte counter for this physical data channel.
    enqueued: Arc<AtomicU64>,
    /// Checked cumulative end offset published only after successful queue admission.
    end: u64,
}

impl<F: Future<Output = Result<()>>, L: Unpin> QueueSend<F, L> {
    /// Assemble an unpolled operation; the first poll must hold generation admission.
    fn new(
        primitive: F,
        permit: SendPermit,
        channel: L,
        enqueued: Arc<AtomicU64>,
        bytes: u64,
    ) -> Result<Self> {
        // The channel lease protects this snapshot; arithmetic is a pure function.
        end_offset(enqueued.load(Ordering::SeqCst), bytes)
            .map(|end| Self {
                primitive: Box::pin(primitive),
                admission: QueueAdmission::Ready(permit),
                _channel: channel,
                enqueued,
                end,
            })
            .ok_or(Error::SendByteCountOverflow)
    }

    /// Interpret one physical poll while retaining the channel lease in this struct.
    fn poll_claimed(
        &mut self,
        proof: IrrevocableSendPermit,
        context: &mut Context<'_>,
    ) -> Poll<Result<u64>> {
        match self.primitive.as_mut().poll(context) {
            Poll::Pending => {
                self.admission = QueueAdmission::Sending(proof);
                Poll::Pending
            }
            Poll::Ready(Err(error)) => {
                self.admission = QueueAdmission::Sending(proof);
                Poll::Ready(Err(error))
            }
            Poll::Ready(Ok(())) => {
                proof.mark_accepted();
                self.enqueued.store(self.end, Ordering::SeqCst);
                Poll::Ready(Ok(self.end))
            }
        }
    }
}

impl<F: Future<Output = Result<()>>, L: Unpin> Future for QueueSend<F, L> {
    type Output = Result<u64>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        // Move, never clone, the capability; Finished is the unwind-safe replacement.
        let admission = std::mem::replace(&mut self.admission, QueueAdmission::Finished).claim();
        match admission {
            QueueAdmission::Sending(proof) => self.poll_claimed(proof, context),
            QueueAdmission::Ready(_) | QueueAdmission::Finished => {
                Poll::Ready(Err(Error::SendPermitRevoked))
            }
        }
    }
}

/// Native preparation cannot be awaited: only start with a real admission lease exposes a future.
#[cfg(feature = "native-webrtc")]
pub(crate) struct PreparedQueue<
    F: Future<Output = Result<()>>,
    O: super::lifecycle::FailureObserver,
> {
    /// The raw queue never escapes its destruction boundary.
    owner: super::owner::OwnedSend<QueueSend<F, tokio::sync::OwnedMutexGuard<()>>, O>,
}

/// Retain the actual native channel lease and install the owner before exposing preparation.
#[cfg(feature = "native-webrtc")]
pub(crate) fn prepare_native<F, O>(
    primitive: F,
    permit: SendPermit,
    channel: tokio::sync::OwnedMutexGuard<()>,
    enqueued: Arc<AtomicU64>,
    bytes: u64,
    observer: O,
) -> Result<PreparedQueue<F, O>>
where
    F: Future<Output = Result<()>>,
    O: super::lifecycle::FailureObserver,
{
    QueueSend::new(primitive, permit, channel, enqueued, bytes).map(|queue| PreparedQueue {
        owner: super::owner::OwnedSend::new(queue, observer),
    })
}

#[cfg(feature = "native-webrtc")]
impl<F: Future<Output = Result<()>>, O: super::lifecycle::FailureObserver> PreparedQueue<F, O> {
    /// Consume preparation, first-poll under the lease, then expose only the protected continuation.
    pub(crate) fn start(
        mut self,
        admission: super::gate::FirstPollLease<'_>,
    ) -> (
        super::owner::OwnedSend<impl Future<Output = Result<u64>>, O>,
        Poll<Result<u64>>,
    ) {
        let first_poll = self.owner.poll_admitted(admission);
        (self.owner, first_poll)
    }
}

/// Execute a non-yielding browser primitive inside the shared owner.
/// Construction, offset snapshot and admission occur during this single poll.
/// A caller cannot supply a Future here or retain an unpolled raw queue snapshot.
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
pub(crate) async fn send_sync<O: super::lifecycle::FailureObserver>(
    send: impl FnOnce() -> Result<()>,
    permit: SendPermit,
    enqueued: Arc<AtomicU64>,
    bytes: u64,
    observer: O,
) -> Result<u64> {
    let queue = QueueSend::new(async move { send() }, permit, (), enqueued, bytes)?;
    super::owner::OwnedSend::new(queue, observer).await
}

/// White-box regression hook for destruction-order tests, absent from production builds.
#[cfg(test)]
pub(crate) fn test_queue<F: Future<Output = Result<()>>, L: Unpin>(
    primitive: F,
    permit: SendPermit,
    channel: L,
    enqueued: Arc<AtomicU64>,
    bytes: u64,
) -> Result<impl Future<Output = Result<u64>>> {
    QueueSend::new(primitive, permit, channel, enqueued, bytes)
}
