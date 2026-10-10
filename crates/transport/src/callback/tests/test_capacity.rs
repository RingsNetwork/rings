//! Receive-side admission against per-lane credit, and data-channel admission by label.

use std::sync::atomic::AtomicU8;

use bytes::Bytes;

use super::*;
use crate::core::credit::LANE_CREDIT_WINDOW;
use crate::core::pool::ChannelLane;
use crate::core::pool::DATA_CHANNEL_POOL_SIZE;
use crate::core::transport::TransportMessage;

/// A transport callback that ignores every event, for admission tests that never dispatch.
struct IgnoredCallback;

#[cfg_attr(target_family = "wasm", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait::async_trait)]
impl crate::core::callback::TransportCallback for IgnoredCallback {}

/// The wire form of `message`.
fn wire(message: &TransportMessage) -> Bytes {
    Bytes::from(rings_codec::serialize(message).expect("transport frame must serialize"))
}

/// A callback for `peer` with the production credit window.
fn callback(peer: &str) -> InnerTransportCallback {
    InnerTransportCallback::new(
        peer,
        Box::new(IgnoredCallback),
        crate::notifier::Notifier::default(),
        NodeReceiveLoad::new(),
    )
}

/// Soft bound (#913 R7 M3): while the node's connections hold at least its soft limit, a lane
/// whose releases complete a batch defers its advertisement; the release, on any connection,
/// that brings the load below the limit makes it.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
#[test]
fn test_a_loaded_node_defers_credit_until_any_release_relieves_it() {
    use futures::FutureExt;

    use crate::core::credit::credit_index;

    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    let frame = u64::try_from(custom.len()).expect("a frame length fits u64");
    // The production batch: half a window.
    let batch = LANE_CREDIT_WINDOW / 2;
    // The limit sits where one connection's batch of releases leaves the node.
    let load = NodeReceiveLoad::with_limit((2 * LANE_CREDIT_WINDOW - batch) * frame);
    let open = |peer| {
        InnerTransportCallback::new(
            peer,
            Box::new(IgnoredCallback),
            crate::notifier::Notifier::default(),
            load.clone(),
        )
    };
    let (deferred, relieving) = (open("deferred"), open("relieving"));
    let lane = ChannelLane::new(1);
    let fill = |callback: &InnerTransportCallback| {
        (0..LANE_CREDIT_WINDOW)
            .map(
                |_| match callback.admit_inbound_frame(custom.clone(), lane) {
                    InboundFrameAdmission::Admitted(frame) => frame,
                    _ => panic!("a frame within the window must be admitted"),
                },
            )
            .collect::<Vec<_>>()
    };
    let credit = |callback: &InnerTransportCallback| {
        callback
            .link_credit()
            .next_credit(credit_index(lane))
            .now_or_never()
            .flatten()
    };
    let (mut held, mut other) = (fill(&deferred), fill(&relieving));
    assert_eq!(load.held(), 2 * LANE_CREDIT_WINDOW * frame);

    // A batch released with the node still at its limit: deferred.
    held.truncate(held.len() - usize::try_from(batch).expect("the batch fits usize"));
    assert_eq!(
        credit(&deferred),
        None,
        "a loaded node defers the advertisement"
    );
    // A release on the other connection brings the node below the limit: the deferred credit
    // is advertised, while the relieving lane, whose batch is not complete, advertises nothing.
    other.pop();
    assert_eq!(credit(&deferred), Some(batch + LANE_CREDIT_WINDOW));
    assert_eq!(credit(&relieving), None);
    drop((held, other));
    assert_eq!(load.held(), 0);
}

/// The priority lane is never deferred (#913 R8 M3): while the node holds its soft limit, a
/// batch released on [`ChannelLane::PRIORITY`] is advertised at once, and a batch released on any
/// other lane is deferred, so a peer's control traffic never waits for what other traffic makes
/// the node hold.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
#[test]
fn test_a_loaded_node_still_advertises_the_priority_lane() {
    use futures::FutureExt;

    use crate::core::credit::credit_index;

    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    let frame = u64::try_from(custom.len()).expect("a frame length fits u64");
    let batch = usize::try_from(LANE_CREDIT_WINDOW / 2).expect("the batch fits usize");
    // One full window loads the node.
    let load = NodeReceiveLoad::with_limit(LANE_CREDIT_WINDOW * frame);
    let callback = InnerTransportCallback::new(
        "loaded",
        Box::new(IgnoredCallback),
        crate::notifier::Notifier::default(),
        load.clone(),
    );
    let fill = |lane| {
        (0..LANE_CREDIT_WINDOW)
            .map(
                |_| match callback.admit_inbound_frame(custom.clone(), lane) {
                    InboundFrameAdmission::Admitted(frame) => frame,
                    _ => panic!("a frame within the window must be admitted"),
                },
            )
            .collect::<Vec<_>>()
    };
    let credit = |lane| {
        callback
            .link_credit()
            .next_credit(credit_index(lane))
            .now_or_never()
            .flatten()
    };
    let bulk_lane = ChannelLane::new(1);
    let (mut control, mut bulk) = (fill(ChannelLane::PRIORITY), fill(bulk_lane));

    control.truncate(control.len() - batch);
    assert_eq!(
        credit(ChannelLane::PRIORITY),
        Some(LANE_CREDIT_WINDOW / 2 + LANE_CREDIT_WINDOW),
        "the priority lane advertises while the node is loaded"
    );
    bulk.truncate(bulk.len() - batch);
    assert!(load.held() >= LANE_CREDIT_WINDOW * frame);
    assert_eq!(credit(bulk_lane), None, "any other lane defers");
    drop((control, bulk));
    assert_eq!(load.held(), 0);
}

/// A connection admits exactly one credit window per lane; the frame beyond it is a credit
/// violation, and a released frame frees its place.
#[test]
fn test_each_lane_admits_exactly_its_credit_window() {
    let callback = callback("peer");
    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    let lane = ChannelLane::new(1);
    let held = (0..LANE_CREDIT_WINDOW)
        .map(
            |_| match callback.admit_inbound_frame(custom.clone(), lane) {
                InboundFrameAdmission::Admitted(frame) => frame,
                _ => panic!("a frame within the window must be admitted"),
            },
        )
        .collect::<Vec<_>>();

    assert!(matches!(
        callback.admit_inbound_frame(custom.clone(), lane),
        InboundFrameAdmission::CreditExceeded {
            received: LANE_CREDIT_WINDOW,
            advertised: LANE_CREDIT_WINDOW,
        }
    ));
    drop(held);
    assert_eq!(callback.link_credit().occupancy(), [0, 0, 0, 0]);
}

/// One lane's full window does not touch another lane's: the frame bound is per lane, and the
/// per-connection bound is their sum.
#[test]
fn test_a_full_lane_leaves_every_other_lane_its_window() {
    let callback = callback("peer");
    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    let held = (0..DATA_CHANNEL_POOL_SIZE)
        .flat_map(|index| (0..LANE_CREDIT_WINDOW).map(move |_| ChannelLane::new(index)))
        .map(
            |lane| match callback.admit_inbound_frame(custom.clone(), lane) {
                InboundFrameAdmission::Admitted(frame) => frame,
                _ => panic!("every lane must admit its own window"),
            },
        )
        .collect::<Vec<_>>();

    assert_eq!(held.len(), INBOUND_PEER_FRAME_CAPACITY);
    assert_eq!(
        callback.link_credit().occupancy(),
        [LANE_CREDIT_WINDOW; DATA_CHANNEL_POOL_SIZE as usize]
    );
}

/// A credit frame is applied, not admitted: it takes no place in any window and is never
/// dispatched.
#[test]
fn test_credit_frames_are_applied_and_never_admitted() {
    let callback = callback("peer");
    let credit = wire(&TransportMessage::Credit(40));
    for _ in 0..LANE_CREDIT_WINDOW.saturating_mul(2) {
        assert!(matches!(
            callback.admit_inbound_frame(credit.clone(), ChannelLane::new(2)),
            InboundFrameAdmission::Credit
        ));
    }
    assert_eq!(callback.link_credit().occupancy(), [0, 0, 0, 0]);
}

/// A remote-created data channel is admitted by the lane its label names, once per lane; a
/// label that names no lane is refused.
#[test]
fn test_inbound_data_channels_are_admitted_by_lane_label_once() {
    let admitted = AtomicU8::new(0);
    for index in 0..DATA_CHANNEL_POOL_SIZE {
        let label = data_channel_label(ChannelLane::new(index));
        assert_eq!(
            admit_inbound_data_channel(&admitted, &label),
            Some(ChannelLane::new(index))
        );
        assert_eq!(admit_inbound_data_channel(&admitted, &label), None);
    }
    let beyond = data_channel_label(ChannelLane::new(DATA_CHANNEL_POOL_SIZE));
    assert_eq!(admit_inbound_data_channel(&admitted, &beyond), None);
    assert_eq!(
        admit_inbound_data_channel(&AtomicU8::new(0), "invalid-frame-accounting"),
        None
    );
}

/// A sender waiting for credit on a full lane fails once the generation ends, although it
/// still holds the callback: the terminal state, not the callback's drop, releases it.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
#[test]
fn test_a_terminal_state_fails_a_sender_waiting_for_credit() {
    use futures::FutureExt;

    let callback = callback("peer");
    let lane = ChannelLane::new(3);
    let held = (0..LANE_CREDIT_WINDOW)
        .map(|_| {
            callback
                .link_credit()
                .reserve(lane)
                .now_or_never()
                .expect("a fresh lane has a window of credit")
                .expect("a fresh lane is open")
        })
        .collect::<Vec<_>>();
    let mut waiting = Box::pin(callback.link_credit().reserve(lane));
    assert!(waiting.as_mut().now_or_never().is_none());

    futures::executor::block_on(
        callback
            .on_peer_connection_state_change(crate::core::transport::WebrtcConnectionState::Closed),
    );

    assert!(matches!(
        waiting.now_or_never(),
        Some(Err(crate::error::Error::LinkCreditClosed(_)))
    ));
    drop(held);
}

/// An abandoned credit wait leaves nothing behind (#913 R7 M2): the wait registers its waker
/// while it waits, and dropping it before credit comes removes exactly its own registration,
/// while another wait on the lane stays registered.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
#[test]
fn test_an_abandoned_credit_wait_leaves_no_waker() {
    use futures::FutureExt;

    let callback = callback("peer");
    let lane = ChannelLane::new(0);
    let held = (0..LANE_CREDIT_WINDOW)
        .map(|_| callback.link_credit().reserve(lane).now_or_never())
        .collect::<Vec<_>>();
    let mut kept = Box::pin(callback.link_credit().reserve(lane));
    assert!(kept.as_mut().now_or_never().is_none());
    let mut abandoned = Box::pin(callback.link_credit().reserve(lane));
    assert!(abandoned.as_mut().now_or_never().is_none());
    assert_eq!(callback.link_credit().send_waiters_for_test(lane), 2);

    drop(abandoned);
    assert_eq!(callback.link_credit().send_waiters_for_test(lane), 1);
    drop(kept);
    assert_eq!(callback.link_credit().send_waiters_for_test(lane), 0);
    drop(held);
}

/// A reservation is settled by its send's fate: a send cancelled before its irrevocable
/// boundary returns the credit, and an irrevocable one consumes it.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
#[test]
fn test_a_reservation_commits_only_when_its_send_became_irrevocable() {
    use futures::FutureExt;

    use crate::core::transport::SendPermit;

    let callback = callback("peer");
    let lane = ChannelLane::new(0);
    let reserve = || {
        callback
            .link_credit()
            .reserve(lane)
            .now_or_never()
            .expect("the lane holds credit")
            .expect("the lane is open")
    };
    for _ in 0..LANE_CREDIT_WINDOW.saturating_mul(2) {
        drop(reserve().bind(SendPermit::always().acceptance()));
    }
    for _ in 0..LANE_CREDIT_WINDOW {
        let permit = SendPermit::always();
        let acceptance = permit.acceptance();
        permit
            .try_mark_irrevocable()
            .expect("an unconditional permit crosses its boundary")
            .mark_accepted();
        drop(reserve().bind(acceptance));
    }
    assert!(callback
        .link_credit()
        .reserve(lane)
        .now_or_never()
        .is_none());
}

/// A transport callback that reports each payload it is handed, the first only once the test
/// opens its gate, so the frames behind it queue on their lane.
#[cfg(feature = "native-webrtc")]
struct OrderRecordingCallback {
    /// Opened by the test once every frame is queued.
    gate: std::sync::Arc<tokio::sync::Semaphore>,
    /// The payloads in the order the lane handed them over.
    seen: tokio::sync::mpsc::UnboundedSender<Bytes>,
}

#[cfg(feature = "native-webrtc")]
#[async_trait::async_trait]
impl crate::core::callback::TransportCallback for OrderRecordingCallback {
    async fn on_admitted_message(
        &self,
        message: crate::core::callback::AdmittedInboundMessage<'_>,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (_, payload, _lease) = message.into_parts();
        let _open = self.gate.acquire().await;
        self.seen.send(payload).ok();
        Ok(())
    }
}

/// A lane hands its frames to the protocol one at a time, in arrival order, while the first
/// is still being handled.
#[cfg(feature = "native-webrtc")]
#[tokio::test]
async fn test_a_lane_hands_its_frames_over_in_arrival_order() {
    let (seen, mut handed) = tokio::sync::mpsc::unbounded_channel();
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let callback = std::sync::Arc::new(InnerTransportCallback::new(
        "peer",
        Box::new(OrderRecordingCallback {
            gate: std::sync::Arc::clone(&gate),
            seen,
        }),
        crate::notifier::Notifier::default(),
        NodeReceiveLoad::new(),
    ));
    let lane = ChannelLane::new(1);
    let count = u8::try_from(LANE_CREDIT_WINDOW).expect("the window fits a byte");
    for index in 0..count {
        let raw = wire(&TransportMessage::Custom(Bytes::from(vec![index])));
        let InboundFrameAdmission::Admitted(frame) = callback.admit_inbound_frame(raw, lane) else {
            panic!("a frame within the window must be admitted");
        };
        callback.dispatch_admitted_frame(frame, lane);
    }
    gate.add_permits(usize::from(count));

    let mut order = Vec::new();
    while order.len() < usize::from(count) {
        let payload = handed
            .recv()
            .await
            .expect("the lane hands every frame over");
        order.extend_from_slice(payload.as_ref());
    }
    assert_eq!(order, (0..count).collect::<Vec<_>>());
}

/// A credit settles only its own connection's window: a send that carries a credit reserved on
/// another connection is refused, so neither window is charged for the other's frame.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
#[test]
fn test_a_send_carrying_another_connections_credit_is_refused() {
    use futures::FutureExt;

    use crate::callback::link_credit::LaneCreditReservation;
    use crate::core::transport::SendPermit;
    use crate::error::Error;

    let (own, other) = (callback("own"), callback("other"));
    let lane = ChannelLane::new(1);
    let foreign = other
        .link_credit()
        .reserve(lane)
        .now_or_never()
        .expect("a fresh lane has credit")
        .expect("a fresh lane is open");
    let mut permit = SendPermit::always().with_credit(LaneCreditReservation(foreign));
    let message = TransportMessage::Custom(Bytes::from_static(b"data"));
    assert!(matches!(
        own.credit_for_send(&message, lane, &mut permit)
            .now_or_never()
            .expect("a carried credit is judged at once"),
        Err(Error::ForeignCredit(_))
    ));
}

/// A credit whose send failed is queued again, joined with any credit released meanwhile, so the
/// peer's sender is never left without the last advertisement; once the generation is closed
/// nothing is queued, since no send can succeed.
#[cfg(feature = "native-webrtc")]
#[test]
fn test_a_credit_whose_send_failed_is_queued_again() {
    use futures::FutureExt;

    use crate::core::credit::CreditIndex;

    let callback = callback("peer");
    let lane = ChannelLane::new(1);
    let index = CreditIndex::ALL[1];
    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    let frames = (0..LANE_CREDIT_WINDOW)
        .map(
            |_| match callback.admit_inbound_frame(custom.clone(), lane) {
                InboundFrameAdmission::Admitted(frame) => frame,
                _ => panic!("a frame within the window must be admitted"),
            },
        )
        .collect::<Vec<_>>();
    drop(frames);
    let link = callback.link_credit();
    let advertised = link
        .next_credit(index)
        .now_or_never()
        .flatten()
        .expect("a released window advertises");

    assert!(link.requeue(index, advertised - 1));
    assert!(link.requeue(index, advertised));
    assert_eq!(
        link.next_credit(index).now_or_never().flatten(),
        Some(advertised)
    );
    assert!(link.next_credit(index).now_or_never().is_none());

    link.close();
    assert!(!link.requeue(index, advertised));
    assert_eq!(link.next_credit(index).now_or_never(), Some(None));
}
