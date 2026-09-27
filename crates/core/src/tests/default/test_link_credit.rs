//! Per-link credit flow control end to end over the controlled dummy transport (#904): a
//! receiver that processes nothing holds at most `w` payload frames of its sender, so the
//! transport refuses none, and once it processes them the whole message arrives, a lost credit
//! return repaired by the next.
//!
//! The load is one message forced into three windows of chunk frames (a small negotiated
//! message size), so every frame is gated by credit and no message-rate quota is involved.
//! Every wait is an activity-woken probe (a frame sent, arrived or delivered by the test), whose
//! only timer is the hang guard.

use std::cell::Cell;
use std::sync::Arc;

use rings_transport::connections::dummy_controlled;
use rings_transport::connections::dummy_controlled::QueuedDeliveryKind;

use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::message::is_payload_frame;
use crate::message::LinkControl;
use crate::message::LinkFrame;
use crate::message::Message;
use crate::swarm::Swarm;
use crate::swarm::LINK_CREDIT_WINDOW;
use crate::tests::activity::probe_on_activity;
use crate::tests::activity::record_activity;
use crate::tests::default::dummy_hooks::ControlledDeliveryGuard;
use crate::tests::default::dummy_hooks::MaxMessageSizeGuard;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::Node;
use crate::tests::default::TEST_HANG_GUARD;
use crate::tests::manually_establish_connection;

/// The negotiated message size of the tests: a frame carries about half of it as chunk data.
const MESSAGE_SIZE: usize = 8_192;

/// The bytes of the one message: at least three windows of chunk frames, so the sender stalls
/// and resumes twice.
const MESSAGE_BYTES: usize = 3 * LINK_CREDIT_WINDOW as usize * MESSAGE_SIZE / 2;

/// The fixed secret key whose scalar repeats the hex byte `byte`.
fn fixed_key(byte: &str) -> SecretKey {
    SecretKey::try_from(byte.repeat(32).as_str()).expect("a fixed scalar")
}

/// The one message the tests send: distinct bytes, so its reassembly is checked exactly.
fn message_bytes() -> Vec<u8> {
    (0..MESSAGE_BYTES)
        .map(|index| u8::try_from(index % 251).expect("below 251"))
        .collect()
}

/// Two connected nodes, quiescent, with controlled delivery on while the guard lives:
/// `(sender, receiver, guard)`.
async fn connected_pair() -> (Node, Node, ControlledDeliveryGuard) {
    let sender = prepare_node(fixed_key("21")).await;
    let receiver = prepare_node(fixed_key("22")).await;
    manually_establish_connection(&sender.swarm, &receiver.swarm).await;
    wait_for_msgs([&sender, &receiver]).await;
    (sender, receiver, ControlledDeliveryGuard::new())
}

/// Send the message from `sender` to `receiver` in a task of its own: its chunk frames stall on
/// credit while the receiver processes nothing.
fn send_message(sender: Arc<Swarm>, receiver: Did) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let message = Message::custom(&message_bytes())?;
        sender.send_direct_message(message, receiver).await?;
        Ok(())
    })
}

/// The payload frames queued on this thread's controlled transport: the tests' one link is the
/// only one carrying any, since the pair is quiescent when controlled delivery starts.
fn queued_payload_frames() -> usize {
    dummy_controlled::inspect_after(None)
        .iter()
        .filter(|delivery| {
            matches!(delivery.kind(), QueuedDeliveryKind::Message(bytes)
                if is_payload_frame(bytes.as_ref()))
        })
        .count()
}

/// Whether a queued delivery is a credit return.
fn is_credit_return(kind: &QueuedDeliveryKind) -> bool {
    matches!(kind, QueuedDeliveryKind::Message(bytes)
    if matches!(
        LinkFrame::from_wire(bytes.as_ref()),
        Ok(LinkFrame::Control(LinkControl::Credit(_)))
    ))
}

/// Deliver nothing until the sender's link to `receiver` holds `w` frames in flight.
async fn await_stalled_window(sender: &Swarm, receiver: Did) -> Result<()> {
    probe_on_activity("the sender fills its window", TEST_HANG_GUARD, || async {
        Ok((sender.link_credit(receiver)?.in_flight() == LINK_CREDIT_WINDOW).then_some(()))
    })
    .await
}

/// Deliver queued events in order, discarding the first credit return if `lose_a_return`,
/// until `receiver` has received a custom message, and return its bytes. Each delivery or
/// discard is a state change the probe records, so it runs again after it.
async fn deliver_until_received(receiver: &Node, lose_a_return: bool) -> Result<Vec<u8>> {
    let lost = Cell::new(!lose_a_return);
    probe_on_activity("the credited message arrives", TEST_HANG_GUARD, || async {
        while let Some(payload) = receiver.try_listen_once().await {
            if let Ok(Message::CustomMessage(custom)) = payload.transaction.data::<Message>() {
                return Ok(Some(custom.0));
            }
        }
        if let Some(next) = dummy_controlled::inspect_after(None).into_iter().next() {
            if !lost.get() && is_credit_return(next.kind()) {
                lost.set(dummy_controlled::discard_sequence(next.sequence()));
            } else {
                dummy_controlled::deliver_sequence(next.sequence()).await;
            }
            record_activity();
        }
        Ok(None)
    })
    .await
}

/// No transport drop (#904): while the receiver processes nothing, its sender stalls with `w`
/// frames in flight and at most `w` payload frames queued toward it, half its transport's
/// per-peer bound (frames it released before, but has not yet returned, count in flight too),
/// so no frame is refused; once it processes them, the whole message arrives.
#[tokio::test]
async fn test_a_stalled_receiver_holds_at_most_one_window_and_everything_arrives() -> Result<()> {
    let _size = MaxMessageSizeGuard::new(MESSAGE_SIZE);
    let (sender, receiver, _controlled) = connected_pair().await;
    let send = send_message(Arc::clone(&sender.swarm), receiver.did());

    await_stalled_window(&sender.swarm, receiver.did()).await?;
    let queued = queued_payload_frames();
    assert!(
        queued > 0 && queued <= LINK_CREDIT_WINDOW as usize,
        "the receiver holds at most one window: {queued}"
    );

    assert_eq!(
        deliver_until_received(&receiver, false).await?,
        message_bytes()
    );
    send.await.expect("the send task runs to its end")?;
    Ok(())
}

/// Liveness (#904): a credit return lost on the way is repaired by the next cumulative return,
/// so the whole message still arrives.
#[tokio::test]
async fn test_a_lost_credit_return_is_repaired_by_the_next() -> Result<()> {
    let _size = MaxMessageSizeGuard::new(MESSAGE_SIZE);
    let (sender, receiver, _controlled) = connected_pair().await;
    let send = send_message(Arc::clone(&sender.swarm), receiver.did());

    await_stalled_window(&sender.swarm, receiver.did()).await?;
    assert_eq!(
        deliver_until_received(&receiver, true).await?,
        message_bytes()
    );
    send.await.expect("the send task runs to its end")?;
    Ok(())
}
