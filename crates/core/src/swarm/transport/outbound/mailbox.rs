//! The outbound worker's command mailbox.
//!
//! One unbounded channel whose sender gate linearises submission validation
//! with `close`: a command is validated and enqueued under the same lock that
//! closes the channel, so a submission that saw the mailbox open is drained by
//! the worker, and one that saw it closed is returned to the caller.
//! Ordering between transfers is not the mailbox's concern: every drain hands
//! the whole backlog to the transfer queues, whose control-first burst law
//! decides which frame goes next.

use std::sync::Mutex;

use futures::channel::mpsc;
use futures::future::FutureExt;
use futures::stream::StreamExt;

pub(super) struct MailboxSender<T> {
    sender: Mutex<mpsc::UnboundedSender<T>>,
}

pub(super) struct MailboxReceiver<T> {
    receiver: mpsc::UnboundedReceiver<T>,
    closed: bool,
}

pub(super) fn channel<T>() -> (MailboxSender<T>, MailboxReceiver<T>) {
    let (sender, receiver) = mpsc::unbounded();
    (
        MailboxSender {
            sender: Mutex::new(sender),
        },
        MailboxReceiver {
            receiver,
            closed: false,
        },
    )
}

impl<T> MailboxSender<T> {
    pub(super) fn send(&self, item: T) -> Result<(), ()> {
        let sender = self.sender.lock().map_err(|_| ())?;
        sender.unbounded_send(item).map_err(|_| ())
    }

    /// Enqueue `item` only if `predicate` holds under the sender gate; a
    /// rejected or refused item is handed back to the caller.
    pub(super) fn send_if(&self, item: T, predicate: impl FnOnce(&T) -> bool) -> Result<(), T> {
        let sender = match self.sender.lock() {
            Ok(sender) => sender,
            Err(_) => return Err(item),
        };
        if !predicate(&item) {
            return Err(item);
        }
        sender
            .unbounded_send(item)
            .map_err(|error| error.into_inner())
    }

    pub(super) fn close(&self) {
        let sender = match self.sender.lock() {
            Ok(sender) => sender,
            Err(poisoned) => poisoned.into_inner(),
        };
        sender.close_channel();
    }
}

impl<T> MailboxReceiver<T> {
    /// Every command that is available now, in submission order.
    pub(super) fn drain_available(&mut self) -> Vec<T> {
        let mut drained = Vec::new();
        while !self.closed {
            match self.receiver.next().now_or_never() {
                Some(Some(item)) => drained.push(item),
                Some(None) => self.closed = true,
                None => break,
            }
        }
        drained
    }

    pub(super) fn close(&mut self) {
        self.receiver.close();
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed
    }

    pub(super) async fn next(&mut self) -> Option<T> {
        if self.closed {
            return None;
        }
        let item = self.receiver.next().await;
        if item.is_none() {
            self.closed = true;
        }
        item
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validation_and_drain_share_the_sender_linearization_boundary() {
        let (sender, mut receiver) = channel();
        sender.send(1).expect("mailbox open");
        assert_eq!(sender.send_if(2, |_| false), Err(2));
        sender.send(3).expect("mailbox open");

        assert_eq!(receiver.drain_available(), vec![1, 3]);
        assert!(receiver.drain_available().is_empty());
    }

    #[test]
    fn test_drain_hands_over_the_whole_backlog_in_submission_order() {
        let (sender, mut receiver) = channel();
        for index in 0..64 {
            sender.send(index).expect("mailbox open");
        }

        assert_eq!(receiver.drain_available(), (0..64).collect::<Vec<_>>());
        assert!(!receiver.is_closed());
        sender.close();
        assert!(receiver.drain_available().is_empty());
        assert!(receiver.is_closed());
    }

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
}
