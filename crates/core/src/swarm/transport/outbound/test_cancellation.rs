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

/// Reuse one admitted pair and worker for cancellation-before-submit, successive
/// scans behind a waiting head, and shutdown across queued/buffered ownership.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
async fn test_cancellation_after_scan_releases_successor_behind_waiting_head() {
    let (node, remote) = connected_swarms().await;
    let peer = remote.did();
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

/// Regression (#860 review, lemma (P)): a worker dropped (panic, runtime cancellation) while a
/// detached transfer's first frame is claimed publishes `Cancelled`, and the caller boundary
/// turns it into the ambiguous `DetachedSendAbandonedAfterClaim`; an unclaimed transfer stays
/// `Cancelled`, a pre-acceptance deferral.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
async fn test_worker_drop_after_a_claim_is_ambiguous_to_the_detached_caller() {
    let (node, remote) = connected_swarms().await;
    let peer = remote.did();
    let capacity = Arc::new(TransferCapacity::new(Arc::new(
        GlobalTransferCapacity::new(),
    )));
    for claimed in [false, true] {
        let (_sender, receiver) = mailbox::channel();
        let (measurements, _measurement_receiver) = MeasurementRecorder::channel(None, peer);
        let mut worker = OutboundWorker::new(
            receiver,
            StopSource::new(),
            measurements,
            peer,
            SharedAnnouncedDelegations::new(),
        );
        let admission = DetachedAdmission::new();
        let admitted = node
            .transport
            .admitted_send_connection(peer)
            .expect("connection registry is readable")
            .expect("peer is admitted");
        let payload = MessagePayload::new_send(
            Message::custom(b"drop-after-claim").expect("fixture message is valid"),
            node.transport.message_signer(),
            peer,
            peer,
        )
        .expect("fixture payload signs");
        let (transfer, completion) = OutboundTransfer::whole(
            OutboundTransferRoute::new(
                TransferClass::Application,
                peer,
                admitted,
                ChunkSendPermit::Always,
            ),
            payload,
            11,
            OutboundCompletion::Detached,
            admission.stop_token(),
            Some(admission.clone()),
        );
        let permit = capacity
            .try_acquire(peer, TransferClass::Application, 1)
            .expect("fixture fits capacity");
        worker.enqueue_transfer(ScheduledTransfer::new(transfer, permit));
        worker.active = worker.ready.pop();
        if claimed {
            // The backend's final send admission claimed the first frame (`Irrevocable`).
            assert!(admission.try_mark_irrevocable().is_some());
        }
        drop(worker);
        let published = completion
            .now_or_never()
            .expect("drop publishes")
            .expect("sender published");
        assert!(matches!(published, Ok(SendCompletionOutcome::Cancelled)));
        let observed = admission.cancelled_outcome(peer);
        if claimed {
            assert!(matches!(
                observed,
                Err(Error::DetachedSendAbandonedAfterClaim { peer: abandoned }) if abandoned == peer
            ));
        } else {
            assert!(matches!(observed, Ok(SendCompletionOutcome::Cancelled)));
        }
    }
}
