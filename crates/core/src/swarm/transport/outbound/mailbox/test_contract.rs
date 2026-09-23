//! Common native/browser contracts for snapshot ingress and queue composition.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;

use futures::future::FutureExt;
use futures::task::ArcWake;

use super::*;
use crate::swarm::transport::outbound::model::TransferClass;
use crate::swarm::transport::outbound::queue::TransferQueues;

/// Exhaust all 6^6 traces with three permit slots: submit, two equivalent
/// notifications, snapshot, idle receive, close. The oracle tracks accepted FIFO
/// ownership independently; it does not model executor fairness or transport IO.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn finite_reducer_traces_preserve_ownership_and_batch_bound() {
    for encoded in 0..6_usize.pow(6) {
        let mut code = encoded;
        let mut actual = MailboxState::default();
        let mut fifo = VecDeque::new();
        let mut notified = false;
        let mut closed = false;
        for id in 0..6 {
            let action = code % 6;
            code /= 6;
            match action {
                0 if fifo.len() < 3 => {
                    assert_eq!(
                        actual.submit(Some(id)),
                        if closed { Err(Some(id)) } else { Ok(()) }
                    );
                    if !closed {
                        fifo.push_back(Some(id));
                    }
                }
                1 | 2 => {
                    assert_eq!(actual.notify(None), if closed { Err(None) } else { Ok(()) });
                    notified |= !closed;
                }
                3 => {
                    let batch = actual.snapshot();
                    assert!(batch.len() <= 4);
                    let mut expected = std::mem::take(&mut fifo);
                    if notified {
                        expected.push_back(None);
                    }
                    notified = false;
                    assert_eq!(batch, expected);
                }
                4 => {
                    let expected = fifo
                        .pop_front()
                        .or_else(|| std::mem::take(&mut notified).then_some(None));
                    assert_eq!(actual.poll_next(), match expected {
                        Some(item) => Poll::Ready(Some(item)),
                        None if closed => Poll::Ready(None),
                        None => Poll::Pending,
                    });
                }
                5 => {
                    actual.close();
                    closed = true;
                }
                _ => {}
            }
            assert_eq!(
                actual.is_terminated(),
                closed && fifo.is_empty() && !notified
            );
        }
        actual.close();
        let mut expected = fifo;
        if notified {
            expected.push_back(None);
        }
        assert_eq!(actual.snapshot(), expected);
        assert!(actual.is_terminated());
    }
}

/// A bulk backlog cannot hide a control admitted before the snapshot boundary.
/// Producers after that boundary cannot lengthen the owned batch or lose a scan.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn snapshot_exposes_control_and_defers_concurrent_producers_to_next_batch() {
    let (sender, mut receiver) = channel();
    for id in 0..64 {
        assert!(sender
            .send_if(Some((TransferClass::Application, id)), |_| true)
            .is_ok());
    }
    assert!(sender
        .send_if(Some((TransferClass::DhtControl, 64)), |_| true)
        .is_ok());
    for _ in 0..1024 {
        assert!(sender.send_coalesced(None).is_ok());
    }
    let batch = receiver.drain_available();
    assert_eq!(batch.len(), 66);
    assert!(sender
        .send_if(Some((TransferClass::Storage, 65)), |_| true)
        .is_ok());
    assert!(sender.send_coalesced(None).is_ok());
    let mut queues = TransferQueues::default();
    let mut scans = 0;
    for command in batch {
        match command {
            Some((class, id)) => queues.push(class, id),
            None => scans += 1,
        }
    }
    assert_eq!(scans, 1);
    let selected = queues.pop().expect("snapshot contains runnable control");
    assert_eq!(selected.class(), TransferClass::DhtControl);
    assert_eq!(*selected.item(), 64);
    assert_eq!(
        receiver.drain_available(),
        VecDeque::from([Some((TransferClass::Storage, 65)), None])
    );
}

/// Counts executor notifications without running a platform-specific executor.
struct WakeCount(AtomicUsize);
impl ArcWake for WakeCount {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Idle receive is woken by submission, cancellation, and closure. The same
/// register-before-check protocol runs in native executors and browser tasks.
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

/// Native producer contention cannot enlarge a detached batch; cancellation
/// flooding retains at most one command independently of notification count.
#[cfg(not(target_family = "wasm"))]
#[test]
fn concurrent_notifications_cannot_extend_the_submission_snapshot() {
    let (sender, mut receiver) = channel();
    let sender = Arc::new(sender);
    for id in 0..256 {
        assert!(sender.send_if(Some(id), |_| true).is_ok());
    }
    let gate = Arc::new(std::sync::Barrier::new(2));
    let producer = {
        let sender = Arc::clone(&sender);
        let gate = Arc::clone(&gate);
        std::thread::spawn(move || {
            gate.wait();
            for _ in 0..10_000 {
                assert!(sender.send_coalesced(None).is_ok());
            }
        })
    };
    gate.wait();
    let batch = receiver.drain_available();
    assert!(batch.len() <= 257);
    assert_eq!(
        batch.into_iter().flatten().collect::<Vec<_>>(),
        (0..256).collect::<Vec<_>>()
    );
    producer.join().expect("notification producer finishes");
    assert!(receiver.drain_available().len() <= 1);
}
