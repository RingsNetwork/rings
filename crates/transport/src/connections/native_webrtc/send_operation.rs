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

use crate::core::transport::IrrevocableSendPermit;
use crate::core::transport::SendPermit;
use crate::error::Error;
use crate::error::Result;

/// Own the serial channel lease and commit acceptance only after successful enqueue.
pub(super) struct QueueSend<F> {
    /// Primitive send, independently pinned without owning the channel lease.
    primitive: Pin<Box<F>>,
    /// Final admission capability, consumed during the first guarded poll.
    permit: Option<SendPermit>,
    /// Queue-acceptance proof, kept outside the primitive across suspension or panic.
    proof: Option<IrrevocableSendPermit>,
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
        // The channel lease excludes all other byte-counter writers until Drop.
        let end = enqueued
            .load(Ordering::SeqCst)
            .checked_add(bytes)
            .ok_or(Error::NativeSendByteCountOverflow)?;
        Ok(Self {
            primitive: Box::pin(primitive),
            permit: Some(permit),
            proof: None,
            _channel: channel,
            enqueued,
            end,
        })
    }
}

impl<F: Future<Output = Result<()>>> Future for QueueSend<F> {
    type Output = Result<u64>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        // Claim exactly once, beneath OwnedSend::poll_admitted's generation gate.
        if let Some(permit) = self.permit.take() {
            self.proof = permit.try_mark_irrevocable();
        }
        // Includes rejected admission and a defensive repeated poll after completion.
        if self.proof.is_none() {
            return Poll::Ready(Err(Error::SendPermitRevoked));
        }
        // On Pending, Err, or panic every resource remains a struct field, so
        // OwnedSend observes failure before automatic field destruction.
        match self.primitive.as_mut().poll(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                if let Some(proof) = self.proof.take() {
                    proof.mark_accepted();
                }
                // The counter follows the successful primitive in channel order.
                self.enqueued.store(self.end, Ordering::SeqCst);
                Poll::Ready(Ok(self.end))
            }
        }
    }
}
