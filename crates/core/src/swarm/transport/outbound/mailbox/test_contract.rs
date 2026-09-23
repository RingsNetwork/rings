//! Common native/browser contracts for bounded ingress and queue composition.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use futures::future::FutureExt;
use futures::task::ArcWake;

use super::*;
use crate::swarm::transport::outbound::model::TransferClass;
use crate::swarm::transport::outbound::queue::TransferQueues;

/// A bulk backlog cannot hide a control admitted before the FIFO drain.
/// Producers after that boundary cannot lengthen the owned batch or lose a scan.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn batch_exposes_control_and_preserves_the_next_scan() {
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
    let selected = queues.pop().expect("batch contains runnable control");
    assert_eq!(selected.class(), TransferClass::DhtControl);
    assert_eq!(*selected.item(), 64);
    assert_eq!(receiver.drain_available(), vec![
        Some((TransferClass::Storage, 65)),
        None
    ]);
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

/// Native producer contention cannot enlarge a detached batch; cancellation
/// flooding retains at most one command independently of notification count.
#[cfg(not(target_family = "wasm"))]
#[test]
fn concurrent_notifications_cannot_extend_the_submission_batch() {
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

/// Idle receipt releases the scan slot before dispatch, and a closed full slot
/// must not be mistaken for a live coalesced notification.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn idle_receipt_reopens_scan_slot_and_close_rejects_notifications() {
    let (sender, mut receiver) = channel();
    assert!(sender.send_coalesced(1).is_ok());
    assert!(sender.send_coalesced(1).is_ok());
    assert_eq!(receiver.next().now_or_never(), Some(Some(1)));
    assert!(sender.send_coalesced(2).is_ok());
    sender.close();
    assert!(sender.send_coalesced(3).is_err());
    assert_eq!(receiver.drain_available(), vec![2]);
    assert_eq!(receiver.next().now_or_never(), Some(None));
}
