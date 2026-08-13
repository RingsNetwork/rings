#[cfg(feature = "dummy")]
use rings_transport::connections::dummy_controlled;
use rings_transport::core::transport::WebrtcConnectionState;

#[cfg(feature = "dummy")]
use crate::dht::successor::SuccessorReader;
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::manually_establish_connection;

#[tokio::test]
async fn test_handshake_on_both_sides_ordered() {
    let keys = gen_ordered_keys(3);
    let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
    test_handshake_on_both_sides(key1, key2, key3).await
}

#[tokio::test]
async fn test_handshake_on_both_sides_desc_ordered() {
    let keys = gen_ordered_keys(3);
    let (key3, key2, key1) = (keys[0], keys[1], keys[2]);
    test_handshake_on_both_sides(key1, key2, key3).await
}

async fn test_handshake_on_both_sides(key1: SecretKey, key2: SecretKey, key3: SecretKey) {
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    let node3 = prepare_node(key3).await;

    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
    assert!(node2.swarm.transport.get_connection(node1.did()).is_none());

    // connect to middle peer
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    manually_establish_connection(&node2.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    assert_no_more_msg([&node1, &node2, &node3]).await;

    assert_eq!(
        node3
            .swarm
            .transport
            .get_connection(node1.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );
    assert_eq!(
        node3
            .swarm
            .transport
            .get_connection(node2.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );

    assert_eq!(
        node1
            .swarm
            .transport
            .get_connection(node3.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );
    assert_eq!(
        node2
            .swarm
            .transport
            .get_connection(node3.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );

    let direct_connection_already_synced =
        node1.swarm.transport.get_connection(node2.did()).is_some()
            && node2.swarm.transport.get_connection(node1.did()).is_some();

    // connect to each at same time
    // Node 1 -> Offer -> Node 2
    // Node 2 -> Offer -> Node 1
    _ = node1.swarm.connect(node2.did()).await;
    _ = node2.swarm.connect(node1.did()).await;

    if direct_connection_already_synced {
        assert_eq!(node1.swarm.transport.pending_connection_count().unwrap(), 0);
        assert_eq!(node2.swarm.transport.pending_connection_count().unwrap(), 0);
    } else {
        // Both offers exist but neither handshake has been admitted. Pending
        // peers must remain invisible to the public connection view.
        assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
        assert!(node2.swarm.transport.get_connection(node1.did()).is_none());
        assert_eq!(node1.swarm.transport.pending_connection_count().unwrap(), 1);
        assert_eq!(node2.swarm.transport.pending_connection_count().unwrap(), 1);
    }

    wait_for_msgs([&node1, &node2, &node3]).await;
    assert_no_more_msg([&node1, &node2, &node3]).await;

    // When node 1 got offer, node 1 may accept offer if did 1 < did 2, drop local connection
    // and response answer
    // When node 2 got offer, node 2 reject offer if did 1 < did 2

    assert_eq!(
        node1
            .swarm
            .transport
            .get_connection(node2.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected,
    );

    assert_eq!(
        node2
            .swarm
            .transport
            .get_connection(node1.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected,
    )
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn dummy_mismatched_data_channel_open_does_not_admit_peer() {
    dummy_controlled::enable(true);

    let keys = gen_ordered_keys(2);
    let node1 = prepare_node(keys[0]).await;
    let node2 = prepare_node(keys[1]).await;

    let offer = node1.swarm.create_offer(node2.did()).await.unwrap();
    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
    assert_eq!(node1.swarm.transport.pending_connection_count().unwrap(), 1);

    let answer = node2.swarm.answer_offer(offer).await.unwrap();
    node1.swarm.accept_answer(answer).await.unwrap();

    assert!(
        dummy_controlled::deliver_next_data_channel_open_with_cid(node1.did().to_string()).await
    );

    assert_eq!(node1.swarm.transport.pending_connection_count().unwrap(), 0);
    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
    assert!(node1.swarm.transport.get_connection(node1.did()).is_none());
    assert!(!node1
        .dht()
        .successors()
        .list()
        .unwrap()
        .contains(&node2.did()));

    _ = node1.swarm.disconnect(node2.did()).await;
    _ = node2.swarm.disconnect(node1.did()).await;
    dummy_controlled::enable(false);
}
