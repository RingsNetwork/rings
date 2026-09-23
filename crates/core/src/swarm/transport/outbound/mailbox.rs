//! Outbound command ingress using futures channels for ownership and wakeups.
//!
//! Submissions retain their permits while collected, so the FIFO drain is bounded
//! by the peer's 256 permits even with concurrent producers. Cancellation has one
//! channel slot and is read once per drain, preventing repeated scan notifications
//! from extending that batch. Control transfers stay in the complete FIFO batch;
//! the transfer queues alone decide frame priority.

use std::sync::Mutex;

use futures::channel::mpsc;
use futures::future::FutureExt;
use futures::stream::select;
use futures::stream::Select;
use futures::stream::StreamExt;

/// Producer endpoints share the existing validation/close gate.
pub(super) struct MailboxSender<T> {
    /// Submission FIFO and sole notification sender. Never clone the bounded
    /// sender: `channel(0)` has one reserved slot per sender, hence one slot here.
    sender: Mutex<(mpsc::UnboundedSender<T>, mpsc::Sender<T>)>,
}

/// Both input sources use the library's stream selection and wakeup machinery.
pub(super) struct MailboxReceiver<T> {
    /// Idle selection is fair; active drains collect submissions then one scan.
    receiver: Select<mpsc::UnboundedReceiver<T>, mpsc::Receiver<T>>,
    /// A drained submission channel seals ingress; shutdown cancels all owners.
    closed: bool,
}

/// Connect the transfer FIFO and a single pending idempotent scan notification.
pub(super) fn channel<T>() -> (MailboxSender<T>, MailboxReceiver<T>) {
    let (sender, receiver) = mpsc::unbounded();
    let (notification, notifications) = mpsc::channel(0);
    (
        MailboxSender {
            sender: Mutex::new((sender, notification)),
        },
        MailboxReceiver {
            receiver: select(receiver, notifications),
            closed: false,
        },
    )
}

impl<T> MailboxSender<T> {
    /// A full slot already promises a scan. Callers set their stop token before
    /// sending; after receipt frees the slot, a later stop queues a fresh scan.
    pub(super) fn send_coalesced(&self, item: T) -> Result<(), ()> {
        let mut sender = self.sender.lock().map_err(|_| ())?;
        match sender.1.try_send(item) {
            Ok(()) => Ok(()),
            Err(error) if error.is_full() && !sender.1.is_closed() => Ok(()),
            Err(_) => Err(()),
        }
    }

    /// Validate and insert under the original sender gate, returning refused ownership.
    pub(super) fn send_if(&self, item: T, predicate: impl FnOnce(&T) -> bool) -> Result<(), T> {
        match self.sender.lock() {
            Ok(sender) if predicate(&item) => sender
                .0
                .unbounded_send(item)
                .map_err(|error| error.into_inner()),
            _ => Err(item),
        }
    }

    /// Seal both channels under the submission gate; retain accepted commands.
    pub(super) fn close(&self) {
        let mut sender = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sender.0.close_channel();
        sender.1.close_channel();
    }
}

impl<T> MailboxReceiver<T> {
    /// Collect the permit-bounded FIFO before handling any command. No permit is
    /// released in this loop, so producers cannot recycle capacity to prolong it.
    /// Read only one notification even if producers refill its slot immediately.
    pub(super) fn drain_available(&mut self) -> Vec<T> {
        let mut drained = Vec::new();
        let (submissions, notifications) = self.receiver.get_mut();
        while !self.closed {
            match submissions.next().now_or_never() {
                Some(Some(item)) => drained.push(item),
                Some(None) => self.closed = true,
                None => break,
            }
        }
        if let Some(Some(notification)) = notifications.next().now_or_never() {
            drained.push(notification);
        }
        drained
    }

    /// Reject new ingress before the worker collects its shutdown batch.
    pub(super) fn close(&mut self) {
        let (submissions, notifications) = self.receiver.get_mut();
        submissions.close();
        notifications.close();
    }

    /// Report drained submission closure to the worker's shutdown path.
    pub(super) fn is_closed(&self) -> bool {
        self.closed
    }

    /// Await either input using the existing futures stream wakeup contract.
    pub(super) async fn next(&mut self) -> Option<T> {
        let item = self.receiver.next().await;
        if item.is_none() {
            self.closed = true;
        }
        item
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::task::Context;
    use std::task::Poll;

    use futures::task::ArcWake;

    use super::*;
    use crate::swarm::transport::outbound::TransferClass;
    use crate::swarm::transport::outbound::TransferQueues;

    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_validation_and_drain_share_the_sender_linearization_boundary() {
        let (sender, mut receiver) = channel();
        sender.send_if(1, |_| true).expect("mailbox open");
        assert_eq!(sender.send_if(2, |_| false), Err(2));
        sender.send_if(3, |_| true).expect("mailbox open");

        assert_eq!(receiver.drain_available(), vec![1, 3]);
        assert!(receiver.drain_available().is_empty());
        // Receipt must reopen the notification slot before the worker scans.
        for _ in 0..1024 {
            assert!(sender.send_coalesced(7).is_ok());
        }
        assert_eq!(receiver.next().now_or_never(), Some(Some(7)));
        assert!(sender.send_coalesced(8).is_ok());
        sender.close();
        assert!(sender.send_coalesced(9).is_err());
        assert_eq!(receiver.drain_available(), vec![8]);
        assert_eq!(receiver.next().now_or_never(), Some(None));
    }

    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_drain_hands_over_the_whole_backlog_in_submission_order() {
        let (sender, mut receiver) = channel();
        let sender = Arc::new(sender);
        for _ in 0..64 {
            sender
                .send_if(Some(TransferClass::Application), |_| true)
                .expect("mailbox open");
        }
        sender
            .send_if(Some(TransferClass::DhtControl), |_| true)
            .expect("mailbox open");
        assert!(sender.send_coalesced(None).is_ok());
        // Native producers can refill the slot during collection; the batch still
        // contains one scan. Browser tasks use the same bound without OS threads.
        #[cfg(not(target_family = "wasm"))]
        let producer = {
            let sender = Arc::clone(&sender);
            std::thread::spawn(move || {
                for _ in 0..10_000 {
                    assert!(sender.send_coalesced(None).is_ok());
                }
            })
        };
        let batch = receiver.drain_available();
        assert_eq!(batch.len(), 66);
        assert_eq!(batch.last(), Some(&None));
        let mut queues = TransferQueues::default();
        for class in batch.into_iter().flatten() {
            queues.push(class, ());
        }
        assert_eq!(
            queues.pop().expect("runnable control").class(),
            TransferClass::DhtControl
        );
        #[cfg(not(target_family = "wasm"))]
        producer.join().expect("producer finishes");
        assert!(sender.send_coalesced(None).is_ok());
        assert_eq!(receiver.drain_available(), vec![None]);
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

        assert_eq!(receiver.drain_available(), vec![1]);
        assert_eq!(sender.send_if(2, |_| true), Err(2));
    }

    /// Counts executor notifications without running a platform-specific executor.
    struct WakeCount(AtomicUsize);
    impl ArcWake for WakeCount {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Idle receive is woken by submission, cancellation, and closure. The same
    /// futures channel selection runs in native executors and browser tasks.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn idle_actor_observes_each_wakeup_source() {
        for action in 0..3 {
            let (sender, mut receiver) = channel();
            let counter = Arc::new(WakeCount(AtomicUsize::new(0)));
            let wake = futures::task::waker(Arc::clone(&counter));
            let mut context = Context::from_waker(&wake);
            let waiting = receiver.next();
            futures::pin_mut!(waiting);
            assert!(waiting.as_mut().poll_unpin(&mut context).is_pending());
            match action {
                0 => assert!(sender.send_if(7, |_| true).is_ok()),
                1 => assert!(sender.send_coalesced(7).is_ok()),
                _ => sender.close(),
            }
            assert!(counter.0.load(Ordering::SeqCst) > 0);
            assert_eq!(
                waiting.as_mut().poll_unpin(&mut context),
                Poll::Ready((action != 2).then_some(7))
            );
        }
    }
}
