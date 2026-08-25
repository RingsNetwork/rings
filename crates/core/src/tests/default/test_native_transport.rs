use std::time::Duration;

use super::prepare_node;
use super::wait_for_connection_state;
use super::wait_for_msgs;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessageClass;
use crate::message::PeerLivenessProbe;
use crate::message::SyncEntriesWithSuccessor;
use crate::tests::assert_control_interleaves_transfer;
use crate::tests::control_interleaves_transfer;
use crate::tests::manually_establish_connection;
use crate::tests::multi_frame_storage_sync_entries;

const TRACE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const TRACE_POLL_ATTEMPTS: usize = 500;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_webrtc_control_interleaves_the_shared_multiframe_storage_fixture() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(
        &node1,
        node2.did(),
        rings_transport::core::transport::WebrtcConnectionState::Connected,
    )
    .await?;
    wait_for_connection_state(
        &node2,
        node1.did(),
        rings_transport::core::transport::WebrtcConnectionState::Connected,
    )
    .await?;
    wait_for_msgs([&node1, &node2]).await;

    node1
        .swarm
        .transport
        .start_outbound_frame_trace_for_test(node2.did());
    let storage = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(node2.did()),
        data: multi_frame_storage_sync_entries()?,
    };
    assert!(node1
        .swarm
        .transport
        .send_storage_sync(storage)
        .await?
        .is_some());

    for round in 0..16 {
        node1
            .swarm
            .send_direct_message(
                Message::PeerLivenessProbe(PeerLivenessProbe {
                    sent_at_ms: i64::from(round),
                }),
                node2.did(),
            )
            .await?;
        if control_interleaves_transfer(
            &node1
                .swarm
                .transport
                .outbound_frame_trace_for_test(node2.did()),
            MessageClass::Storage,
        ) {
            break;
        }
    }

    for _ in 0..TRACE_POLL_ATTEMPTS {
        if control_interleaves_transfer(
            &node1
                .swarm
                .transport
                .outbound_frame_trace_for_test(node2.did()),
            MessageClass::Storage,
        ) {
            break;
        }
        tokio::time::sleep(TRACE_POLL_INTERVAL).await;
    }
    let trace = node1
        .swarm
        .transport
        .take_outbound_frame_trace_for_test(node2.did());
    assert_control_interleaves_transfer(&trace, MessageClass::Storage);
    Ok(())
}
