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

use tokio::sync::OwnedMutexGuard;

use super::send_model::end_offset;
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
pub(super) struct QueueSend<F> {
    /// Primitive send, independently pinned without owning the channel lease.
    primitive: Pin<Box<F>>,
    /// One linear capability state; the primitive never owns its permit/proof.
    admission: QueueAdmission,
    /// Channel serialization lease; retained through the outer failure boundary.
    _channel: OwnedMutexGuard<()>,
    /// Successful queue-admission byte counter for this physical data channel.
    enqueued: Arc<AtomicU64>,
    /// Checked cumulative end offset published only after successful queue admission.
    end: u64,
}

impl<F: Future<Output = Result<()>>> QueueSend<F> {
    /// Assemble an unpolled operation; the first poll must hold generation admission.
    pub(super) fn new(
        primitive: F,
        permit: SendPermit,
        channel: OwnedMutexGuard<()>,
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
            .ok_or(Error::NativeSendByteCountOverflow)
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

impl<F: Future<Output = Result<()>>> Future for QueueSend<F> {
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
