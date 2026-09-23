//! Deterministic cancellation interleaving with a blocked same-category head.

use std::task::Context;

use futures::channel::oneshot;
use futures::task::ArcWake;

use super::*;
use crate::ecc::SecretKey;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::swarm::Swarm;
use crate::tests::manually_establish_connection;

/// Native setup adapter: retain the established dummy fixture's admission gates.
#[cfg(not(target_family = "wasm"))]
async fn connected_swarms() -> (Arc<Swarm>, Arc<Swarm>) {
    use rings_transport::core::transport::WebrtcConnectionState;

    use crate::tests::default::prepare_node;
    use crate::tests::default::wait_for_connection_state;
    use crate::tests::default::wait_for_msgs;
    use crate::tests::default::wait_for_successor;
    let node = prepare_node(SecretKey::random()).await;
    let remote = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node.swarm, &remote.swarm).await;
    wait_for_connection_state(&node, remote.did(), WebrtcConnectionState::Connected)
        .await
        .expect("fixture connects");
    wait_for_successor(&node, remote.did())
        .await
        .expect("fixture admits peer");
    wait_for_msgs([&node, &remote]).await;
    (node.swarm, remote.swarm)
}

/// Browser setup adapter: real RTC establishment, then bounded admission polling.
/// The subsequent worker state and assertions are identical on both platforms.
#[cfg(target_family = "wasm")]
async fn connected_swarms() -> (Arc<Swarm>, Arc<Swarm>) {
    let node = crate::tests::wasm::prepare_node(SecretKey::random()).await;
    let remote = crate::tests::wasm::prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node, &remote).await;
    for _ in 0..400 {
        if node
            .transport
            .admitted_send_connection(remote.did())
            .expect("registry readable")
            .is_some()
        {
            break;
        }
        crate::utils::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(node
        .transport
        .admitted_send_connection(remote.did())
        .expect("registry readable")
        .is_some());
    (node, remote)
}

/// Attach the real admission permit to a tracked transfer fixture.
fn scheduled_transfer(
    node: &Swarm,
    peer: Did,
    capacity: &Arc<TransferCapacity>,
    stop: &StopSource,
) -> (
    ScheduledTransfer,
    oneshot::Receiver<Result<SendCompletionOutcome>>,
) {
    let (transfer, completion) = tracked_transfer(node, peer, stop);
    let permit = capacity
        .try_acquire(peer, TransferClass::Application, 1)
        .expect("fixture fits capacity");
    (ScheduledTransfer::new(transfer, permit), completion)
}

/// Construct a signed, admitted transfer shared by native and browser tests.
fn tracked_transfer(
    node: &Swarm,
    peer: Did,
    stop: &StopSource,
) -> (
    OutboundTransfer,
    oneshot::Receiver<Result<SendCompletionOutcome>>,
) {
    // The admitted connection is shared by all three transfers in this lane.
    let admitted = node
        .transport
        .admitted_send_connection(peer)
        .expect("connection registry is readable")
        .expect("peer is admitted");
    // Each logical payload retains the real signer and destination semantics.
    let payload = MessagePayload::new_send(
        Message::custom(b"cancel-race").expect("fixture message is valid"),
        node.transport.message_signer(),
        peer,
        peer,
    )
    .expect("fixture payload signs");
    // Completion and stop state belong to this transfer, independently of its siblings.
    let (transfer, completion) = OutboundTransfer::whole(
        OutboundTransferRoute::new(
            TransferClass::Application,
            peer,
            admitted,
            ChunkSendPermit::Always,
        ),
        payload,
        11,
        OutboundCompletion::Tracked,
        stop.token(),
        None,
    );
    (transfer, completion)
}

/// A stop published after the scan reclaims its successor before head delivery.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
async fn test_cancellation_after_scan_releases_successor_behind_waiting_head() {
    // Only connection setup uses the runtime; the worker interleaving is synchronous.
    let (node, remote) = connected_swarms().await;
    let peer = remote.did();
    // This private capacity accountant excludes connection-setup traffic.
    let capacity = Arc::new(TransferCapacity::new(Arc::new(
        GlobalTransferCapacity::new(),
    )));
    let (sender, receiver) = mailbox::channel();
    let sender = Arc::new(sender);
    let (measurements, _measurement_receiver) = MeasurementRecorder::channel(None, peer);
    let mut worker = OutboundWorker::new(
        receiver,
        StopSource::new(),
        measurements,
        peer,
        SharedAnnouncedSessions::new(),
    );
    // Hold the lane head in the delivery state throughout both cancellation scans.
    let (head, _head_completion) = scheduled_transfer(&node, peer, &capacity, &StopSource::new());
    worker.enqueue_transfer(head);
    let waiting = worker.ready.pop().expect("head is initially runnable");
    worker.ready.wait_for_delivery(1, waiting);
    // The first cancellation publishes completion after remove_ready_where finishes.
    let first_stop = StopSource::new();
    let (first, first_completion) = scheduled_transfer(&node, peer, &capacity, &first_stop);
    worker.enqueue_transfer(first);
    let successor_stop = StopSource::new();
    let (successor, successor_completion) =
        scheduled_transfer(&node, peer, &capacity, &successor_stop);
    worker.enqueue_transfer(successor);
    assert_eq!(capacity.admitted(), 3);
    // Stop after the production scan but before its completion effects. This
    // explicit reducer/effect boundary is executable on both platforms without
    // requiring browser connection handles to implement Send for an ArcWake.
    first_stop.request_stop();
    let results = worker.apply_command(OutboundCommand::CancelStopped);
    assert_eq!(capacity.admitted(), 2);
    successor_stop.request_stop();
    assert!(sender
        .send_coalesced(OutboundCommand::CancelStopped)
        .is_ok());
    worker.publish_command_results(results);
    assert!(matches!(
        first_completion.now_or_never(),
        Some(Ok(Ok(SendCompletionOutcome::Cancelled)))
    ));
    // The next normal drain must consume the later command and reclaim its permit.
    worker.drain_available();
    assert_eq!(
        capacity.admitted(),
        1,
        "only the blocked head retains capacity"
    );
    assert!(matches!(
        successor_completion.now_or_never(),
        Some(Ok(Ok(SendCompletionOutcome::Cancelled)))
    ));
    assert!(
        worker
            .ready
            .take_waiting(TransferClass::Application, 1)
            .is_some(),
        "head delivery is still pending"
    );
}

/// Observe real admission permits at the instant a tracked result is published.
struct ReleasedBeforeWake {
    /// Independent accountant shared by every transfer in this fixture.
    capacity: Arc<TransferCapacity>,
}

impl ArcWake for ReleasedBeforeWake {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        assert_eq!(
            arc_self.capacity.admitted(),
            0,
            "shutdown publishes after all permits release"
        );
    }
}

/// The real submission predicate handles cancel-before-submit on both targets;
/// shutdown drains a waiting head, queued work, and a detached ingress snapshot
/// before publishing even the first cancelled submission's completion.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
async fn stopped_snapshot_releases_all_owners_before_completion() {
    let (node, remote) = connected_swarms().await;
    let peer = remote.did();
    let capacity = Arc::new(TransferCapacity::new(Arc::new(
        GlobalTransferCapacity::new(),
    )));
    let (sender, receiver) = mailbox::channel();
    let stop = StopSource::new();
    let handle = OutboundPeerHandle {
        state: Arc::new(OutboundPeerState {
            #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
            peer,
            sender,
            link: PeerLinkState::new(),
            _capacity_anchor: TransferCapacityAnchor::new(Arc::clone(&capacity)),
            stop: stop.clone(),
        }),
    };
    let (measurements, _receiver) = MeasurementRecorder::channel(None, peer);
    let mut worker = OutboundWorker::new(
        receiver,
        stop,
        measurements,
        peer,
        SharedAnnouncedSessions::new(),
    );
    let cancelled = StopSource::new();
    let (transfer, completion) = tracked_transfer(&node, peer, &cancelled);
    let permit = capacity
        .try_acquire(peer, TransferClass::Application, 1)
        .expect("fixture fits capacity");
    cancelled.request_stop();
    handle.cancel_stopped();
    assert!(handle.submit(transfer, permit).is_ok());
    assert_eq!(capacity.admitted(), 0);
    assert!(matches!(
        completion.now_or_never(),
        Some(Ok(Ok(SendCompletionOutcome::Cancelled)))
    ));
    let mut completions = Vec::new();
    for index in 0..4 {
        let (mut scheduled, completion) =
            scheduled_transfer(&node, peer, &capacity, &StopSource::new());
        scheduled.transfer.bind_scheduler_stop(worker.stop.token());
        if index < 2 {
            worker.enqueue_transfer(scheduled);
        } else {
            assert!(handle
                .state
                .sender
                .send_if(OutboundCommand::Submit(Box::new(scheduled)), |_| true)
                .is_ok());
        }
        completions.push(completion);
    }
    let waiting = worker.ready.pop().expect("head runnable");
    worker.ready.wait_for_delivery(1, waiting);
    let wake = futures::task::waker(Arc::new(ReleasedBeforeWake {
        capacity: Arc::clone(&capacity),
    }));
    let mut context = Context::from_waker(&wake);
    for completion in &mut completions {
        assert!(Pin::new(completion).poll(&mut context).is_pending());
    }
    handle.shutdown();
    worker.drain_available();
    assert_eq!(capacity.admitted(), 0);
    for completion in completions {
        assert!(matches!(
            completion.now_or_never(),
            Some(Ok(Ok(SendCompletionOutcome::Cancelled)))
        ));
    }
}
