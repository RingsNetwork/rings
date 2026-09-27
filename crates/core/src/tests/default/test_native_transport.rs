use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::sync::mpsc;
use tokio::sync::watch;
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
use crate::error::CallbackError;
use crate::error::Error;
use crate::error::Result;
use crate::message::test_probe_request;
use crate::message::Message;
use crate::message::MessageCategory;
use crate::message::MessagePayload;
use crate::message::SyncEntriesWithSuccessor;
use crate::message::TRANSACTION_REPLAY_WINDOW;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::callback::SwarmEvent;
use crate::tests::activity::ActivityCallback;
use crate::tests::assert_control_interleaves_transfer;
use crate::tests::control_interleaves_transfer;
use crate::tests::data_transfer_progressed;
use crate::tests::frame_count;
use crate::tests::manually_establish_connection;
use crate::tests::multi_frame_storage_sync_entries;

/// Join `node1` and `node2` over loopback WebRTC and wait until both are connected and past
/// their first exchange. A connection captures its node's callback when it is created, so a
/// test that replaces a callback does so before this join.
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
        .send_storage_sync(storage)
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

/// A receiving application whose `on_validate` holds every Application-class message until
/// released, and records every validated message, in validation order.
///
/// The core dispatches a frame and awaits its validation before the transport reads the next
/// frame of the same data channel, so the held validation stalls exactly the data channel that
/// carried it: the handler stall of the lane-pinning law.
struct StalledApplicationCallback {
    /// Released once the test has seen the other classes proceed.
    release: watch::Receiver<bool>,
    /// Application messages that have entered `on_validate`.
    stalled: AtomicUsize,
    /// Every validated message, in validation order.
    validated: mpsc::UnboundedSender<MessagePayload>,
    /// Activity recording, so event-woken waits observe this node.
    activity: ActivityCallback,
}

#[async_trait]
impl SwarmCallback for StalledApplicationCallback {
    async fn on_validate(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), CallbackError> {
        if matches!(
            payload.transaction.data::<Message>()?,
            Message::CustomMessage(_)
        ) {
            self.stalled.fetch_add(1, Ordering::SeqCst);
            self.activity.on_validate(payload).await?;
            let mut release = self.release.clone();
            release
                .wait_for(|released| *released)
                .await
                .map_err(|_| Error::InvalidMessage("the release gate closed".to_string()))?;
        } else {
            self.activity.on_validate(payload).await?;
        }
        self.validated
            .send(payload.clone())
            .map_err(|_| Error::InvalidMessage("the recorder closed".to_string()))?;
        Ok(())
    }

    async fn on_inbound(&self, payload: &MessagePayload) -> std::result::Result<(), CallbackError> {
        self.activity.on_inbound(payload).await
    }

    async fn on_event(&self, event: &SwarmEvent) -> std::result::Result<(), CallbackError> {
        self.activity.on_event(event).await
    }

    /// A streaming namespace paced per neighbour: the backlog, over the per-origin message
    /// burst, is admitted by the byte floor alone.
    fn delegates_admission(&self, _application_payload: &[u8]) -> bool {
        true
    }
}

/// The index an Application message of this test carries, if `payload` is one.
fn backlog_index(payload: &MessagePayload) -> Result<Option<usize>> {
    let Message::CustomMessage(message) = payload.transaction.data::<Message>()? else {
        return Ok(None);
    };
    let index = std::str::from_utf8(message.0.as_slice())
        .ok()
        .and_then(|text| text.strip_prefix("backlog-"))
        .and_then(|index| index.parse().ok());
    Ok(index)
}

/// Lane pinning (#906): with the receiver's Application handler stalled on its data channel,
/// control keeps flowing on its own channel, and the held Application backlog, twice the replay
/// window, arrives afterwards in send order with no replay rejection.
///
/// Under a rotating channel choice the backlog would spread over every channel: the unstalled
/// channels would admit later sequences past the stalled one, and more than a replay window of
/// them makes the held frames `TransactionSequenceStale` once released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_a_stalled_class_channel_holds_only_its_class_and_never_goes_stale() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    let (release_tx, release) = watch::channel(false);
    let (validated, mut recorded) = mpsc::unbounded_channel();
    let callback = Arc::new(StalledApplicationCallback {
        release,
        stalled: AtomicUsize::new(0),
        validated,
        activity: ActivityCallback,
    });
    node2.swarm.set_callback(callback.clone())?;
    connect_native_nodes(&node1, &node2).await?;
    let peer = node2.did();

    let backlog = 2 * TRANSACTION_REPLAY_WINDOW;
    for index in 0..backlog {
        let message = Message::custom(format!("backlog-{index}").as_bytes())?;
        node1.swarm.send_direct_message(message, peer).await?;
    }
    wait_until_result("the receiver's Application handler stalls", || {
        Ok(callback.stalled.load(Ordering::SeqCst) >= 1)
    })
    .await?;

    let probes: u8 = 4;
    for nonce in 0..probes {
        node1
            .swarm
            .send_direct_message(Message::ProbeRequest(test_probe_request(nonce)), peer)
            .await?;
    }
    let mut controls = 0;
    while controls < probes {
        let payload = timeout(TEST_HANG_GUARD, recorded.recv())
            .await
            .map_err(|_| Error::InvalidMessage("control did not pass the stall".to_string()))?
            .ok_or_else(|| Error::InvalidMessage("the recorder closed".to_string()))?;
        controls += u8::from(matches!(
            payload.transaction.data::<Message>()?,
            Message::ProbeRequest(_)
        ));
    }
    // Control crossed while the Application channel stayed held at its first frame.
    assert_eq!(callback.stalled.load(Ordering::SeqCst), 1);

    release_tx
        .send(true)
        .map_err(|_| Error::InvalidMessage("the release gate closed".to_string()))?;
    let mut arrived = Vec::with_capacity(backlog);
    while arrived.len() < backlog {
        let payload = timeout(TEST_HANG_GUARD, recorded.recv())
            .await
            .map_err(|_| Error::InvalidMessage("the held backlog did not arrive".to_string()))?
            .ok_or_else(|| Error::InvalidMessage("the recorder closed".to_string()))?;
        arrived.extend(backlog_index(&payload)?);
    }
    assert_eq!(arrived, (0..backlog).collect::<Vec<_>>());
    assert_eq!(node2.swarm.transaction_replay_counters().stale, 0);
    Ok(())
}
