//! Pure ingress reducer, shared by native and WASM adapters and model tests.

use super::Poll;
use super::VecDeque;

/// State relation: Submit appends while open; Notify replaces the equivalent
/// pending scan; Snapshot transfers all ownership; Close permanently seals ingress.
/// Invariant: each accepted transfer belongs to exactly one FIFO or detached batch.
/// The caller's permit system, not this generic reducer, bounds FIFO cardinality.
pub(super) struct MailboxState<T> {
    /// FIFO of admitted transfers, independent of scheduler class priority.
    submissions: VecDeque<T>,
    /// One pending idempotent scan command; extraction clears its ownership.
    notification: Option<T>,
    /// Monotonic ingress seal shared by all producers and the consumer.
    closed: bool,
}

impl<T> Default for MailboxState<T> {
    fn default() -> Self {
        Self {
            submissions: VecDeque::new(),
            notification: None,
            closed: false,
        }
    }
}

impl<T> MailboxState<T> {
    /// Move a submission into the FIFO or return its entire ownership after close.
    pub(super) fn submit(&mut self, item: T) -> Result<(), T> {
        if self.closed {
            Err(item)
        } else {
            self.submissions.push_back(item);
            Ok(())
        }
    }

    /// Coalesce equivalent notifications without changing transfer ownership.
    pub(super) fn notify(&mut self, item: T) -> Result<(), T> {
        if self.closed {
            Err(item)
        } else {
            self.notification = Some(item);
            Ok(())
        }
    }

    /// Seal ingress. Already accepted work remains available to the shutdown batch.
    pub(super) fn close(&mut self) {
        self.closed = true;
    }

    /// Detach the entire finite FIFO and its scan in one reducer transition.
    pub(super) fn snapshot(&mut self) -> VecDeque<T> {
        let mut batch = std::mem::take(&mut self.submissions);
        batch.extend(self.notification.take());
        batch
    }

    /// Observe drained closure, never merely an empty but open ingress.
    pub(super) fn is_terminated(&self) -> bool {
        self.closed && self.submissions.is_empty() && self.notification.is_none()
    }

    /// Consume one command for the idle actor, or expose its pending/closed state.
    pub(super) fn poll_next(&mut self) -> Poll<Option<T>> {
        match self
            .submissions
            .pop_front()
            .or_else(|| self.notification.take())
        {
            Some(item) => Poll::Ready(Some(item)),
            None if self.closed => Poll::Ready(None),
            None => Poll::Pending,
        }
    }
}
