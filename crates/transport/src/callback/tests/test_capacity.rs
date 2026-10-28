//! Receive-side admission against per-lane credit, and data-channel admission by label.

use std::sync::atomic::AtomicU8;

use bytes::Bytes;

use super::*;
use crate::core::credit::LANE_CREDIT_WINDOW;
use crate::core::pool::ChannelLane;
use crate::core::pool::DATA_CHANNEL_POOL_SIZE;
use crate::core::transport::TransportMessage;

/// Law 3 (pool, rebalance) and Prop. 3(5): a data lane is lent within its edge's fair claim
/// `⌈P_F / n⌉`, first come, first served; when an edge below its share `⌊P_F / n⌋` finds the
/// pool full, every edge above its share is shrunk to it, and the returns pay the waiting edge,
/// which ends at its claim with the pool full and no edge above its claim. A closed edge gives
/// its share back.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
#[test]
fn test_the_pool_lends_within_fair_claims_and_rebalances_to_the_shares() {
    use crate::core::credit::LaneSignal;

    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    let lane = ChannelLane::new(1);
    let pool = NodeFramePool::with_capacity(40);
    let (first, second, third) = (
        pooled_callback("first", pool.clone()),
        pooled_callback("second", pool.clone()),
        pooled_callback("third", pool.clone()),
    );
    let admit = |callback: &InnerTransportCallback| match callback
        .admit_inbound_frame(custom.clone(), lane)
    {
        InboundFrameAdmission::Admitted(frame) => frame,
        _ => panic!("a frame within the credit is admitted"),
    };
    let held_by = |callback: &InnerTransportCallback| pool.held_by(callback.link_credit());
    // Alone, then two: each is lent its lane's whole window, within its claim.
    drop(admit_window(&first, &custom, lane));
    drop(admit(&second));
    assert_eq!((held_by(&first), held_by(&second)), (15, 15));

    // A third edge's claim is ⌈40 / 3⌉ = 14, its share ⌊40 / 3⌋ = 13: lent the 10 free, it is
    // below its share, so both others are shrunk to 13.
    let third_frame = admit(&third);
    assert_eq!(held_by(&third), 10);
    assert_eq!(
        take_due_signal(&first, lane),
        Some(LaneSignal::Shrink {
            limit: 30,
            generation: 1,
        })
    );
    assert_eq!(
        take_due_signal(&second, lane),
        Some(LaneSignal::Shrink {
            limit: 15,
            generation: 1,
        })
    );
    // The returns give the excess back, and the waiting edge is paid up to its claim.
    for (callback, bound) in [(&first, 30), (&second, 15)] {
        callback.link_credit().apply(lane, LaneSignal::Returned {
            bound,
            generation: 1,
        });
    }
    assert_eq!(
        (held_by(&first), held_by(&second), held_by(&third)),
        (13, 13, 14)
    );
    assert_eq!(pool.used(), 40);
    assert_eq!(take_due_signal(&third, lane), Some(credit(1 + 14)));
    assert_eq!(
        pool.waiting(),
        1,
        "the third edge, at its claim, still wants one frame"
    );

    third.link_credit().close();
    drop(third_frame);
    assert_eq!(pool.used(), 26, "a closed edge gives its share back");
    assert_eq!(pool.waiting(), 0);
}

/// A link that ends without `close` leaves the pool as it found it: its drop releases whatever
/// the pool counts for its edge and its waiting demands, and a later link never shares its
/// edge, since edges are keyed by a number the pool never gives twice.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
#[test]
fn test_a_link_dropped_without_close_leaves_the_pool_unchanged() {
    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    let lane = ChannelLane::new(1);
    let pool = NodeFramePool::with_capacity(20);
    let (lender, waiter) = (
        pooled_callback("lender", pool.clone()),
        pooled_callback("waiter", pool.clone()),
    );
    drop(admit_window(&lender, &custom, lane));
    let waiting = match waiter.admit_inbound_frame(custom.clone(), lane) {
        InboundFrameAdmission::Admitted(frame) => frame,
        _ => panic!("the floor frame is admitted"),
    };
    assert_eq!((pool.used(), pool.edges()), (20, 2));
    assert_eq!(pool.waiting(), 1);
    drop(waiting);
    drop(waiter);
    assert_eq!(pool.waiting(), 0, "the dropped link's demand left the pool");
    assert_eq!(pool.used(), 15, "and so did its share");
    drop(lender);
    assert_eq!((pool.used(), pool.edges(), pool.waiting()), (0, 0, 0));

    let fresh = pooled_callback("fresh", pool.clone());
    drop(admit_window(&fresh, &custom, lane));
    assert_eq!(pool.held_by(fresh.link_credit()), 15);
    assert_eq!((pool.used(), pool.edges()), (15, 1));
}

/// Law 3 and Prop. 3(5): the priority lane's floor is its whole window, so it admits a full
/// window and advertises at once with the pool empty, while a data lane is held to its floor.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
#[test]
fn test_the_priority_lane_never_waits_for_the_pool() {
    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    let pool = NodeFramePool::with_capacity(0);
    let callback = pooled_callback("loaded", pool.clone());
    let mut control = admit_window(&callback, &custom, ChannelLane::PRIORITY);
    control.truncate(control.len() - usize::try_from(LANE_CREDIT_WINDOW / 2).unwrap_or(0));
    assert_eq!(
        take_due_signal(&callback, ChannelLane::PRIORITY),
        Some(credit(LANE_CREDIT_WINDOW / 2 + LANE_CREDIT_WINDOW))
    );

    let bulk = ChannelLane::new(1);
    let floor = callback.admit_inbound_frame(custom.clone(), bulk);
    assert!(matches!(floor, InboundFrameAdmission::Admitted(_)));
    assert!(matches!(
        callback.admit_inbound_frame(custom.clone(), bulk),
        InboundFrameAdmission::CreditExceeded {
            received: 1,
            advertised: 1,
        }
    ));
    assert_eq!(pool.used(), 0);
    drop((control, floor));
}

/// Law 3 (idle) and Def. Reclaim: a data lane that holds a share and admitted no frame between
/// two idle checks is shrunk to its floor in a new generation; it advertises nothing until the
/// sender's return, which ends the shrink and gives the share back to the pool.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
#[test]
fn test_an_idle_lane_is_shrunk_and_returns_its_share() {
    use crate::core::credit::LaneSignal;

    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    let lane = ChannelLane::new(2);
    let pool = NodeFramePool::with_capacity(64);
    let callback = pooled_callback("idle", pool.clone());
    let link = callback.link_credit();
    drop(admit_window(&callback, &custom, lane));
    assert_eq!(pool.used(), LANE_CREDIT_WINDOW - 1);
    take_due_signal(&callback, lane);

    assert!(
        link.idle_check(),
        "the window's admissions keep the lane active"
    );
    assert_eq!(take_due_signal(&callback, lane), None);
    assert!(link.idle_check());
    let shrink = LaneSignal::Shrink {
        limit: LANE_CREDIT_WINDOW + 1,
        generation: 1,
    };
    assert_eq!(take_due_signal(&callback, lane), Some(shrink));
    assert_eq!(
        pool.used(),
        LANE_CREDIT_WINDOW - 1,
        "the share is counted until the return"
    );

    link.apply(lane, LaneSignal::Returned {
        bound: LANE_CREDIT_WINDOW + 1,
        generation: 1,
    });
    assert_eq!(pool.used(), 0, "the return gives the share back");
    assert_eq!(take_due_signal(&callback, lane), None);
}

/// A connection admits exactly one credit window per lane, and a full lane leaves every other
/// lane its window: lanes are filled in turn, each to the frame beyond its window, which is a
/// credit violation. The frame bound is per lane, the per-connection bound is their sum, and
/// released frames free their places.
#[test]
fn test_each_lane_admits_exactly_its_own_credit_window() {
    let callback = callback("peer");
    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    let held = (0..DATA_CHANNEL_POOL_SIZE)
        .flat_map(|index| {
            let lane = ChannelLane::new(index);
            let window = admit_window(&callback, &custom, lane);
            assert!(matches!(
                callback.admit_inbound_frame(custom.clone(), lane),
                InboundFrameAdmission::CreditExceeded {
                    received: LANE_CREDIT_WINDOW,
                    advertised: LANE_CREDIT_WINDOW,
                }
            ));
            window
        })
        .collect::<Vec<_>>();

    assert_eq!(held.len(), INBOUND_PEER_FRAME_CAPACITY);
    assert_eq!(
        callback.link_credit().occupancy(),
        [LANE_CREDIT_WINDOW; DATA_CHANNEL_POOL_SIZE as usize]
    );
    drop(held);
    assert_eq!(callback.link_credit().occupancy(), [0, 0, 0, 0]);
}

/// A credit frame is applied, not admitted: it takes no place in any window and is never
/// dispatched.
#[test]
fn test_credit_frames_are_applied_and_never_admitted() {
    let callback = callback("peer");
    let credit = wire(&TransportMessage::Signal(
        crate::core::credit::LaneSignal::Credit {
            limit: 40,
            generation: 0,
        },
    ));
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
    let held = reserve_window(&callback, lane);
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

/// An abandoned credit wait leaves nothing behind: the wait registers its waker while it
/// waits, and dropping it before credit comes removes exactly its own registration, while
/// another wait on the lane stays registered.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
#[test]
fn test_an_abandoned_credit_wait_leaves_no_waker() {
    use futures::FutureExt;

    let callback = callback("peer");
    let lane = ChannelLane::new(0);
    let held = reserve_window(&callback, lane);
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
        NodeFramePool::new(),
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
    assert!(matches!(
        own.credit_for_custom_send(lane, &mut permit)
            .now_or_never()
            .expect("a carried credit is judged at once"),
        Err(Error::ForeignCredit(_))
    ));
}

/// A signal whose send failed is queued again, joined with any credit released meanwhile, so
/// the peer's sender is never left without the last advertisement; once the generation is
/// closed nothing is queued, since no send can succeed.
#[cfg(feature = "native-webrtc")]
#[test]
fn test_a_credit_whose_send_failed_is_queued_again() {
    use futures::FutureExt;

    use crate::core::credit::CreditIndex;

    let callback = callback("peer");
    let lane = ChannelLane::new(1);
    let index = CreditIndex::ALL[1];
    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    drop(admit_window(&callback, &custom, lane));
    let link = callback.link_credit();
    let Some(crate::core::credit::LaneSignal::Credit { limit, .. }) =
        take_due_signal(&callback, lane)
    else {
        panic!("a released window advertises");
    };

    assert!(link.requeue(index, credit(limit - 1)));
    assert!(link.requeue(index, credit(limit)));
    assert_eq!(take_due_signal(&callback, lane), Some(credit(limit)));
    assert!(link.next_signal(index).now_or_never().is_none());

    link.close();
    assert!(!link.requeue(index, credit(limit)));
    assert_eq!(link.next_signal(index).now_or_never(), Some(None));
}
