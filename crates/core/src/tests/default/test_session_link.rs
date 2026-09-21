//! Session references between real swarms over the dummy transport's immediate delivery.
//! Every wait is the arrival of a message at a node's callback.

use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::CustomMessage;
use crate::message::LinkControl;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::PayloadSender;
use crate::message::PerSlot;
use crate::session::SessionDigest;
use crate::swarm::transport::dispatched_link_control_for_test;
use crate::swarm::transport::referenced_slots_for_test;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;
use crate::tests::TEST_NETWORK_ID;

/// How many messages a link needs, at most, to be seen switching to references over the dummy
/// transport's immediate delivery. The confirmation crosses a fixed number of task hops (the
/// receiver's spawned control send, the dummy's dispatch task, the sender's callback) and each
/// awaited round trip below yields to every task spawned before it, so the switch is visible
/// within a small constant number of messages; the bound leaves room for the overlay's own
/// traffic to interleave.
const SWITCH_WITHIN_MESSAGES: usize = 8;

/// The digests that went by reference on `link` so far, per slot.
fn referenced_on(link: (Did, Did)) -> PerSlot<Vec<SessionDigest>> {
    referenced_slots_for_test()
        .remove(&link)
        .unwrap_or(PerSlot {
            origin: Vec::new(),
            hop: Vec::new(),
        })
}

/// Whether any question was asked on this test thread since `dispatched_before` control frames
/// had been dispatched.
fn questions_asked_since(dispatched_before: usize) -> bool {
    dispatched_link_control_for_test()
        .split_off(dispatched_before)
        .iter()
        .any(|(_, control)| matches!(control, LinkControl::Request(_)))
}

/// The next custom message at `node` that `origin` signed and that carries `data`, skipping
/// the overlay's own traffic.
async fn next_custom_message_from(node: &Node, origin: Did, data: &[u8]) -> Result<MessagePayload> {
    let mut scan = node.message_scan().await;
    loop {
        let payload = scan.next().await.ok_or_else(|| {
            Error::InvalidMessage("node inbox closed before the message arrived".to_string())
        })?;
        let carries_data = matches!(
            payload.transaction.data::<Message>(),
            Ok(Message::CustomMessage(CustomMessage(custom))) if custom == data
        );
        if carries_data && payload.transaction.signer() == origin {
            return Ok(payload);
        }
    }
}

/// Connect `left` and `right` and return once each has admitted the other: the join traffic has
/// been consumed and each end lists the other as a successor.
async fn establish_admitted_link(left: &Node, right: &Node) -> Result<()> {
    manually_establish_connection(&left.swarm, &right.swarm).await;
    wait_for_msgs([left, right]).await;
    wait_for_successor(left, right.did()).await?;
    wait_for_successor(right, left.did()).await
}

/// Send `data` from `sender` to `destination` through `relay`, and wait for it to arrive: the
/// payload as delivered, with the origin's session in its transaction proof.
async fn relay_custom(
    sender: &Node,
    relay: &Node,
    destination: &Node,
    data: &[u8],
) -> Result<MessagePayload> {
    sender
        .swarm
        .transport
        .send_message_by_hop(Message::custom(data)?, destination.did(), relay.did())
        .await?;
    let delivered = next_custom_message_from(destination, sender.did(), data).await?;
    assert_eq!(delivered.transaction.destination, destination.did());
    assert_eq!(delivered.signer(), relay.did());
    assert!(delivered.verify_transaction_and_payload(TEST_NETWORK_ID));
    Ok(delivered)
}

/// Acceptance, end to end: two swarms exchange the join traffic and then messages. The link
/// reaches references on its own (the receiver confirms what it verified, the sender switches),
/// and no question is ever asked, because nothing was forgotten.
#[tokio::test]
async fn test_link_reaches_references_without_a_question() -> Result<()> {
    let left = prepare_node(SecretKey::random()).await;
    let right = prepare_node(SecretKey::random()).await;
    let dispatched_before = dispatched_link_control_for_test().len();
    establish_admitted_link(&left, &right).await?;
    let link = (left.did(), right.did());
    let referenced_before = referenced_on(link);

    // Confirmations travel as control frames after the join traffic; send until a frame goes
    // by reference, which the confirmation exchange guarantees within a few messages.
    let mut sent = 0;
    while referenced_on(link) == referenced_before {
        assert!(
            sent < SWITCH_WITHIN_MESSAGES,
            "the link never switched to references"
        );
        left.swarm
            .transport
            .send_direct_message(Message::custom(b"steady")?, right.did())
            .await?;
        next_custom_message_from(&right, left.did(), b"steady").await?;
        sent += 1;
    }
    assert!(!questions_asked_since(dispatched_before));
    Ok(())
}

/// The relayed case the design turns on: a destination that never met the origin verifies the
/// origin's proof at no extra cost, because the last hop sends the origin's session inline until
/// the destination confirms it, and by reference afterwards. No question is asked anywhere on
/// the path.
#[tokio::test]
async fn test_relayed_origin_session_needs_no_question() -> Result<()> {
    let origin = prepare_node(SecretKey::random()).await;
    let relay = prepare_node(SecretKey::random()).await;
    let destination = prepare_node(SecretKey::random()).await;
    establish_admitted_link(&origin, &relay).await?;
    establish_admitted_link(&relay, &destination).await?;
    let dispatched_before = dispatched_link_control_for_test().len();
    let last_hop = (relay.did(), destination.did());
    let first = relay_custom(&origin, &relay, &destination, b"relayed").await?;
    let origin_digest = first.transaction.verification.session.digest()?;

    // The relay forwards the origin's session inline until the destination confirms it, then
    // by reference: within a few messages a frame on the last hop carries the origin's own
    // session, not one of the relay's, as a digest in its origin slot.
    let mut sent = 1;
    while !referenced_on(last_hop).origin.contains(&origin_digest) {
        assert!(
            sent < SWITCH_WITHIN_MESSAGES,
            "the relay never referenced the origin's session"
        );
        relay_custom(&origin, &relay, &destination, b"relayed").await?;
        sent += 1;
    }
    assert!(!questions_asked_since(dispatched_before));
    Ok(())
}
