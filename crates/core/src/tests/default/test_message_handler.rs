use std::str::FromStr;

use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::time::sleep;
use tokio::time::Duration;

use crate::dht::successor::SuccessorReader;
use crate::dht::vnode::VirtualNode;
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message;
use crate::message::Encoder;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::Message;
use crate::message::PayloadSender;
use crate::prelude::vnode::VNodeOperation;
use crate::tests::default::prepare_node;
use crate::tests::manually_establish_connection;

#[tokio::test]
async fn test_handle_join() -> Result<()> {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1, &node2).await;
    assert!(node1.listen_once().await.is_some());
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));
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
    manually_establish_connection(&node3, &node2).await;

    // 1 to 2
    manually_establish_connection(&node1, &node2).await;

    sleep(Duration::from_secs(3)).await;

    tokio::select! {
        _ = async {
            futures::join!(
                async { node1.clone().listen().await },
                async { node2.clone().listen().await },
                async { node3.clone().listen().await },
            )
        } => {unreachable!();}
        _ = async {
            // handle join dht situation
            println!("wait connection 1 to 2 and connection 2 to 3 connected");
            sleep(Duration::from_millis(1)).await;
            let connection_1_to_2 = node1.get_connection(node2.did()).unwrap();
            let connection_2_to_3 = node2.get_connection(node3.did()).unwrap();

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
                println!("node1.dht() successor: {:?}", dht1_successor);
                println!("node2.dht() successor: {:?}", dht2_successor);
                println!("node3.dht() successor: {:?}", dht3_successor);

                assert!(
                    dht1_successor.list()?.contains(
                        &key2.address().into()
                    ),
                    "Expect node1.dht() successor is key2, Found: {:?}",
                    dht1_successor.list()?
                );
                assert!(
                    dht2_successor.list()?.contains(
                        &key3.address().into()
                    ), "{:?}", dht2_successor.list());
                assert!(
                    dht3_successor.list()?.contains(
                        &key2.address().into()
                    ),
                    "node3.dht() successor is key2"
                );
            }

            assert_eq!(
                connection_1_to_2.ice_connection_state(),
                WebrtcConnectionState::Connected,
            );
            assert_eq!(
                connection_2_to_3.ice_connection_state(),
                WebrtcConnectionState::Connected,
            );

            // node1.dht() send msg to node2.dht() ask for connecting node3.dht()
            node1.connect(node3.did()).await.unwrap();
            sleep(Duration::from_millis(10000)).await;

            let connection_1_to_3 = node1.get_connection(node3.did());
            assert!(connection_1_to_3.is_some());
            let connection_1_to_3 = connection_1_to_3.unwrap();
            let both = {
                connection_1_to_3.ice_connection_state() == WebrtcConnectionState::New ||
                    connection_1_to_3.ice_connection_state() == WebrtcConnectionState::Connecting ||
                    connection_1_to_3.ice_connection_state() == WebrtcConnectionState::Connected
            };
            assert!(both, "{:?}", connection_1_to_3.ice_connection_state());
            assert_eq!(
                connection_1_to_3.ice_connection_state(),
                WebrtcConnectionState::Connected
            );
            Ok::<(), Error>(())
        } => {}
    }
    Ok(())
}

#[tokio::test]
async fn test_handle_notify_predecessor() -> Result<()> {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1, &node2).await;

    // handle join dht situation
    tokio::select! {
        _ = async {
            futures::join!(
                async {
                    loop {
                        node1.clone().listen().await;
                    }
                },
                async {
                    loop {
                        node2.clone().listen().await;
                    }
                }
            );
        } => { unreachable!();}
        _ = async {
            let connection_1_to_2 = node1.get_connection(node2.did()).unwrap();
            sleep(Duration::from_millis(1000)).await;
            assert!(node1.dht().successors().list()?.contains(&key2.address().into()));
            assert!(node2.dht().successors().list()?.contains(&key1.address().into()));
            assert_eq!(connection_1_to_2.ice_connection_state(), WebrtcConnectionState::Connected);
            node1
                .send_message(
                    Message::NotifyPredecessorSend(message::NotifyPredecessorSend {
                        did: key1.address().into(),
                    }),
                    node2.did(),
                )
                .await
                .unwrap();
            sleep(Duration::from_millis(1000)).await;
            assert_eq!(*node2.dht().lock_predecessor()?, Some(key1.address().into()));
            assert!(node1.dht().successors().list()?.contains(&key2.address().into()));
            Ok::<(), Error>(())
        } => {}
    }

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
    manually_establish_connection(&node1, &node2).await;

    tokio::select! {
        _ = async {
            futures::join!(
                async {
                    loop {
                        node1.clone().listen().await;
                    }
                },
                async {
                    loop {
                        node2.clone().listen().await;
                    }
                }
            );
        } => { unreachable!();}
        _ = async {
            let connection_1_to_2 = node1.get_connection(node2.did()).unwrap();
            sleep(Duration::from_millis(1000)).await;
            assert!(node1.dht().successors().list()?.contains(&key2.address().into()), "{:?}", node1.dht().successors().list()?);
            assert!(node2.dht().successors().list()?.contains(&key1.address().into()));
            assert_eq!(connection_1_to_2.ice_connection_state(), WebrtcConnectionState::Connected);
            node1
                .send_message(
                    Message::NotifyPredecessorSend(message::NotifyPredecessorSend {
                        did: node1.did(),
                    }),
                    node2.did(),
                )
                .await
                .unwrap();
            sleep(Duration::from_millis(1000)).await;
            assert_eq!(*node2.dht().lock_predecessor()?, Some(key1.address().into()));
            assert!(node1.dht().successors().list()?.contains(&key2.address().into()));

            println!(
                "node1: {:?}, node2: {:?}",
                node1.did(),
                node2.did()
            );
            node2
                .send_message(
                    Message::FindSuccessorSend(message::FindSuccessorSend {
                        did: node2.did(),
                        then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                        strict: true
                    }),
                    node1.did(),
                )
                .await
                .unwrap();
            sleep(Duration::from_millis(1000)).await;
            assert!(node2.dht().successors().list()?.contains(&key1.address().into()));
            assert!(node1.dht().successors().list()?.contains(&key2.address().into()));
            Ok::<(), Error>(())
        } => {}
    }
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
    manually_establish_connection(&node1, &node2).await;

    // handle join dht situation
    tokio::select! {
        _ = async {
            futures::join!(
                async {
                    loop {
                        node1.clone().listen().await;
                    }
                },
                async {
                    loop {
                        node2.clone().listen().await;
                    }
                }
            );
        } => {unreachable!();}
        _ = async {
            let connection_1_to_2 = node1.get_connection(node2.did()).unwrap();
            sleep(Duration::from_millis(1000)).await;
            assert!(node1.dht().successors().list()?.contains(&key2.address().into()));
            assert!(node2.dht().successors().list()?.contains(&key1.address().into()));
            assert!(node1.dht()
                .lock_finger()?
                .contains(Some(key2.address().into())));
            assert!(node2.dht()
                .lock_finger()?
                .contains(Some(key1.address().into())));
            assert_eq!(connection_1_to_2.ice_connection_state(), WebrtcConnectionState::Connected);
            node1
                .send_message(
                    Message::NotifyPredecessorSend(message::NotifyPredecessorSend {
                        did: node1.did(),
                    }),
                    node2.did(),
                )
                .await
                .unwrap();
            sleep(Duration::from_millis(1000)).await;
            assert_eq!(*node2.dht().lock_predecessor()?, Some(key1.address().into()));
            assert!(node1.dht().successors().list()?.contains(&key2.address().into()));
            println!(
                "node1: {:?}, node2: {:?}",
                node1.did(),
                node2.did()
            );
            node2
                .send_message(
                    Message::FindSuccessorSend(message::FindSuccessorSend {
                        did: node2.did(),
                        then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                        strict: true
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
            Ok::<(), Error>(())
        } => {}
    };
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
    manually_establish_connection(&node1, &node2).await;

    let n1 = node1.clone();
    let n2 = node2.clone();
    tokio::spawn(async move { n1.listen().await });
    tokio::spawn(async move { n2.listen().await });

    let connection_1_to_2 = node1.get_connection(node2.did()).unwrap();
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
        connection_1_to_2.ice_connection_state(),
        WebrtcConnectionState::Connected
    );
    node1
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
    // the vid is hash of string
    let vnode: VirtualNode = (message.clone(), encoded_message).try_into().unwrap();
    node1
        .send_message(
            Message::OperateVNode(VNodeOperation::Overwrite(vnode.clone())),
            node2.did(),
        )
        .await
        .unwrap();
    sleep(Duration::from_millis(5000)).await;
    assert!(node1.dht().storage.count().await.unwrap() == 0);
    assert!(node2.dht().storage.count().await.unwrap() > 0);
    let data: Result<Option<VirtualNode>> = node2.dht().storage.get(&vnode.did.to_string()).await;
    assert!(data.is_ok(), "vnode: {:?} not in", vnode.did);
    let data = data.unwrap().unwrap();
    assert_eq!(data.data[0].clone().decode::<String>().unwrap(), message);
    Ok(())
}
