//! Deterministic cancellation interleaving with a blocked same-category head.

use std::task::Context;

use futures::channel::oneshot;
use futures::task::ArcWake;
use rings_transport::core::transport::WebrtcConnectionState;

use super::*;
use crate::ecc::SecretKey;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_connection_state;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;

/// Cancels the successor when the first cancelled transfer publishes completion.
/// Publication happens after the queue scan, fixing the race without timing hooks.
struct CancelSuccessorOnCompletion {
    /// Stop source belonging only to the successor that the scan already inspected.
    stop: StopSource,
    /// Shared mailbox ingress used to notify the worker of that later cancellation.
    sender: Arc<MailboxSender<OutboundCommand>>,
}

impl ArcWake for CancelSuccessorOnCompletion {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.stop.request_stop();
        assert!(arc_self.sender.send(OutboundCommand::CancelStopped).is_ok());
    }
}

/// Build a real tracked transfer and capacity permit without starting its worker.
fn scheduled_transfer(
    node: &Node,
    peer: Did,
    capacity: &Arc<TransferCapacity>,
    stop: &StopSource,
) -> (
    ScheduledTransfer,
    oneshot::Receiver<Result<SendCompletionOutcome>>,
) {
    // The admitted connection is shared by all three transfers in this lane.
    let admitted = node
        .swarm
        .transport
        .admitted_send_connection(peer)
        .expect("connection registry is readable")
        .expect("peer is admitted");
    // Each logical payload retains the real signer and destination semantics.
    let payload = MessagePayload::new_send(
        Message::custom(b"cancel-race").expect("fixture message is valid"),
        node.swarm.transport.message_signer(),
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
    // The permit is the observable capacity reclamation witness.
    let permit = capacity
        .try_acquire(peer, TransferClass::Application, 1)
        .expect("fixture fits the lane capacity");
    (ScheduledTransfer::new(transfer, permit), completion)
}

/// A stop published after the scan reclaims its successor before head delivery.
#[tokio::test]
async fn test_cancellation_after_scan_releases_successor_behind_waiting_head() {
    // Only connection setup uses the runtime; the worker interleaving is synchronous.
    let node = prepare_node(SecretKey::random()).await;
    let remote = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node.swarm, &remote.swarm).await;
    let peer = remote.did();
    wait_for_connection_state(&node, peer, WebrtcConnectionState::Connected)
        .await
        .expect("fixture connection completes");
    wait_for_successor(&node, peer)
        .await
        .expect("fixture peer is admitted");
    wait_for_msgs([&node, &remote]).await;
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
    let (first, mut first_completion) = scheduled_transfer(&node, peer, &capacity, &first_stop);
    worker.enqueue_transfer(first);
    let successor_stop = StopSource::new();
    let (successor, successor_completion) =
        scheduled_transfer(&node, peer, &capacity, &successor_stop);
    worker.enqueue_transfer(successor);
    assert_eq!(capacity.admitted(), 3);
    // Register a synchronous wake that sets the successor's stop and enqueues its command.
    let wake = futures::task::waker(Arc::new(CancelSuccessorOnCompletion {
        stop: successor_stop,
        sender,
    }));
    let mut context = Context::from_waker(&wake);
    assert!(Pin::new(&mut first_completion)
        .poll(&mut context)
        .is_pending());
    first_stop.request_stop();
    worker.handle_command(OutboundCommand::CancelStopped);
    assert_eq!(
        capacity.admitted(),
        2,
        "successor stopped only after the first scan"
    );
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
