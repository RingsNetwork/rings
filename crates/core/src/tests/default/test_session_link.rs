//! Session references between real swarms over the dummy transport's immediate (sequenced)
//! delivery. Every wait is the arrival of a message at a node's callback.

use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::CustomMessage;
use crate::message::HopBudget;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageRelay;
use crate::message::MessageSigner;
use crate::message::MessageVerificationExt;
use crate::message::PayloadSender;
use crate::message::Transaction;
use crate::session::SessionSk;
use crate::swarm::transport::session_announcement_count_for_test;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;
use crate::tests::TEST_NETWORK_ID;

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

/// A transaction `origin` signed for `destination`, carried one hop by `hop`.
fn carried_by(
    origin: &SessionSk,
    hop: &Node,
    destination: Did,
    sequence: u64,
    data: &[u8],
) -> Result<MessagePayload> {
    let transaction = Transaction::new(
        destination,
        crate::utils::new_uuid(),
        sequence,
        Message::custom(data)?,
        MessageSigner::new(origin, TEST_NETWORK_ID),
    )?;
    let relay = MessageRelay::new(destination, destination, HopBudget::MAX);
    MessagePayload::new(transaction, hop.swarm.transport.message_signer(), relay)
}

/// Acceptance (origin miss, end to end). The forwarding hop announces an origin's session in a
/// frame its peer refuses, so the two ends of the link disagree: the hop believes the session
/// is announced, the peer never learned it. The next frame references the session; the peer
/// holds it, asks, is answered from the hop's table, and delivers it. Nothing is lost and
/// nobody asks the origin.
#[tokio::test]
async fn test_refused_announcement_is_repaired_by_the_next_reference() -> Result<()> {
    let hop = prepare_node(SecretKey::random()).await;
    let destination = prepare_node(SecretKey::random()).await;
    establish_admitted_link(&hop, &destination).await?;
    let origin = SessionSk::new_with_seckey(&SecretKey::random())?;
    let answered_before = session_announcement_count_for_test();

    // The hop's carrier signature covers the tampered sequence, the origin's does not: the hop
    // sends a well-formed frame whose transaction proof the peer must refuse.
    let mut refused = carried_by(&origin, &hop, destination.did(), 0, b"refused")?;
    refused.transaction.sequence = 1;
    let refused = MessagePayload::new(
        refused.transaction,
        hop.swarm.transport.message_signer(),
        refused.relay,
    )?;
    assert!(!refused.transaction.verify(TEST_NETWORK_ID));
    hop.swarm.transport.send_payload(refused).await?;

    let accepted = carried_by(&origin, &hop, destination.did(), 0, b"accepted")?;
    hop.swarm.transport.send_payload(accepted.clone()).await?;

    let delivered =
        next_custom_message_from(&destination, origin.account_did(), b"accepted").await?;
    assert_eq!(delivered.transaction, accepted.transaction);
    assert_eq!(
        session_announcement_count_for_test(),
        answered_before + 1,
        "the accepted frame must have referenced the origin session and been repaired once"
    );
    Ok(())
}

/// The relayed case the design turns on: a destination that never met the origin verifies the
/// origin's proof at no extra cost, because the last hop sends the origin's session inline the
/// first time it forwards for that origin and by reference afterwards. No question is asked
/// anywhere on the path.
#[tokio::test]
async fn test_relayed_origin_session_needs_no_round_trip() -> Result<()> {
    let origin = prepare_node(SecretKey::random()).await;
    let relay = prepare_node(SecretKey::random()).await;
    let destination = prepare_node(SecretKey::random()).await;
    establish_admitted_link(&origin, &relay).await?;
    establish_admitted_link(&relay, &destination).await?;
    let answered_before = session_announcement_count_for_test();

    for data in [b"first".as_slice(), b"second".as_slice()] {
        origin
            .swarm
            .transport
            .send_message_by_hop(Message::custom(data)?, destination.did(), relay.did())
            .await?;
        let delivered = next_custom_message_from(&destination, origin.did(), data).await?;
        assert_eq!(delivered.transaction.destination, destination.did());
        assert_eq!(delivered.signer(), relay.did());
        assert!(delivered.verify_transaction_and_payload(TEST_NETWORK_ID));
    }

    assert_eq!(session_announcement_count_for_test(), answered_before);
    Ok(())
}
