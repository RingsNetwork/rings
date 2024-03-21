use rings_transport::core::transport::WebrtcConnectionState;

use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::manually_establish_connection;

#[tokio::test]
#[ignore]
async fn test_handshake_on_both_sides_ordered() {
    let keys = gen_ordered_keys(3);
    let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
    test_handshake_on_both_sides(key1, key2, key3).await
}

#[tokio::test]
#[ignore]
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
    wait_for_msgs(&node1, &node2, &node3).await;
    assert_no_more_msg(&node1, &node2, &node3).await;

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

    // connect to each at same time
    // Node 1 -> Offer -> Node 2
    // Node 2 -> Offer -> Node 1
    node1.swarm.connect(node2.did()).await.unwrap();
    node2.swarm.connect(node1.did()).await.unwrap();

    // The conn state of swarm1 -> swarm2 is new
    assert_eq!(
        node1
            .swarm
            .transport
            .get_connection(node2.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::New,
    );
    // The conn state of swarm2 -> swarm1 is new
    assert_eq!(
        node2
            .swarm
            .transport
            .get_connection(node1.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::New,
    );

    wait_for_msgs(&node1, &node2, &node3).await;
    assert_no_more_msg(&node1, &node2, &node3).await;

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
