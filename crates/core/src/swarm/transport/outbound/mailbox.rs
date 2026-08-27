//! Two-lane worker mailbox with cancellation/control precedence.
//!
//! Capacity is owned by each submitted item before it enters this unbounded
//! channel. Priority input is observed first, but each drain has a fixed budget
//! and then includes regular input, so a sustained priority stream cannot hide
//! already-buffered regular commands forever. Closing is serialized with
//! validated submission by the sender mutex.

use std::sync::Mutex;

use futures::channel::mpsc;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::stream::StreamExt;

struct Senders<T> {
    priority: mpsc::UnboundedSender<T>,
    regular: mpsc::UnboundedSender<T>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MailboxLane {
    Priority,
    Regular,
}

pub(super) struct MailboxSender<T> {
    senders: Mutex<Senders<T>>,
}

pub(super) struct MailboxReceiver<T> {
    priority: mpsc::UnboundedReceiver<T>,
    regular: mpsc::UnboundedReceiver<T>,
    priority_closed: bool,
    regular_closed: bool,
}

pub(super) fn channel<T>() -> (MailboxSender<T>, MailboxReceiver<T>) {
    let (priority_sender, priority_receiver) = mpsc::unbounded();
    let (regular_sender, regular_receiver) = mpsc::unbounded();
    (
        MailboxSender {
            senders: Mutex::new(Senders {
                priority: priority_sender,
                regular: regular_sender,
            }),
        },
        MailboxReceiver {
            priority: priority_receiver,
            regular: regular_receiver,
            priority_closed: false,
            regular_closed: false,
        },
    )
}

impl<T> MailboxSender<T> {
    pub(super) fn send(&self, item: T, lane: MailboxLane) -> Result<(), ()> {
        let senders = self.senders.lock().map_err(|_| ())?;
        let sender = match lane {
            MailboxLane::Priority => &senders.priority,
            MailboxLane::Regular => &senders.regular,
        };
        sender.unbounded_send(item).map_err(|_| ())
    }

    pub(super) fn send_if(
        &self,
        item: T,
        lane: MailboxLane,
        predicate: impl FnOnce(&T) -> bool,
    ) -> Result<(), T> {
        let senders = match self.senders.lock() {
            Ok(senders) => senders,
            Err(_) => return Err(item),
        };
        if !predicate(&item) {
            return Err(item);
        }
        let sender = match lane {
            MailboxLane::Priority => &senders.priority,
            MailboxLane::Regular => &senders.regular,
        };
        sender
            .unbounded_send(item)
            .map_err(|error| error.into_inner())
    }

    pub(super) fn close(&self) {
        let senders = match self.senders.lock() {
            Ok(senders) => senders,
            Err(poisoned) => poisoned.into_inner(),
        };
        senders.priority.close_channel();
        senders.regular.close_channel();
    }
}

impl<T> MailboxReceiver<T> {
    pub(super) fn drain_available(&mut self, budget: usize) -> Vec<T> {
        let mut drained = Vec::new();
        Self::drain_stream(
            &mut self.priority,
            &mut self.priority_closed,
            budget,
            &mut drained,
        );
        Self::drain_stream(
            &mut self.regular,
            &mut self.regular_closed,
            budget,
            &mut drained,
        );
        drained
    }

    fn drain_stream(
        stream: &mut mpsc::UnboundedReceiver<T>,
        closed: &mut bool,
        budget: usize,
        drained: &mut Vec<T>,
    ) {
        while !*closed && drained.len() < budget {
            match stream.next().now_or_never() {
                Some(Some(item)) => drained.push(item),
                Some(None) => *closed = true,
                None => break,
            }
        }
    }

    pub(super) fn close(&mut self) {
        self.priority.close();
        self.regular.close();
    }

    #[cfg(test)]
    pub(super) fn drain_regular_available(&mut self) -> Vec<T> {
        let mut drained = Vec::new();
        Self::drain_stream(
            &mut self.regular,
            &mut self.regular_closed,
            usize::MAX,
            &mut drained,
        );
        drained
    }

    pub(super) fn drain_all(&mut self) -> Vec<T> {
        self.drain_available(usize::MAX)
    }

    pub(super) fn is_closed(&self) -> bool {
        self.priority_closed && self.regular_closed
    }

    pub(super) async fn next(&mut self) -> Option<T> {
        loop {
            match (self.priority_closed, self.regular_closed) {
                (true, true) => return None,
                (false, true) => match self.priority.next().await {
                    Some(item) => return Some(item),
                    None => self.priority_closed = true,
                },
                (true, false) => match self.regular.next().await {
                    Some(item) => return Some(item),
                    None => self.regular_closed = true,
                },
                (false, false) => {
                    enum Input<T> {
                        Priority(Option<T>),
                        Regular(Option<T>),
                    }
                    let input = {
                        let priority = self.priority.next().fuse();
                        let regular = self.regular.next().fuse();
                        pin_mut!(priority, regular);
                        futures::select_biased! {
                            item = priority => Input::Priority(item),
                            item = regular => Input::Regular(item),
                        }
                    };
                    match input {
                        Input::Priority(Some(item)) | Input::Regular(Some(item)) => {
                            return Some(item);
                        }
                        Input::Priority(None) => self.priority_closed = true,
                        Input::Regular(None) => self.regular_closed = true,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MailboxLane::Priority;
    use super::MailboxLane::Regular;
    use super::*;

    #[test]
    fn test_priority_submission_bypasses_regular_backlog_beyond_drain_budget() {
        let (sender, mut receiver) = channel();
        for index in 0..64 {
            sender.send(index, Regular).expect("regular mailbox open");
        }
        sender
            .send(usize::MAX, Priority)
            .expect("priority mailbox open");

        let first_batch = receiver.drain_available(32);

        assert_eq!(first_batch.first(), Some(&usize::MAX));
        assert_eq!(first_batch[1..], (0..31).collect::<Vec<_>>());
        assert_eq!(receiver.drain_available(64), (31..64).collect::<Vec<_>>());
    }

    #[test]
    fn test_validation_and_regular_drain_share_the_sender_linearization_boundary() {
        let (sender, mut receiver) = channel();
        sender.send(1, Regular).expect("regular mailbox open");
        assert_eq!(sender.send_if(2, Regular, |_| false), Err(2));
        sender.send(3, Regular).expect("regular mailbox open");

        assert_eq!(receiver.drain_regular_available(), vec![1, 3]);
        assert!(receiver.drain_regular_available().is_empty());
    }

    #[test]
    fn test_cancellation_scan_reaches_the_tail_of_a_regular_backlog() {
        #[derive(Debug, Eq, PartialEq)]
        enum Command {
            Submit(usize),
            Cancel,
        }

        let (sender, mut receiver) = channel();
        for index in 0..64 {
            sender
                .send(Command::Submit(index), Regular)
                .expect("regular mailbox open");
        }
        sender
            .send(Command::Cancel, Priority)
            .expect("priority mailbox open");

        let first_batch = receiver.drain_available(32);
        assert_eq!(first_batch.first(), Some(&Command::Cancel));
        let remainder = receiver.drain_regular_available();
        assert_eq!(remainder.last(), Some(&Command::Submit(63)));
    }

    #[test]
    fn test_drain_all_grows_from_actual_items_instead_of_the_unbounded_budget() {
        let (sender, mut receiver) = channel();
        sender.send(1, Priority).expect("priority mailbox open");
        sender.send(2, Regular).expect("regular mailbox open");

        assert_eq!(receiver.drain_all(), vec![1, 2]);
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
                let result = sender.send_if(1, Regular, |_| {
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

        assert_eq!(receiver.drain_all(), vec![1]);
        assert_eq!(sender.send_if(2, Regular, |_| true), Err(2));
    }
}
