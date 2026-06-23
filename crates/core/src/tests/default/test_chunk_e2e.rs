//! End-to-end chunking over the dummy backend: by overriding the negotiated `max_message_size`
//! (via the dummy test hook) we force `do_send_payload` down the real chunked path and verify the
//! receiver reassembles the original message — exercising stream → wrap → send → reassemble, not
//! just the pure `WireReserves::plan` decision.

use rings_transport::connections::dummy_controlled;

use crate::ecc::SecretKey;
use crate::message::Message;
use crate::tests::default::prepare_node;
use crate::tests::manually_establish_connection;

/// Read inbound messages on `node` until a `CustomMessage` arrives (skipping DHT bookkeeping), or
/// give up after a bounded number of messages.
async fn recv_custom(node: &crate::tests::default::Node) -> Option<Vec<u8>> {
    for _ in 0..64 {
        let payload = node.listen_once().await?;
        if let Ok(Message::CustomMessage(cm)) = payload.transaction.data() {
            return Some(cm.0);
        }
    }
    None
}

#[tokio::test]
async fn large_message_is_chunked_and_reassembled() {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    // Force a small negotiated limit so the payload below must be chunked. Set it *after* the
    // handshake so the connect offer/answer themselves are unaffected.
    dummy_controlled::set_max_message_size(8192);

    // Comfortably larger than the negotiated limit → many chunks.
    let big: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
    node1
        .swarm
        .send_message(Message::custom(&big).unwrap(), node2.did())
        .await
        .expect("send should succeed and chunk");

    let got = recv_custom(&node2)
        .await
        .expect("reassembled custom message");
    assert_eq!(
        got, big,
        "receiver must reassemble the exact original payload"
    );

    dummy_controlled::set_max_message_size(0);
}

#[tokio::test]
async fn negotiated_size_too_small_errors_without_partial_send() {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    // Below `chunk_overhead + MIN_CHUNK_DATA`: no usable chunk size exists, so framing must reject
    // *before* any chunk is sent (the `None` is returned ahead of the send loop).
    dummy_controlled::set_max_message_size(5000);

    let big: Vec<u8> = vec![0xab; 10_000];
    // Count data-channel sends from here, to prove the failed send enqueues nothing.
    dummy_controlled::reset_sent_count();
    let err = node1
        .swarm
        .send_message(Message::custom(&big).unwrap(), node2.did())
        .await
        .expect_err("an unusably small negotiated size must fail the send");
    assert!(
        matches!(err, crate::error::Error::PeerMaxMessageSizeTooSmall(_)),
        "expected PeerMaxMessageSizeTooSmall, got {err:?}"
    );
    assert_eq!(
        dummy_controlled::sent_count(),
        0,
        "no chunk (partial or otherwise) must be dispatched when framing rejects the size"
    );

    dummy_controlled::set_max_message_size(0);
}
