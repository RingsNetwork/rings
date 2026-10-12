//! The transfer capacity bounds real waiting and queued lifetimes.

use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio::time::Duration;

use super::connected_nodes;
use super::invalid_test_state;
use super::wait_until;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::test_probe_request;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::swarm::application::ApplicationData;
use crate::swarm::transport::OUTBOUND_CONTROL_RESERVED_TRANSFERS;
use crate::swarm::transport::OUTBOUND_DATA_TRANSFER_CAPACITY;
use crate::swarm::transport::OUTBOUND_TRANSFER_QUEUE_CAPACITY;
use crate::tests::default::dummy_hooks::PendingDeliveryGuard;
use crate::tests::default::Node;

fn spawn_data_capacity_transfers(node: &Node, peer: Did) -> Vec<JoinHandle<Result<uuid::Uuid>>> {
    let mut sends = Vec::with_capacity(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
    for index in 0..OUTBOUND_DATA_TRANSFER_CAPACITY {
        let swarm = node.swarm.clone();
        sends.push(tokio::spawn(async move {
            let body = format!("capacity-transfer-{index}");
            let payload = MessagePayload::new_send(
                Message::custom(body.as_bytes())?,
                swarm.transport.message_signer(),
                peer,
                peer,
            )?;
            let tx_id = payload.transaction.tx_id;
            swarm
                .transport
                .send_payload_detached_observing_scheduler_submit_for_test(payload, || {})
                .await
                .map(|_| tx_id)
        }));
    }
    sends
}

async fn assert_data_capacity_is_bounded(node: &Node, peer: Did) -> Result<()> {
    wait_until("non-control transfer capacity", || {
        node.swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(OUTBOUND_DATA_TRANSFER_CAPACITY)
    })
    .await?;

    let data_error = node
        .swarm
        .send_direct_message(ApplicationData::custom(b"over-data-capacity"), peer)
        .await
        .expect_err("non-control work must not consume the control reserve");
    assert!(matches!(
        data_error,
        Error::OutboundTransferCapacityExceeded {
            peer: error_peer,
            capacity: OUTBOUND_DATA_TRANSFER_CAPACITY,
        } if error_peer == peer
    ));
    assert!(data_error.is_deferrable_data_plane_send());
    assert!(!data_error.records_peer_send_failure());
    Ok(())
}

fn spawn_reserved_control_transfers(
    node: &Node,
    peer: Did,
    sends: &mut Vec<JoinHandle<Result<uuid::Uuid>>>,
) {
    for index in 0..OUTBOUND_CONTROL_RESERVED_TRANSFERS {
        let swarm = node.swarm.clone();
        sends.push(tokio::spawn(async move {
            let payload = MessagePayload::new_send(
                Message::ProbeRequest(test_probe_request(u8::try_from(index).unwrap_or(u8::MAX))),
                swarm.transport.message_signer(),
                peer,
                peer,
            )?;
            let tx_id = payload.transaction.tx_id;
            swarm
                .transport
                .send_payload_detached_observing_scheduler_submit_for_test(payload, || {})
                .await
                .map(|_| tx_id)
        }));
    }
}

async fn assert_control_capacity_is_bounded(node: &Node, peer: Did) -> Result<()> {
    wait_until("reserved control transfer capacity", || {
        node.swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(OUTBOUND_DATA_TRANSFER_CAPACITY + OUTBOUND_CONTROL_RESERVED_TRANSFERS)
    })
    .await?;
    let control_error = node
        .swarm
        .send_direct_core_message_for_test(Message::ProbeRequest(test_probe_request(255)), peer)
        .await
        .expect_err("control traffic must preserve the other data-class reserves");
    assert!(matches!(
        control_error,
        Error::OutboundTransferCapacityExceeded {
            peer: error_peer,
            ..
        } if error_peer == peer
    ));
    Ok(())
}

async fn disconnect_and_join_capacity_transfers(
    node: &Node,
    peer: Did,
    sends: Vec<JoinHandle<Result<uuid::Uuid>>>,
) -> Result<()> {
    node.swarm.transport.disconnect(peer).await?;
    for send in sends {
        let result = timeout(Duration::from_secs(2), send)
            .await
            .map_err(|_| invalid_test_state("capacity transfer did not stop on disconnect"))?
            .map_err(|error| invalid_test_state(format!("capacity task failed: {error}")))?;
        if let Err(error) = result {
            assert!(matches!(
                error,
                Error::OutboundTransferCapacityExceeded { .. }
                    | Error::OutboundTransferMemoryCapacityExceeded { .. }
                    | Error::OutboundTransferAdmissionTimeout { .. }
                    | Error::OutboundFirstFrameAdmissionTimeout { .. }
                    | Error::ConnectionAttemptSuperseded { .. }
                    | Error::ChannelSendMessageFailed
            ));
        }
    }
    wait_until("retired scheduler capacity release", || {
        node.swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            .is_none()
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn test_transfer_capacity_bounds_real_waiting_and_queued_lifetimes() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _pending_delivery = PendingDeliveryGuard::new();
    let mut sends = spawn_data_capacity_transfers(&node1, peer);

    assert_data_capacity_is_bounded(&node1, peer).await?;
    spawn_reserved_control_transfers(&node1, peer, &mut sends);
    assert_control_capacity_is_bounded(&node1, peer).await?;
    disconnect_and_join_capacity_transfers(&node1, peer, sends).await
}
