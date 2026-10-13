//! Delegation references between real swarms over the dummy transport's immediate delivery.
//! Every wait is the arrival of a message at a node's inbound tap.

use rings_test_support::with_hang_guard;

use crate::delegation::DelegationDigest;
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
use crate::message::TRANSACTION_REPLAY_WINDOW;
use crate::swarm::application::ApplicationData;
use crate::swarm::transport::dispatched_link_control_for_test;
use crate::swarm::transport::dispatched_link_control_since;
use crate::swarm::transport::hold_first_frame_deadlines_for_test;
use crate::swarm::transport::referenced_slots_for_test;
use crate::swarm::transport::LinkDirection;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::default::TEST_HANG_GUARD;
use crate::tests::manually_establish_connection;
use crate::tests::TEST_NETWORK_ID;

/// How many messages a link needs, at most, to be seen switching to references over the dummy
/// transport's immediate delivery. The confirmation crosses a fixed number of task hops (the
/// receiver's spawned control send, the dummy's dispatch task, the sender's callback) and each
/// awaited round trip below yields to every task spawned before it, so the switch is visible
/// within a small constant number of messages; the bound leaves room for the overlay's own
/// traffic to interleave.
const SWITCH_WITHIN_MESSAGES: usize = 8;

/// A same-class backlog longer than the replay window: a frame overtaken by this many later
/// frames of its class would be refused as stale.
const BACKLOG_BEYOND_REPLAY_WINDOW: usize = TRANSACTION_REPLAY_WINDOW + 8;

/// The digests that went by reference on `link` so far, per slot.
fn referenced_on(link: LinkDirection) -> PerSlot<Vec<DelegationDigest>> {
    referenced_slots_for_test()
        .remove(&link)
        .unwrap_or(PerSlot {
            origin: Vec::new(),
            hop: Vec::new(),
        })
}

/// Whether any reference missed on this test thread since `dispatched_before` control frames had
/// been dispatched: a miss is answered with a disclaimer.
fn misses_since(dispatched_before: usize) -> bool {
    dispatched_link_control_since(dispatched_before)
        .iter()
        .any(|(_, control)| matches!(control, LinkControl::Unknown(_)))
}

/// The next `count` custom messages at `node` that `origin` signed and whose data `wanted`
/// accepts, in arrival order. Everything else is put back in the node's inbox, in order, when
/// the scan ends. Each message must arrive within [`TEST_HANG_GUARD`] of the one before it.
async fn scan_custom_from(
    node: &Node,
    origin: Did,
    count: usize,
    wanted: impl Fn(&[u8]) -> bool,
) -> Result<Vec<MessagePayload>> {
    let mut scan = node.message_scan().await;
    let mut found = Vec::with_capacity(count);
    while found.len() < count {
        let label = format!(
            "custom message {} of {count} from {origin}",
            found.len() + 1
        );
        let payload = with_hang_guard(&label, TEST_HANG_GUARD, scan.next())
            .await
            .ok_or_else(|| Error::InvalidMessage(format!("node inbox closed before {label}")))?;
        let accepted = payload.transaction.signer() == origin
            && matches!(
                payload.transaction.data::<Message>(),
                Ok(Message::CustomMessage(CustomMessage(custom))) if wanted(&custom)
            );
        if accepted {
            found.push(payload);
        } else {
            scan.skip(payload);
        }
    }
    Ok(found)
}

/// The data of `payload`'s custom message; empty for any other message.
fn custom_data(payload: &MessagePayload) -> Vec<u8> {
    match payload.transaction.data::<Message>() {
        Ok(Message::CustomMessage(CustomMessage(custom))) => custom,
        _ => Vec::new(),
    }
}

/// The next custom message at `node` that `origin` signed and that carries `data`.
async fn next_custom_message_from(node: &Node, origin: Did, data: &[u8]) -> Result<MessagePayload> {
    scan_custom_from(node, origin, 1, |custom| custom == data)
        .await?
        .pop()
        .ok_or_else(|| Error::InvalidMessage("scan returned no message".to_string()))
}

/// The data of the next `count` custom messages at `node` that `origin` signed, in arrival
/// order.
async fn custom_data_from(node: &Node, origin: Did, count: usize) -> Result<Vec<Vec<u8>>> {
    let found = scan_custom_from(node, origin, count, |_| true).await?;
    Ok(found.iter().map(custom_data).collect())
}

/// Send `data` from `sender` to `destination` with `relay` as its first hop.
async fn send_by_hop(sender: &Node, relay: &Node, destination: &Node, data: &[u8]) -> Result<()> {
    sender
        .swarm
        .transport
        .protocol_egress()
        .send_message_by_hop(Message::custom(data)?, destination.did(), relay.did())
        .await
        .map(drop)
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
    send_by_hop(sender, relay, destination, data).await?;
    let delivered = next_custom_message_from(destination, sender.did(), data).await?;
    assert_eq!(delivered.transaction.destination, destination.did());
    assert_eq!(delivered.signer(), relay.did());
    assert!(delivered.verify_transaction_and_payload(TEST_NETWORK_ID));
    Ok(delivered)
}

/// Acceptance, end to end: two swarms exchange the join traffic and then messages. The link
/// reaches references on its own (the receiver leases what it verified, the sender switches),
/// and no reference ever misses.
#[tokio::test]
async fn test_link_reaches_references_without_a_miss() -> Result<()> {
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
            .send_direct_message(ApplicationData::custom(b"steady"), right.did())
            .await?;
        next_custom_message_from(&right, left.did(), b"steady").await?;
        sent += 1;
    }
    assert!(!misses_since(dispatched_before));
    Ok(())
}

/// The relayed case the design turns on: a destination that never met the origin verifies the
/// origin's proof at no extra cost, because the last hop sends the origin's session inline until
/// the destination leases it, and by reference afterwards. No reference misses anywhere on the
/// path.
#[tokio::test]
async fn test_relayed_origin_session_never_misses() -> Result<()> {
    let origin = prepare_node(SecretKey::random()).await;
    let relay = prepare_node(SecretKey::random()).await;
    let destination = prepare_node(SecretKey::random()).await;
    establish_admitted_link(&origin, &relay).await?;
    establish_admitted_link(&relay, &destination).await?;
    let dispatched_before = dispatched_link_control_for_test().len();
    let last_hop = (relay.did(), destination.did());
    let first = relay_custom(&origin, &relay, &destination, b"relayed").await?;
    let origin_digest = first.transaction.verification.delegation.digest()?;

    // The relay forwards the origin's session inline until the destination leases it, then
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
    assert!(!misses_since(dispatched_before));
    Ok(())
}

/// Acceptance (#908): a same-class backlog longer than the replay window, sent back to back
/// while the last hop switches the origin's new session from inline to a reference, reaches
/// the destination whole and in order, and no reference misses. Nothing is held on the link,
/// so no frame of the backlog is overtaken and lost as stale.
///
/// The backlog outruns a lane's credit window, so most of it waits in the sender's queue for
/// credit. The protocol egress drops a payload whose first frame is not admitted within the
/// first-frame deadline, as local backpressure its origin's retry covers, and the dummy
/// profile's deadline is shorter than a debug build takes to relay the window. That loss is
/// the egress's, not the link's, and this law is not about it, so the test holds the deadline:
/// every frame of the backlog waits for credit instead.
#[tokio::test]
async fn test_a_backlog_across_a_session_switch_loses_no_frame() -> Result<()> {
    let origin = prepare_node(SecretKey::random()).await;
    let relay = prepare_node(SecretKey::random()).await;
    let destination = prepare_node(SecretKey::random()).await;
    establish_admitted_link(&origin, &relay).await?;
    establish_admitted_link(&relay, &destination).await?;
    let dispatched_before = dispatched_link_control_for_test().len();
    let _waits_for_credit = hold_first_frame_deadlines_for_test();

    let backlog = (0..BACKLOG_BEYOND_REPLAY_WINDOW)
        .map(|index| format!("backlog {index}").into_bytes())
        .collect::<Vec<_>>();
    for data in &backlog {
        send_by_hop(&origin, &relay, &destination, data).await?;
    }
    assert_eq!(
        custom_data_from(&destination, origin.did(), backlog.len()).await?,
        backlog
    );
    assert!(!misses_since(dispatched_before));
    Ok(())
}
