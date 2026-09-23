//! Executor-neutral snapshot mailbox for the outbound actor.
//!
//! The sender gate linearizes validation, insertion, snapshots and close. A
//! snapshot moves the entire FIFO into worker ownership at one gate boundary; new
//! producers cannot extend it. Transfer permits bound that FIFO to 256 entries.
//! The sole idempotent notification, `CancelStopped`, occupies one separate
//! slot. It is moved with the snapshot and dispatched after its submissions.
//! This is coalescing of one scan command, not a second cancellation flag.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::task::Poll;

use futures::future::poll_fn;
use futures::task::AtomicWaker;

mod state;
use state::MailboxState;

/// Shared effect boundary: serialization and executor wakeup only.
struct Shared<T> {
    /// Pure ingress state; no callbacks or IO occur in its transitions.
    state: Mutex<MailboxState<T>>,
    /// The single actor waiting for a command or closure.
    wake: AtomicWaker,
}

impl<T> Shared<T> {
    /// Recover ownership even after a panicking validation callback.
    fn lock(&self) -> MutexGuard<'_, MailboxState<T>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Close under the same gate as submission, then notify outside the lock.
    fn close(&self) {
        self.lock().close();
        self.wake.wake();
    }
}

/// Sole producer handle, shared by reference or Arc by its actor clients.
pub(super) struct MailboxSender<T> {
    /// Shared ownership keeps ingress alive until both endpoints disappear.
    shared: Arc<Shared<T>>,
}

/// Single consumer; batches belong exclusively to this actor after extraction.
pub(super) struct MailboxReceiver<T> {
    /// The same gate used by submission validation and close.
    shared: Arc<Shared<T>>,
}

/// Construct the platform-independent command ingress and its single consumer.
pub(super) fn channel<T>() -> (MailboxSender<T>, MailboxReceiver<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(MailboxState::default()),
        wake: AtomicWaker::new(),
    });
    (
        MailboxSender {
            shared: Arc::clone(&shared),
        },
        MailboxReceiver { shared },
    )
}

impl<T> MailboxSender<T> {
    /// Schedule one idempotent scan. Callers must use this only for equivalent
    /// notifications whose predicate is stored outside the command (stop tokens).
    /// A scan requested after snapshot extraction occupies the next batch.
    pub(super) fn send_coalesced(&self, item: T) -> Result<(), ()> {
        let result = self.shared.lock().notify(item);
        self.shared.wake.wake();
        result.map_err(|_| ())
    }

    /// Validate and insert under the close gate, returning refused ownership.
    pub(super) fn send_if(&self, item: T, predicate: impl FnOnce(&T) -> bool) -> Result<(), T> {
        let result = {
            let mut state = self.shared.lock();
            if predicate(&item) {
                state.submit(item)
            } else {
                Err(item)
            }
        };
        self.shared.wake.wake();
        result
    }

    /// Prevent new submissions without discarding previously accepted ownership.
    pub(super) fn close(&self) {
        self.shared.close();
    }
}

impl<T> Drop for MailboxSender<T> {
    fn drop(&mut self) {
        self.close();
    }
}

impl<T> MailboxReceiver<T> {
    /// Atomically detach the finite batch, including its pending scan command.
    /// Production bound: 256 permit-bearing submissions plus one notification.
    pub(super) fn drain_available(&mut self) -> VecDeque<T> {
        self.shared.lock().snapshot()
    }

    /// Close the same gate used by producers before shutdown drains ownership.
    pub(super) fn close(&mut self) {
        self.shared.close();
    }

    /// Closure is terminal only after all accepted commands have been extracted.
    pub(super) fn is_closed(&self) -> bool {
        self.shared.lock().is_terminated()
    }

    /// Register before inspecting state, preventing a send/close between the
    /// empty check and registration from losing the actor's wakeup.
    pub(super) async fn next(&mut self) -> Option<T> {
        poll_fn(|context| {
            self.shared.wake.register(context.waker());
            self.shared.lock().poll_next()
        })
        .await
    }
}

impl<T> Drop for MailboxReceiver<T> {
    fn drop(&mut self) {
        self.close();
        // Release payloads outside the gate: their Drop may wake producers.
        drop(self.drain_available());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_validation_and_drain_share_the_sender_linearization_boundary() {
        let (sender, mut receiver) = channel();
        sender.send_if(1, |_| true).expect("mailbox open");
        assert_eq!(sender.send_if(2, |_| false), Err(2));
        sender.send_if(3, |_| true).expect("mailbox open");

        assert_eq!(receiver.drain_available(), VecDeque::from([1, 3]));
        assert!(receiver.drain_available().is_empty());
    }

    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_drain_hands_over_the_whole_backlog_in_submission_order() {
        let (sender, mut receiver) = channel();
        for index in 0..64 {
            sender.send_if(index, |_| true).expect("mailbox open");
        }

        assert_eq!(receiver.drain_available(), (0..64).collect::<VecDeque<_>>());
        assert!(!receiver.is_closed());
        sender.close();
        assert!(receiver.drain_available().is_empty());
        assert!(receiver.is_closed());
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn test_send_validation_linearizes_with_close_under_contention() {
        let (sender, mut receiver) = channel();
        let sender = std::sync::Arc::new(sender);
        let (validation_entered_tx, validation_entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_validation_tx, release_validation_rx) = std::sync::mpsc::sync_channel(0);
        let (send_done_tx, send_done_rx) = std::sync::mpsc::channel();
        let submitting = {
            let sender = std::sync::Arc::clone(&sender);
            std::thread::spawn(move || {
                let result = sender.send_if(1, |_| {
                    validation_entered_tx
                        .send(())
                        .expect("validation observer must remain open");
                    release_validation_rx
                        .recv_timeout(std::time::Duration::from_secs(1))
                        .expect("validation must be released within the test bound");
                    true
                });
                let _ = send_done_tx.send(result);
            })
        };
        validation_entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("send_if must hold the sender gate during validation");

        let (close_done_tx, close_done_rx) = std::sync::mpsc::channel();
        let closing = {
            let sender = std::sync::Arc::clone(&sender);
            std::thread::spawn(move || {
                sender.close();
                let _ = close_done_tx.send(());
            })
        };
        release_validation_tx
            .send(())
            .expect("validation release receiver must remain open");
        assert_eq!(
            send_done_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("validated submission must finish"),
            Ok(())
        );
        close_done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("close must finish after the validated submission");
        submitting.join().expect("submission thread must not panic");
        closing.join().expect("close thread must not panic");

        assert_eq!(receiver.drain_available(), VecDeque::from([1]));
        assert_eq!(sender.send_if(2, |_| true), Err(2));
    }
}

#[cfg(test)]
mod test_contract;
