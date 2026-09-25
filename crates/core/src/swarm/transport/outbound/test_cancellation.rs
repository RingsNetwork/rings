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

/// Native setup adapter: a dummy-transport test swarm; no connection is established.
#[cfg(not(target_family = "wasm"))]
async fn test_swarm() -> Arc<Swarm> {
    crate::tests::default::prepare_node(SecretKey::random())
        .await
        .swarm
}

/// Browser setup adapter: a browser test swarm; no connection is established.
#[cfg(target_family = "wasm")]
async fn test_swarm() -> Arc<Swarm> {
    crate::tests::wasm::prepare_node(SecretKey::random()).await
}

/// Admit `peer` on `node` without a link, so the worker has an admitted generation to route to.
///
/// ```text
/// reserve(peer)            ⊢ Pending(a)
/// new_pending_connection   ⊢ raw(peer) exists              (local object; no offer, no ICE)
/// activate_for_test(a)     ⊢ Pending(a) → Admitting(a) → Active(a)
/// ⟹ admitted_send_connection(peer) = Some(token(a))
/// ```
///
/// Every transfer in this test is cancelled before any frame is sent, so the token is never
/// used for I/O and no network is needed on either platform. Each step completes before the
/// next begins, so nothing is awaited on a transport and there is no admission to poll for.
async fn admit_detached_peer(node: &Swarm, peer: Did) {
    let attempt = node
        .transport
        .reserve_pending_connection(peer)
        .await
        .expect("fixture reserves a pending generation");
    let callback = node
        .inner_callback()
        .expect("fixture callback is installed")
        .with_pending_connection_attempt(attempt);
    node.transport
        .new_pending_connection(attempt, callback)
        .await
        .expect("fixture creates the local transport object");
    assert!(
        node.transport
            .activate_connection_for_test(attempt)
            .expect("lifecycle registry is writable"),
        "the reserved generation becomes active"
    );
}

/// Build a tracked transfer with its real capacity permit for the shared contracts.
fn scheduled_transfer(
    node: &Swarm,
    peer: Did,
    capacity: &Arc<TransferCapacity>,
    stop: &StopSource,
) -> (
    ScheduledTransfer,
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
    let permit = capacity
        .try_acquire(peer, TransferClass::Application, 1)
        .expect("fixture fits capacity");
    (ScheduledTransfer::new(transfer, permit), completion)
}

/// Assert release-before-publication at the actual completion wake boundary.
struct ReleasedBeforeWake(Arc<TransferCapacity>);
impl ArcWake for ReleasedBeforeWake {
    fn wake_by_ref(this: &Arc<Self>) {
        assert_eq!(
            this.0.admitted(),
            0,
            "shutdown must release all permits first"
        );
    }
}

/// Reuse one admitted generation and worker for cancellation-before-submit, successive
/// scans behind a waiting head, and shutdown across queued/buffered ownership.
///
/// The generation is minted by [`admit_detached_peer`] without a link, so the test awaits
/// nothing on a transport on either platform: every assertion follows a synchronous worker
/// step (`handle_commands`, `drain_available`) or an immediate `now_or_never` poll. The run is
/// a function of the command sequence alone, with no clock and no network, and it cannot hang
/// on a handshake.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
async fn test_cancellation_after_scan_releases_successor_behind_waiting_head() {
    let node = test_swarm().await;
    let peer: Did = SecretKey::random().address().into();
    admit_detached_peer(&node, peer).await;
    let capacity = Arc::new(TransferCapacity::new(Arc::new(
        GlobalTransferCapacity::new(),
    )));
    let (sender, receiver) = mailbox::channel();
    let (measurements, _measurement_receiver) = MeasurementRecorder::channel(None, peer);
    let mut worker = OutboundWorker::new(
        receiver,
        StopSource::new(),
        measurements,
        peer,
        SharedAnnouncedDelegations::new(),
    );
    let (head, head_completion) = scheduled_transfer(&node, peer, &capacity, &StopSource::new());
    worker.enqueue_transfer(head);
    let waiting = worker.ready.pop().expect("head runnable");
    worker.ready.wait_for_delivery(1, waiting);
    // Each later cancellation must reclaim its successor without completing the head.
    for _ in 0..2 {
        let stop = StopSource::new();
        let (transfer, completion) = scheduled_transfer(&node, peer, &capacity, &stop);
        worker.enqueue_transfer(transfer);
        worker.handle_commands([OutboundCommand::CancelStopped]);
        assert_eq!(
            capacity.admitted(),
            2,
            "live successor survives the first scan"
        );
        stop.request_stop();
        sender
            .send_coalesced(OutboundCommand::CancelStopped)
            .expect("ingress open");
        worker.drain_available();
        assert_eq!(capacity.admitted(), 1, "only waiting head retains capacity");
        assert!(matches!(
            completion.now_or_never(),
            Some(Ok(Ok(SendCompletionOutcome::Cancelled)))
        ));
    }
    // The actor rechecks stop even when the scan preceded its Submit command.
    let stop = StopSource::new();
    let (transfer, completion) = scheduled_transfer(&node, peer, &capacity, &stop);
    stop.request_stop();
    worker.handle_commands([
        OutboundCommand::CancelStopped,
        OutboundCommand::Submit(Box::new(transfer)),
    ]);
    assert_eq!(capacity.admitted(), 1);
    assert!(matches!(
        completion.now_or_never(),
        Some(Ok(Ok(SendCompletionOutcome::Cancelled)))
    ));
    // Reuse the same blocked head with a queued successor and two buffered submits.
    let mut completions = vec![head_completion];
    for index in 0..3 {
        let (mut transfer, completion) =
            scheduled_transfer(&node, peer, &capacity, &StopSource::new());
        transfer.transfer.bind_scheduler_stop(worker.stop.token());
        if index == 0 {
            worker.enqueue_transfer(transfer);
        } else {
            assert!(sender
                .send_if(OutboundCommand::Submit(Box::new(transfer)), |_| true)
                .is_ok());
        }
        completions.push(completion);
    }
    let wake = futures::task::waker(Arc::new(ReleasedBeforeWake(Arc::clone(&capacity))));
    let mut context = Context::from_waker(&wake);
    for completion in &mut completions {
        assert!(Pin::new(completion).poll(&mut context).is_pending());
    }
    worker.stop.request_stop();
    sender.close();
    worker.drain_available();
    for completion in completions {
        assert!(matches!(
            completion.now_or_never(),
            Some(Ok(Ok(SendCompletionOutcome::Cancelled)))
        ));
    }
}
