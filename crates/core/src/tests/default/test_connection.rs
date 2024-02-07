use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::WebrtcConnectionState;

use crate::ecc::SecretKey;
use crate::error::Result;
use crate::message::handlers::tests::assert_no_more_msg;
use crate::message::handlers::tests::wait_for_msgs;
use crate::tests::default::prepare_node;
use crate::tests::manually_establish_connection;

#[tokio::test]
async fn test_handshake_on_both_sides() -> Result<()> {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let key3 = SecretKey::random();

    let swarm1 = prepare_node(key1).await;
    let swarm2 = prepare_node(key2).await;
    let swarm3 = prepare_node(key3).await;

    assert!(swarm1.get_connection(swarm2.did()).is_none());
    assert!(swarm2.get_connection(swarm1.did()).is_none());

    // connect to middle peer
    manually_establish_connection(&swarm1, &swarm3).await;
    manually_establish_connection(&swarm2, &swarm3).await;
    wait_for_msgs(&swarm1, &swarm2, &swarm3).await;
    assert_no_more_msg(&swarm1, &swarm2, &swarm3).await;

    assert_eq!(
        swarm3
            .get_connection(swarm1.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );
    assert_eq!(
        swarm3
            .get_connection(swarm2.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );

    assert_eq!(
        swarm1
            .get_connection(swarm3.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );
    assert_eq!(
        swarm2
            .get_connection(swarm3.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );

    // connect to each at same time
    // Node 1 -> Offer -> Node 2
    // Node 2 -> Offer -> Node 1
    swarm1.connect(swarm2.did()).await.unwrap();
    swarm2.connect(swarm1.did()).await.unwrap();

    // The conn state of swarm1 -> swarm2 is new
    assert_eq!(
        swarm1
            .get_connection(swarm2.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::New,
    );
    // The conn state of swarm2 -> swarm1 is new
    assert_eq!(
        swarm2
            .get_connection(swarm1.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::New,
    );

    wait_for_msgs(&swarm1, &swarm2, &swarm3).await;
    assert_no_more_msg(&swarm1, &swarm2, &swarm3).await;

    // When node 1 got offer, node 1 may accept offer if did 1 < did 2, drop local connection
    // and response answer
    // When node 2 got offer, node 2 reject offer if did 1 < did 2

    assert_eq!(
        swarm1
            .get_connection(swarm2.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected,
    );

    assert_eq!(
        swarm2
            .get_connection(swarm1.did())
            .unwrap()
            .webrtc_connection_state(),
        WebrtcConnectionState::Connected,
    );

    Ok(())
}
