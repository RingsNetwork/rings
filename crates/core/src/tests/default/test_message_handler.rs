use std::str::FromStr;
#[cfg(feature = "dummy")]
use std::sync::Arc;

#[cfg(feature = "dummy")]
use rings_transport::connections::dummy_controlled;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::time::sleep;
#[cfg(feature = "dummy")]
use tokio::time::timeout;
use tokio::time::Duration;

use crate::dht::entry::Entry;
use crate::dht::successor::SuccessorReader;
#[cfg(feature = "dummy")]
use crate::dht::PeerRingAction;
#[cfg(feature = "dummy")]
use crate::dht::PeerRingRemoteAction;
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::message;
use crate::message::Encoder;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::Message;
#[cfg(feature = "dummy")]
use crate::message::MessageHandler;
use crate::prelude::entry::EntryOperation;
#[cfg(feature = "dummy")]
use crate::swarm::callback::SwarmCallback;
use crate::tests::default::prepare_node;
use crate::tests::manually_establish_connection;

#[cfg(feature = "dummy")]
struct NoopCallback;

#[cfg(feature = "dummy")]
impl SwarmCallback for NoopCallback {}

#[tokio::test]
async fn test_handle_join() -> Result<()> {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    assert!(node1.listen_once().await.is_some());
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_join_dht_keeps_local_join_when_convergence_send_fails() -> Result<()> {
    dummy_controlled::enable(true);
    dummy_controlled::set_max_message_size(0);

    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    assert!(
        node1.dht().successors().list()?.is_empty(),
        "controlled delivery should prevent automatic DataChannelOpen join"
    );

    dummy_controlled::enable(false);
    dummy_controlled::set_max_message_size(1);

    let handler = MessageHandler::new(node1.swarm.transport.clone(), Arc::new(NoopCallback));
    let join_result = handler.join_dht(node2.did()).await;

    dummy_controlled::set_max_message_size(0);

    assert!(
        join_result.is_ok(),
        "join must not fail when follow-up convergence sends fail: {join_result:?}"
    );
    assert!(node1.dht().successors().list()?.contains(&node2.did()));

    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_handle_dht_notify_remote_action_sends_predecessor_to_target() -> Result<()> {
    dummy_controlled::enable(true);

    let keys = gen_ordered_keys(3);
    let node1 = prepare_node(keys[0]).await;
    let node2 = prepare_node(keys[1]).await;
    let node3 = prepare_node(keys[2]).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    // Clear queued connection-open callbacks so this test exercises only the
    // explicit handler action below, not automatic join/stabilization traffic.
    dummy_controlled::enable(false);

    let handler = MessageHandler::new(node1.swarm.transport.clone(), Arc::new(NoopCallback));
    handler
        .handle_dht_events(&PeerRingAction::RemoteAction(
            node2.did(),
            PeerRingRemoteAction::Notify(node3.did()),
        ))
        .await?;

    let payload = timeout(Duration::from_secs(1), node2.listen_once())
        .await
        .expect("notify target should receive a message")
        .expect("notify target message stream should stay open");

    assert_eq!(payload.transaction.destination, node2.did());
    match payload.transaction.data::<Message>()? {
        Message::NotifyPredecessorSend(message::NotifyPredecessorSend { did }) => {
            assert_eq!(did, node3.did());
        }
        other => panic!("expected NotifyPredecessorSend, got {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn test_handle_connect_node() -> Result<()> {
    let keys = gen_ordered_keys(3);
    let (key1, key2, key3) = (keys[0], keys[1], keys[2]);

    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    let node3 = prepare_node(key3).await;

    // 2 to 3
    manually_establish_connection(&node3.swarm, &node2.swarm).await;

    // 1 to 2
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    sleep(Duration::from_secs(3)).await;

    // handle join dht situation
    println!("wait connection 1 to 2 and connection 2 to 3 connected");
    sleep(Duration::from_millis(1)).await;
    let connection_1_to_2 = node1.swarm.transport.get_connection(node2.did()).unwrap();
    let connection_2_to_3 = node2.swarm.transport.get_connection(node3.did()).unwrap();

    println!("wait events trigger");
    sleep(Duration::from_millis(1)).await;

    println!("node1 key address: {:?}", node1.did());
    println!("node2 key address: {:?}", node2.did());
    println!("node3 key address: {:?}", node3.did());
    let dht1 = node1.dht();
    let dht2 = node2.dht();
    let dht3 = node3.dht();
    {
        let dht1_successor = dht1.successors();
        let dht2_successor = dht2.successors();
        let dht3_successor = dht3.successors();
        println!("node1.dht() successor: {dht1_successor:?}");
        println!("node2.dht() successor: {dht2_successor:?}");
        println!("node3.dht() successor: {dht3_successor:?}");

        assert!(
            dht1_successor.list()?.contains(&key2.address().into()),
            "Expect node1.dht() successor is key2, Found: {:?}",
            dht1_successor.list()?
        );
        assert!(
            dht2_successor.list()?.contains(&key3.address().into()),
            "{:?}",
            dht2_successor.list()
        );
        assert!(
            dht3_successor.list()?.contains(&key2.address().into()),
            "node3.dht() successor is key2"
        );
    }

    assert_eq!(
        connection_1_to_2.webrtc_connection_state(),
        WebrtcConnectionState::Connected,
    );
    assert_eq!(
        connection_2_to_3.webrtc_connection_state(),
        WebrtcConnectionState::Connected,
    );

    // node1 may already have connected node3 while syncing successor-list
    // candidates. If not, ask DHT to connect it through node2.
    if node1.swarm.transport.get_connection(node3.did()).is_none() {
        node1.swarm.connect(node3.did()).await.unwrap();
    }
    sleep(Duration::from_millis(10000)).await;

    let connection_1_to_3 = node1.swarm.transport.get_connection(node3.did());
    assert!(connection_1_to_3.is_some());
    let connection_1_to_3 = connection_1_to_3.unwrap();
    let both = {
        connection_1_to_3.webrtc_connection_state() == WebrtcConnectionState::New
            || connection_1_to_3.webrtc_connection_state() == WebrtcConnectionState::Connecting
            || connection_1_to_3.webrtc_connection_state() == WebrtcConnectionState::Connected
    };
    assert!(both, "{:?}", connection_1_to_3.webrtc_connection_state());
    assert_eq!(
        connection_1_to_3.webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );

    Ok(())
}

#[tokio::test]
async fn test_handle_notify_predecessor() -> Result<()> {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    let connection_1_to_2 = node1.swarm.transport.get_connection(node2.did()).unwrap();
    sleep(Duration::from_millis(1000)).await;
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));
    assert!(node2
        .dht()
        .successors()
        .list()?
        .contains(&key1.address().into()));
    assert_eq!(
        connection_1_to_2.webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );
    node1
        .swarm
        .send_message(
            Message::NotifyPredecessorSend(message::NotifyPredecessorSend {
                did: key1.address().into(),
            }),
            node2.did(),
        )
        .await
        .unwrap();
    sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        *node2.dht().lock_predecessor()?,
        Some(key1.address().into())
    );
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));

    Ok(())
}

#[tokio::test]
async fn test_handle_find_successor_increase() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    if key1.address() > key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    let connection_1_to_2 = node1.swarm.transport.get_connection(node2.did()).unwrap();
    sleep(Duration::from_millis(1000)).await;
    assert!(
        node1
            .dht()
            .successors()
            .list()?
            .contains(&key2.address().into()),
        "{:?}",
        node1.dht().successors().list()?
    );
    assert!(node2
        .dht()
        .successors()
        .list()?
        .contains(&key1.address().into()));
    assert_eq!(
        connection_1_to_2.webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );
    node1
        .swarm
        .send_message(
            Message::NotifyPredecessorSend(message::NotifyPredecessorSend { did: node1.did() }),
            node2.did(),
        )
        .await
        .unwrap();
    sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        *node2.dht().lock_predecessor()?,
        Some(key1.address().into())
    );
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));

    println!("node1: {:?}, node2: {:?}", node1.did(), node2.did());
    node2
        .swarm
        .send_message(
            Message::FindSuccessorSend(message::FindSuccessorSend {
                did: node2.did(),
                then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                strict: true,
            }),
            node1.did(),
        )
        .await
        .unwrap();
    sleep(Duration::from_millis(1000)).await;
    assert!(node2
        .dht()
        .successors()
        .list()?
        .contains(&key1.address().into()));
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));

    Ok(())
}

#[tokio::test]
async fn test_handle_find_successor_decrease() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    // key 2 > key 1 here
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    let connection_1_to_2 = node1.swarm.transport.get_connection(node2.did()).unwrap();
    sleep(Duration::from_millis(1000)).await;
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));
    assert!(node2
        .dht()
        .successors()
        .list()?
        .contains(&key1.address().into()));
    assert!(node1
        .dht()
        .lock_finger()?
        .contains(Some(key2.address().into())));
    assert!(node2
        .dht()
        .lock_finger()?
        .contains(Some(key1.address().into())));
    assert_eq!(
        connection_1_to_2.webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );
    node1
        .swarm
        .send_message(
            Message::NotifyPredecessorSend(message::NotifyPredecessorSend { did: node1.did() }),
            node2.did(),
        )
        .await
        .unwrap();
    sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        *node2.dht().lock_predecessor()?,
        Some(key1.address().into())
    );
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));
    println!("node1: {:?}, node2: {:?}", node1.did(), node2.did());
    node2
        .swarm
        .send_message(
            Message::FindSuccessorSend(message::FindSuccessorSend {
                did: node2.did(),
                then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                strict: true,
            }),
            node1.did(),
        )
        .await
        .unwrap();
    sleep(Duration::from_millis(1000)).await;
    let dht1_successor = node1.dht().successors();
    let dht2_successor = node2.dht().successors();
    assert!(dht2_successor.list()?.contains(&key1.address().into()));
    assert!(dht1_successor.list()?.contains(&key2.address().into()));

    Ok(())
}

#[tokio::test]
async fn test_handle_storage() -> Result<()> {
    // random key may failed here, because if key1 is more close to virtual_peer
    // key2 will try send msg back to key1
    let key1 =
        SecretKey::from_str("ff3e0ea83de6909db79f3452764a24efb25c86c1e85c7c453d903c0cf462df07")
            .unwrap();
    let key2 =
        SecretKey::from_str("f782f6b07ae0151b5f83ff49f46087a7a45eb5c97d210c907a2b52ffece4be69")
            .unwrap();
    println!(
        "test with key1: {:?}, key2: {:?}",
        key1.address(),
        key2.address()
    );
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    let connection_1_to_2 = node1.swarm.transport.get_connection(node2.did()).unwrap();
    sleep(Duration::from_millis(1000)).await;
    // node1's successor is node2
    // node2's successor is node1
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));
    assert!(node2
        .dht()
        .successors()
        .list()?
        .contains(&key1.address().into()));
    assert_eq!(
        connection_1_to_2.webrtc_connection_state(),
        WebrtcConnectionState::Connected
    );
    node1
        .swarm
        .send_message(
            Message::NotifyPredecessorSend(message::NotifyPredecessorSend { did: node1.did() }),
            node2.did(),
        )
        .await
        .unwrap();
    sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        *node2.dht().lock_predecessor()?,
        Some(key1.address().into())
    );
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));

    assert!(node2.dht().storage.count().await.unwrap() == 0);
    let message = String::from("this is a test string");
    let encoded_message = message.encode().unwrap();
    // the entry_key is hash of string
    let entry: Entry = (message.clone(), encoded_message).try_into().unwrap();
    node1
        .swarm
        .send_message(
            Message::OperateEntry(EntryOperation::Overwrite(entry.clone())),
            node2.did(),
        )
        .await
        .unwrap();
    sleep(Duration::from_millis(5000)).await;
    assert!(node1.dht().storage.count().await.unwrap() == 0);
    assert!(node2.dht().storage.count().await.unwrap() > 0);
    let data: Result<Option<Entry>> = node2.dht().storage.get(&entry.did.to_string()).await;
    assert!(data.is_ok(), "entry: {:?} not in", entry.did);
    let data = data.unwrap().unwrap();
    assert_eq!(data.data[0].clone().decode::<String>().unwrap(), message);
    Ok(())
}
