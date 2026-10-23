//! Tracked storage sync: it finishes only when every frame stops, its deadline retires a stalled
//! generation, and dropping it stops its transfer.

use rings_transport::connections::dummy_controlled;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::time::Duration;

use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::SyncEntriesWithSuccessor;
use crate::swarm::transport::hold_frame_deadlines_for_test;
use crate::swarm::transport::StorageSyncOutcome;
use crate::tests::default::connected_nodes;
use crate::tests::default::dummy_hooks::MaxMessageSizeGuard;
use crate::tests::default::dummy_hooks::PendingAfterSentCountGuard;
use crate::tests::default::dummy_hooks::PendingCloseGuard;
use crate::tests::default::dummy_hooks::PendingDeliveryGuard;
use crate::tests::default::test_chunk_e2e::large_storage_sync_entries;
use crate::tests::default::test_chunk_e2e::wait_for_test_condition;
use crate::tests::default::TEST_HANG_GUARD;
use crate::tests::outbound_capacity_released;
#[tokio::test]
async fn test_tracked_storage_sync_does_not_finish_while_a_chunk_tail_is_pending() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;

    let _max_size = MaxMessageSizeGuard::new(8192);
    dummy_controlled::reset_sent_count();
    let _pending_after_first_chunk = PendingAfterSentCountGuard::new(1);
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(node2.did()),
        data: large_storage_sync_entries()?,
    };
    let transport = node1.swarm.transport.clone();
    let send = tokio::spawn(async move { transport.send_storage_sync_tracked(msg).await });

    wait_for_test_condition("the tracked path must enter the real chunk tail", || {
        dummy_controlled::sent_count() == 1
    })
    .await;
    assert!(
        !send.is_finished(),
        "tracked storage sync must not report completion after first-chunk admission"
    );

    node1.dht().remove(node2.did())?;
    let send_result = tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("route cancellation should release the tracked chunk tail")
        .expect("tracked storage sync task should not panic");
    assert_eq!(send_result?, StorageSyncOutcome::Deferred);
    assert_eq!(
        dummy_controlled::sent_count(),
        1,
        "route cancellation must prevent any additional chunk admission"
    );
    Ok(())
}

#[tokio::test]
async fn test_tracked_storage_sync_timeout_closes_stalled_delivery_generation() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;

    dummy_controlled::reset_sent_count();
    let pending_delivery = PendingDeliveryGuard::new();
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(node2.did()),
        data: Vec::new(),
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        node1.swarm.transport.send_storage_sync_tracked(msg),
    )
    .await
    .expect("tracked delivery deadline must bound a stuck delivery future")?;

    assert_eq!(outcome, StorageSyncOutcome::Deferred);
    assert_eq!(dummy_controlled::sent_count(), 1);
    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
    assert!(
        outbound_capacity_released(&node1.swarm.transport, node2.did()),
        "tracked cancellation must release capacity before returning"
    );

    drop(pending_delivery);
    assert_eq!(dummy_controlled::sent_count(), 1);
    Ok(())
}

/// The tracked deadline and its cleanup grace retire a generation whose delivery and close
/// never answer.
///
/// The frame deadlines are held, so the frame's delivery stall cannot retire the generation
/// first: the tracked deadline is the only deadline that starts the cleanup, and the typed
/// cleanup timeout is the verdict. Wall time bounds only a hang.
#[tokio::test]
async fn test_tracked_cleanup_grace_terminalizes_a_nonresponsive_generation() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let _frame_deadlines = hold_frame_deadlines_for_test();

    let peer = node2.did();
    let attempt = node1
        .swarm
        .transport
        .active_attempt(peer)?
        .ok_or(Error::ConnectionNotFound)?;
    let connection = node1
        .swarm
        .transport
        .get_connection(peer)
        .ok_or(Error::ConnectionNotFound)?;
    let _pending_delivery = PendingDeliveryGuard::new();
    let _pending_close = PendingCloseGuard::new();
    let payload = MessagePayload::new_send(
        Message::custom(b"tracked-cleanup-grace")?,
        node1.swarm.transport.message_signer(),
        peer,
        peer,
    )?;

    let error = tokio::time::timeout(
        TEST_HANG_GUARD,
        node1
            .swarm
            .transport
            .send_payload_tracked_with_matching_delivery_deadline_for_test(payload),
    )
    .await
    .expect("tracked cleanup grace must end a nonresponsive generation")
    .expect_err("nonresponsive cleanup must return its typed timeout");

    assert!(matches!(
        error,
        Error::TrackedPayloadCleanupTimeout { peer: failed, .. } if failed == peer
    ));
    assert_eq!(node1.swarm.transport.active_attempt(peer)?, None);
    assert!(!node1.swarm.transport.is_send_terminal_attempt(attempt)?);
    assert!(node1.swarm.transport.get_connection(peer).is_none());
    assert_eq!(
        connection.webrtc_connection_state(),
        WebrtcConnectionState::Closed
    );
    wait_for_test_condition(
        "terminal cleanup must release retained transfer capacity",
        || outbound_capacity_released(&node1.swarm.transport, peer),
    )
    .await;
    Ok(())
}

#[tokio::test]
async fn test_dropping_tracked_storage_sync_requests_transfer_stop() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;

    dummy_controlled::reset_sent_count();
    let pending_delivery = PendingDeliveryGuard::new();
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(node2.did()),
        data: Vec::new(),
    };
    let transport = node1.swarm.transport.clone();
    let send = tokio::spawn(async move { transport.send_storage_sync_tracked(msg).await });

    wait_for_test_condition("tracked send must reach the stalled delivery", || {
        dummy_controlled::sent_count() == 1
            && node1
                .swarm
                .transport
                .outbound_admitted_transfer_count_for_test(node2.did())
                == Some(1)
    })
    .await;

    send.abort();
    let _ = send.await;
    wait_for_test_condition(
        "dropping a tracked send must stop and release its transfer",
        || {
            node1.swarm.transport.get_connection(node2.did()).is_none()
                && outbound_capacity_released(&node1.swarm.transport, node2.did())
        },
    )
    .await;

    drop(pending_delivery);
    assert_eq!(dummy_controlled::sent_count(), 1);
    Ok(())
}
