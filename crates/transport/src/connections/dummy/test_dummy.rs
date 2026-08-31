use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use super::commit_irrevocable_dispatch;
use super::complete_irrevocable_send;
use super::controlled;
use super::retirement::mark_connection_state_closed_with_observer_for_test;
use super::state::ControlledDeliveryState;
use super::DummyConnection;
use super::DummyConnectionState;
use super::DummySendTarget;
use super::Event;
use super::CONNS;
use crate::callback::inbound_peer_frame_capacity_for_test;
use crate::callback::InboundFrameCapacity;
use crate::callback::InnerTransportCallback;
use crate::core::callback::TransportCallback;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::IrrevocableSendGuard;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;
use crate::error::Error;
use crate::notifier::Notifier;

struct NoopCallback;

#[async_trait]
impl TransportCallback for NoopCallback {}

async fn wait_until(label: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
}

struct CloseCallback {
    peer_closed: Arc<AtomicUsize>,
    data_closed: Arc<AtomicUsize>,
}

#[async_trait]
impl TransportCallback for CloseCallback {
    async fn on_data_channel_close(
        &self,
        _cid: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.data_closed.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    async fn on_peer_connection_state_change(
        &self,
        _cid: &str,
        state: WebrtcConnectionState,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if state == WebrtcConnectionState::Closed {
            self.peer_closed.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }
}

fn guarded_test_permit(
    connection_state: &Arc<Mutex<DummyConnectionState>>,
    permit: SendPermit,
) -> IrrevocableSendGuard<impl FnOnce()> {
    let retirement_state = Arc::clone(connection_state);
    let mut guard = IrrevocableSendGuard::new(permit.acceptance(), move || {
        let mut state = retirement_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.webrtc = WebrtcConnectionState::Closed;
        state.data_channel_open_override = Some(false);
    });
    guard.bind(
        permit
            .try_mark_irrevocable()
            .expect("test send must become irrevocable"),
    );
    guard
}

struct InvalidFrameCallback {
    invalid_frames: Arc<AtomicUsize>,
}

#[async_trait]
impl TransportCallback for InvalidFrameCallback {
    async fn on_invalid_inbound_frame(
        &self,
        _cid: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.invalid_frames.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[test]
fn test_snapshot_generation_witnesses_transient_queue_activity() {
    let mut state = ControlledDeliveryState::new();
    let idle = state.snapshot();

    state.push_back(("peer".to_owned(), Event::DataChannelOpen(None)));
    let queued = state.snapshot();
    assert_eq!(queued.pending(), 1);
    assert_ne!(queued.generation(), idle.generation());

    assert!(state.remove(0).is_some());
    let drained = state.snapshot();
    assert!(drained.is_idle());
    assert_ne!(drained.generation(), idle.generation());
}

#[test]
fn test_sequence_index_survives_arbitrary_removal() {
    let mut state = ControlledDeliveryState::new();
    for _ in 0..3 {
        state.push_back(("peer".to_owned(), Event::DataChannelOpen(None)));
    }

    assert!(state.remove_sequence(1).is_some());
    assert_eq!(
        state
            .inspect_after(Some(0))
            .into_iter()
            .map(|delivery| delivery.sequence())
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(
        state.inspect(0).map(|delivery| delivery.sequence()),
        Some(0)
    );
    assert_eq!(
        state.inspect(1).map(|delivery| delivery.sequence()),
        Some(2)
    );
}

#[test]
fn test_close_waits_for_claimed_dummy_dispatch_commit() {
    let connection_state = Arc::new(Mutex::new(DummyConnectionState {
        webrtc: WebrtcConnectionState::Connected,
        data_channel_open_override: None,
    }));
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let permit = guarded_test_permit(&connection_state, permit);
    let (dispatch_started_sender, dispatch_started_receiver) = std::sync::mpsc::channel();
    let (release_dispatch_sender, release_dispatch_receiver) = std::sync::mpsc::channel();
    let (dispatch_done_sender, dispatch_done_receiver) = std::sync::mpsc::channel();
    let sender = {
        let connection_state = Arc::clone(&connection_state);
        std::thread::spawn(move || {
            let result = commit_irrevocable_dispatch(&connection_state, permit, || {
                dispatch_started_sender
                    .send(())
                    .expect("dispatch observer must remain open");
                release_dispatch_receiver
                    .recv_timeout(Duration::from_secs(1))
                    .expect("dispatch release must arrive");
                Ok(())
            });
            dispatch_done_sender
                .send(result)
                .expect("dispatch completion observer must remain open");
        })
    };
    dispatch_started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("the claimed dispatch must hold the state lock");

    let (close_waiting_sender, close_waiting_receiver) = std::sync::mpsc::channel();
    let (close_done_sender, close_done_receiver) = std::sync::mpsc::channel();
    let closer = {
        let connection_state = Arc::clone(&connection_state);
        std::thread::spawn(move || {
            mark_connection_state_closed_with_observer_for_test(&connection_state, || {
                close_waiting_sender
                    .send(())
                    .expect("close boundary observer must remain open");
            });
            close_done_sender
                .send(())
                .expect("close completion observer must remain open");
        })
    };
    close_waiting_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("close must reach the actual state gate");

    release_dispatch_sender
        .send(())
        .expect("the dispatch must still be waiting");
    dispatch_done_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("dispatch must finish after release")
        .expect("open claimed dispatch must succeed");
    close_done_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("close must finish after dispatch releases the state lock");
    sender.join().expect("dispatch thread must not panic");
    closer.join().expect("close thread must not panic");
    assert!(acceptance.is_accepted());
}

#[tokio::test]
async fn test_queued_dummy_message_retains_raw_frame_capacity() {
    controlled::enable(true);
    let capacity = Arc::new(InboundFrameCapacity::new());
    let callback = InnerTransportCallback::new_for_test(
        "remote-peer",
        Box::new(NoopCallback),
        Notifier::default(),
        capacity.clone(),
    );
    let remote = Arc::new(DummyConnection::new(callback));
    let connection_state = Arc::new(Mutex::new(DummyConnectionState {
        webrtc: WebrtcConnectionState::Connected,
        data_channel_open_override: None,
    }));
    let data = rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(&[1])))
        .map(Bytes::from)
        .expect("dummy transport frame must serialize");

    let _delivery = complete_irrevocable_send(
        &connection_state,
        data,
        DummySendTarget::Deliver(remote),
        guarded_test_permit(&connection_state, SendPermit::always()),
    )
    .expect("first dummy frame must enter the controlled queue");
    assert_eq!(controlled::pending(), 1);

    let permits = (1..inbound_peer_frame_capacity_for_test())
        .map(|_| capacity.try_acquire("remote-peer", 1))
        .collect::<Option<Vec<_>>>()
        .expect("remaining peer data allowance must be available");
    assert!(capacity.try_acquire("remote-peer", 1).is_none());

    controlled::enable(false);
    assert!(capacity.try_acquire("remote-peer", 1).is_some());
    drop(permits);
}

#[tokio::test]
async fn test_receiver_capacity_drop_does_not_fail_irrevocable_dummy_send() {
    controlled::enable(true);
    controlled::reset_sent_count();
    let capacity = Arc::new(InboundFrameCapacity::new());
    let callback = InnerTransportCallback::new_for_test(
        "remote-peer",
        Box::new(NoopCallback),
        Notifier::default(),
        capacity.clone(),
    );
    let remote = Arc::new(DummyConnection::new(callback));
    let connection_state = Arc::new(Mutex::new(DummyConnectionState {
        webrtc: WebrtcConnectionState::Connected,
        data_channel_open_override: None,
    }));
    let permits = (0..inbound_peer_frame_capacity_for_test())
        .map(|_| capacity.try_acquire("remote-peer", 1))
        .collect::<Option<Vec<_>>>()
        .expect("test must fill the receiver's peer data allowance");
    let data = rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(&[1])))
        .map(Bytes::from)
        .expect("dummy transport frame must serialize");
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let permit = guarded_test_permit(&connection_state, permit);

    let _delivery = complete_irrevocable_send(
        &connection_state,
        data,
        DummySendTarget::Deliver(remote),
        permit,
    )
    .expect("receiver-local capacity pressure must not fail the sender");

    assert!(acceptance.is_accepted());
    assert_eq!(controlled::sent_count(), 1);
    assert_eq!(controlled::pending(), 0);
    assert_eq!(
        connection_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .webrtc,
        WebrtcConnectionState::Connected
    );
    controlled::enable(false);
    drop(permits);
}

#[tokio::test]
async fn test_oversized_dummy_frame_is_dropped_before_receiver_dispatch() {
    controlled::enable(true);
    controlled::reset_sent_count();
    let capacity = Arc::new(InboundFrameCapacity::new());
    let invalid_frames = Arc::new(AtomicUsize::new(0));
    let callback = InnerTransportCallback::new_for_test(
        "remote-peer",
        Box::new(InvalidFrameCallback {
            invalid_frames: Arc::clone(&invalid_frames),
        }),
        Notifier::default(),
        capacity,
    );
    let remote = Arc::new(DummyConnection::new(callback));
    CONNS.insert(remote.rand_id.clone(), Arc::clone(&remote));
    let connection_state = Arc::new(Mutex::new(DummyConnectionState {
        webrtc: WebrtcConnectionState::Connected,
        data_channel_open_override: None,
    }));
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();

    let _delivery = complete_irrevocable_send(
        &connection_state,
        Bytes::from(vec![0; MAX_DATA_CHANNEL_MESSAGE_SIZE + 1]),
        DummySendTarget::Deliver(Arc::clone(&remote)),
        guarded_test_permit(&connection_state, permit),
    )
    .expect("the local send may complete while the receiver drops an oversized frame");

    assert!(acceptance.is_accepted());
    assert_eq!(controlled::pending(), 0);
    assert_eq!(controlled::sent_count(), 1);
    wait_until(
        "invalid-frame callback outside the controlled data queue",
        || invalid_frames.load(Ordering::Acquire) != 0,
    )
    .await;
    assert_eq!(invalid_frames.load(Ordering::Acquire), 1);
    CONNS.remove(&remote.rand_id);
    controlled::enable(false);
}

#[tokio::test]
async fn test_pending_dummy_close_fences_an_irrevocable_dispatch_synchronously() {
    controlled::enable(true);
    controlled::reset_sent_count();
    controlled::set_drop_messages(true);
    controlled::pause_irrevocable_send();
    controlled::set_close_pending(true);
    let callback = InnerTransportCallback::new_for_test(
        "retired-peer",
        Box::new(NoopCallback),
        Notifier::default(),
        Arc::new(InboundFrameCapacity::new()),
    );
    let connection = Arc::new(DummyConnection::new(callback));
    connection.force_webrtc_connection_state_without_callback(WebrtcConnectionState::Connected);
    let sender = {
        let connection = Arc::clone(&connection);
        tokio::spawn(async move {
            connection
                .send_message_with_permit(
                    TransportMessage::Custom(Bytes::from_static(b"payload")),
                    SendPermit::always(),
                )
                .await
        })
    };
    wait_until("dummy irrevocable send gate", || {
        controlled::irrevocable_send_gate_waiting()
    })
    .await;

    let closer = {
        let connection = Arc::clone(&connection);
        tokio::spawn(async move { connection.close().await })
    };
    wait_until("dummy retirement state gate", || {
        connection.webrtc_connection_state() == WebrtcConnectionState::Closed
    })
    .await;
    assert!(!closer.is_finished());
    controlled::release_irrevocable_send_gate();

    assert!(matches!(
        sender.await.expect("dummy send task must not panic"),
        Err(Error::DummyConnectionRetiredBeforeDispatch)
    ));
    assert_eq!(controlled::sent_count(), 0);
    closer.abort();
    let _cancelled = closer.await;
    controlled::enable(false);
}

#[tokio::test(flavor = "current_thread")]
async fn test_cancelling_pending_close_completes_physical_retirement() {
    controlled::enable(true);
    controlled::set_close_pending(true);
    let local_peer_closed = Arc::new(AtomicUsize::new(0));
    let local_data_closed = Arc::new(AtomicUsize::new(0));
    let local_callback = InnerTransportCallback::new_for_test(
        "local-peer",
        Box::new(CloseCallback {
            peer_closed: Arc::clone(&local_peer_closed),
            data_closed: Arc::clone(&local_data_closed),
        }),
        Notifier::default(),
        Arc::new(InboundFrameCapacity::new()),
    );
    let local = Arc::new(DummyConnection::new(local_callback));
    let remote_callback = InnerTransportCallback::new_for_test(
        "remote-peer",
        Box::new(NoopCallback),
        Notifier::default(),
        Arc::new(InboundFrameCapacity::new()),
    );
    let remote = Arc::new(DummyConnection::new(remote_callback));
    local.force_webrtc_connection_state_without_callback(WebrtcConnectionState::Connected);
    remote.force_webrtc_connection_state_without_callback(WebrtcConnectionState::Connected);
    local.set_remote_rand_id(remote.rand_id.clone());
    remote.set_remote_rand_id(local.rand_id.clone());
    CONNS.insert(local.rand_id.clone(), Arc::clone(&local));
    CONNS.insert(remote.rand_id.clone(), Arc::clone(&remote));

    let closer = {
        let local = Arc::clone(&local);
        tokio::spawn(async move { local.close().await })
    };
    wait_until("synchronous retirement fence", || {
        local.webrtc_connection_state() == WebrtcConnectionState::Closed
    })
    .await;
    closer.abort();
    let _cancelled = closer.await;

    wait_until("cancelled close callbacks and remote retirement", || {
        remote.webrtc_connection_state() == WebrtcConnectionState::Closed
            && local_peer_closed.load(Ordering::Acquire) == 1
            && local_data_closed.load(Ordering::Acquire) == 1
    })
    .await;
    assert!(CONNS.get(&local.rand_id).is_none());
    assert!(!local.dispatch(Event::DataChannelOpen(None)));
    assert!(local.event_listener.is_finished());

    CONNS.remove(&remote.rand_id);
    controlled::enable(false);
}

#[test]
fn test_irrevocable_retirement_drop_outside_runtime_is_panic_free() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime must build");
    let (connection, guard) = {
        let _entered = runtime.enter();
        let callback = InnerTransportCallback::new_for_test(
            "runtime-peer",
            Box::new(NoopCallback),
            Notifier::default(),
            Arc::new(InboundFrameCapacity::new()),
        );
        let connection = Arc::new(DummyConnection::new(callback));
        connection.force_webrtc_connection_state_without_callback(WebrtcConnectionState::Connected);
        CONNS.insert(connection.rand_id.clone(), Arc::clone(&connection));
        let fence = connection.retirement_fence();
        let permit = SendPermit::always();
        let mut guard = IrrevocableSendGuard::new(permit.acceptance(), move || fence.request());
        guard.bind(
            permit
                .try_mark_irrevocable()
                .expect("test retirement must become irrevocable"),
        );
        (connection, guard)
    };
    drop(runtime);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(guard)));
    assert!(result.is_ok());
    assert_eq!(
        connection.webrtc_connection_state(),
        WebrtcConnectionState::Closed
    );
    assert!(CONNS.get(&connection.rand_id).is_none());
    assert!(!connection.dispatch(Event::DataChannelOpen(None)));
}

#[tokio::test]
async fn test_failed_irrevocable_dummy_dispatch_retires_connection_and_rejects_later_send() {
    let local_callback = InnerTransportCallback::new_for_test(
        "local-peer",
        Box::new(NoopCallback),
        Notifier::default(),
        Arc::new(InboundFrameCapacity::new()),
    );
    let local = Arc::new(DummyConnection::new(local_callback));
    local.force_webrtc_connection_state_without_callback(WebrtcConnectionState::Connected);
    let remote_callback = InnerTransportCallback::new_for_test(
        "remote-peer",
        Box::new(NoopCallback),
        Notifier::default(),
        Arc::new(InboundFrameCapacity::new()),
    );
    let remote = Arc::new(DummyConnection::new(remote_callback));
    local.set_remote_rand_id(remote.rand_id.clone());
    remote.set_remote_rand_id(local.rand_id.clone());
    CONNS.insert(local.rand_id.clone(), Arc::clone(&local));
    CONNS.insert(remote.rand_id.clone(), Arc::clone(&remote));
    remote.event_listener.abort();
    wait_until("aborted remote listener", || {
        remote.event_listener.is_finished()
    })
    .await;
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let result = local
        .send_message_with_permit(
            TransportMessage::Custom(Bytes::from_static(b"payload")),
            permit,
        )
        .await;

    assert!(matches!(result, Err(Error::DummyRemoteConnectionClosed)));
    assert!(acceptance.is_irrevocable());
    assert!(!acceptance.is_accepted());
    assert_eq!(
        local.webrtc_connection_state(),
        WebrtcConnectionState::Closed
    );
    assert!(CONNS.get(&local.rand_id).is_none());
    assert!(remote.remote_conn().is_none());
    assert!(!local.dispatch(Event::DataChannelOpen(None)));
    assert!(matches!(
        local
            .send_message_with_permit(
                TransportMessage::Custom(Bytes::from_static(b"later")),
                SendPermit::always(),
            )
            .await,
        Err(Error::DataChannelOpen(_))
    ));
    CONNS.remove(&remote.rand_id);
}
