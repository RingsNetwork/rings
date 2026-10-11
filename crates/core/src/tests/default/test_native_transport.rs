use rings_transport::core::transport::WebrtcConnectionState;
use tokio::time::timeout;

use super::prepare_node;
use super::wait_for_connection_state;
use super::wait_for_msgs;
use super::wait_until_result;
use super::Node;
use super::TEST_HANG_GUARD;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::test_probe_request;
use crate::message::CustomMessage;
use crate::message::Message;
use crate::message::MessageCategory;
use crate::message::MessagePayload;
use crate::message::SyncEntriesWithSuccessor;
use crate::message::TRANSACTION_REPLAY_WINDOW;
use crate::swarm::application::ApplicationData;
use crate::swarm::application::ApplicationDelivery;
use crate::swarm::application::ApplicationMessage;
use crate::swarm::application::APPLICATION_INBOX_WEIGHT;
use crate::swarm::transport::StorageSyncSend;
use crate::tests::assert_control_interleaves_transfer;
use crate::tests::control_interleaves_transfer;
use crate::tests::data_transfer_progressed;
use crate::tests::frame_count;
use crate::tests::manually_establish_connection;
use crate::tests::multi_frame_storage_sync_entries;

/// Join `node1` and `node2` over loopback WebRTC and wait until both are connected and past
/// their first exchange.
async fn connect_native_nodes(node1: &Node, node2: &Node) -> Result<()> {
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_connection_state(node2, node1.did(), WebrtcConnectionState::Connected).await?;
    wait_for_msgs([node1, node2]).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_native_webrtc_control_interleaves_the_shared_multiframe_storage_fixture() -> Result<()>
{
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    connect_native_nodes(&node1, &node2).await?;

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
        .send_storage_sync_or_defer(storage, StorageSyncSend::Admitted, "test")
        .await?
        .is_sent());

    for round in 0..16 {
        // The next control is sent only once this one is traced and a storage frame follows
        // it, so consecutive controls always have a storage frame between them. The wait is on
        // the transfer's progress; the control's own activity does not satisfy it.
        let trace = node1
            .swarm
            .transport
            .outbound_frame_trace_for_test(node2.did());
        if control_interleaves_transfer(&trace, MessageCategory::Storage) {
            break;
        }
        let controls_before = frame_count(&trace, MessageCategory::DhtControl);
        node1
            .swarm
            .send_direct_message(
                Message::ProbeRequest(test_probe_request(round)),
                node2.did(),
            )
            .await?;
        wait_until_result(
            &format!("the storage transfer progresses after control {round}"),
            || {
                Ok(data_transfer_progressed(
                    &node1
                        .swarm
                        .transport
                        .outbound_frame_trace_for_test(node2.did()),
                    MessageCategory::Storage,
                    controls_before,
                    node1
                        .swarm
                        .transport
                        .outbound_admitted_transfer_total_for_test(),
                ))
            },
        )
        .await?;
    }
    wait_until_result("control interleaves the storage transfer", || {
        Ok(control_interleaves_transfer(
            &node1
                .swarm
                .transport
                .outbound_frame_trace_for_test(node2.did()),
            MessageCategory::Storage,
        ))
    })
    .await?;
    let trace = node1
        .swarm
        .transport
        .take_outbound_frame_trace_for_test(node2.did());
    assert_control_interleaves_transfer(&trace, MessageCategory::Storage);
    Ok(())
}

/// The leading bytes of every Application message this test sends, registered as a delegating
/// namespace: a streaming namespace paced per neighbour, whose backlog, over the per-origin
/// message burst, is admitted by the byte floor alone.
const BACKLOG_PREFIX: &[u8] = b"backlog-";

/// The index an Application payload of this test carries, if `bytes` is one.
fn backlog_index_of(bytes: &[u8]) -> Option<usize> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.strip_prefix("backlog-"))
        .and_then(|index| index.parse().ok())
}

/// The index an Application message of this test carries, if `payload` is one.
fn backlog_index(payload: &MessagePayload) -> Result<Option<usize>> {
    let Message::CustomMessage(message) = payload.transaction.data::<Message>()? else {
        return Ok(None);
    };
    Ok(backlog_index_of(message.0.as_slice()))
}

/// The index a delivery read from the application inbox carries, if it is a backlog message.
fn delivered_backlog_index(delivery: &ApplicationDelivery) -> Option<usize> {
    match delivery {
        ApplicationDelivery::Message(ApplicationMessage {
            data: ApplicationData::Custom(CustomMessage(bytes)),
            ..
        }) => backlog_index_of(bytes),
        _ => None,
    }
}

/// Stall `node`'s Application lane as an application that never reads would: queue one message
/// of the whole inbox weight into its claimed inbox, so the next Application message entering
/// the lane waits for room until the inbox is read.
fn fill_application_inbox(node: &Node) -> Result<()> {
    let filler = ApplicationMessage {
        tx_id: uuid::Uuid::new_v4(),
        from: node.did(),
        data: ApplicationData::Custom(CustomMessage(vec![0; APPLICATION_INBOX_WEIGHT])),
    };
    let room = node
        .swarm
        .transport
        .application()
        .outlet
        .try_reserve(&filler)
        .ok_or_else(|| Error::InvalidMessage("an empty inbox has no room".to_string()))?;
    room.fill(filler);
    Ok(())
}

/// The next payload entering `node`'s logical inbound stage, within the hang guard.
async fn next_inbound(node: &Node, waiting_for: &str) -> Result<MessagePayload> {
    timeout(TEST_HANG_GUARD, node.listen_once())
        .await
        .map_err(|_| Error::InvalidMessage(format!("{waiting_for} did not happen")))?
        .ok_or_else(|| Error::InvalidMessage("the inbound tap closed".to_string()))
}

/// Lane pinning (#906): with the receiver's application not reading its full inbox, the
/// Application lane waits for room on its data channel, control keeps flowing on its own
/// channel, and the held Application backlog, twice the replay window, arrives in the inbox
/// afterwards in send order with no replay rejection.
///
/// The stalled application is a claimed inbox nobody reads, filled to its weight: core never
/// waits on application code (#935), so the only way an application holds a lane is by leaving
/// its inbox full, and that holds only the Application lane.
///
/// Under per-lane credit (#924) the stalled lane holds back its sender once the receiver holds
/// a window of its frames, so the backlog is sent in order by a task of its own, whose sends
/// complete only after the release; control, on its own lane with its own credit, is not held
/// back.
///
/// The control and order checks are what discriminate. Spreading Application frames over every
/// channel puts some on the control channel, so control stalls behind the held lane; spreading
/// them over the other three channels delivers the backlog out of send order. The stale count
/// stays zero either way: the receiver's single Application actor holds each channel after one
/// frame, so no more than a window of frames can pass the stalled one, and the `stale == 0`
/// check guards the law rather than detecting its violation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_a_stalled_class_channel_holds_only_its_class_and_keeps_its_order() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    let inbox = node2.swarm.application_inbox()?;
    node2
        .swarm
        .set_admission_delegation(BACKLOG_PREFIX.to_vec(), true);
    fill_application_inbox(&node2)?;
    connect_native_nodes(&node1, &node2).await?;
    let peer = node2.did();

    let backlog = 2 * TRANSACTION_REPLAY_WINDOW;
    let sender = node1.swarm.clone();
    let backlog_sends = tokio::spawn(async move {
        for index in 0..backlog {
            let message = Message::custom(format!("backlog-{index}").as_bytes())?;
            sender.send_direct_message(message, peer).await?;
        }
        Ok::<(), Error>(())
    });
    // The first backlog message enters the lane, which then waits for inbox room.
    let mut entered = 0;
    while entered == 0 {
        let payload = next_inbound(&node2, "the receiver's Application lane stall").await?;
        entered += usize::from(backlog_index(&payload)?.is_some());
    }

    let probes: u8 = 4;
    for nonce in 0..probes {
        node1
            .swarm
            .send_direct_message(Message::ProbeRequest(test_probe_request(nonce)), peer)
            .await?;
    }
    let mut controls = 0;
    while controls < probes {
        let payload = next_inbound(&node2, "control passing the stall").await?;
        entered += usize::from(backlog_index(&payload)?.is_some());
        controls += u8::from(matches!(
            payload.transaction.data::<Message>()?,
            Message::ProbeRequest(_)
        ));
    }
    // Control crossed while the Application lane stayed held at its first frame.
    assert_eq!(entered, 1);

    // Reading the inbox releases the lane: the filler, then the backlog in send order.
    let mut arrived = Vec::with_capacity(backlog);
    while arrived.len() < backlog {
        let delivery = timeout(TEST_HANG_GUARD, inbox.next())
            .await
            .map_err(|_| Error::InvalidMessage("the held backlog did not arrive".to_string()))?
            .ok_or_else(|| Error::InvalidMessage("the application inbox closed".to_string()))?;
        arrived.extend(delivered_backlog_index(&delivery));
    }
    assert_eq!(arrived, (0..backlog).collect::<Vec<_>>());
    backlog_sends
        .await
        .map_err(|error| Error::InvalidMessage(format!("backlog task failed: {error}")))??;
    assert_eq!(node2.swarm.transaction_replay_counters().stale, 0);
    Ok(())
}
