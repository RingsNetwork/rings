use std::str::FromStr;
use std::sync::Arc;

use tokio::time::sleep;
use tokio::time::Duration;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;

use super::prepare_node;
use crate::dht::vnode::VirtualNode;
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::err::Error;
use crate::err::Result;
use crate::message;
use crate::message::Encoder;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::Message;
use crate::message::PayloadSender;
use crate::prelude::vnode::VNodeOperation;
use crate::storage::PersistenceStorageOperation;
use crate::storage::PersistenceStorageReadAndWrite;
use crate::swarm::tests::new_swarm;
use crate::tests::manually_establish_connection;
use crate::transports::manager::TransportManager;
use crate::types::ice_transport::IceTransportInterface;

#[tokio::test]
async fn test_handle_join() -> Result<()> {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = Arc::new(new_swarm(key1).await?);
    let node2 = Arc::new(new_swarm(key2).await?);
    manually_establish_connection(&node1, &node2).await?;
    assert!(node1.listen_once().await.is_some());
    assert!(node1
        .dht()
        .lock_successor()?
        .list()
        .contains(&key2.address().into()));
    tokio::fs::remove_dir_all("./tmp").await.ok();
    Ok(())
}

#[tokio::test]
async fn test_handle_connect_node() -> Result<()> {
    let keys = gen_ordered_keys(3);
    let (key1, key2, key3) = (keys[0], keys[1], keys[2]);

    let (node1, _path1) = prepare_node(key1).await;
    let (node2, _path2) = prepare_node(key2).await;
    let (node3, _path3) = prepare_node(key3).await;

    // 2 to 3
    manually_establish_connection(&node3, &node2).await?;

    // 1 to 2
    manually_establish_connection(&node1, &node2).await?;

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
            println!("wait tranposrt 1 to 2 and transport 2 to 3 connected");
            sleep(Duration::from_millis(1)).await;
            let transport_1_to_2 = node1.get_transport(node2.did()).unwrap();
            transport_1_to_2.wait_for_data_channel_open().await.unwrap();
            let transport_2_to_3 = node2.get_transport(node3.did()).unwrap();
            transport_2_to_3.wait_for_data_channel_open().await.unwrap();

            println!("wait events trigger");
            sleep(Duration::from_millis(1)).await;

            println!("node1 key address: {:?}", node1.did());
            println!("node2 key address: {:?}", node2.did());
            println!("node3 key address: {:?}", node3.did());
            let dht1 = node1.dht();
            let dht2 = node2.dht();
            let dht3 = node3.dht();
            {
                let dht1_successor = dht1.lock_successor()?;
                let dht2_successor = dht2.lock_successor()?;
                let dht3_successor = dht3.lock_successor()?;
                println!("node1.dht() successor: {:?}", dht1_successor);
                println!("node2.dht() successor: {:?}", dht2_successor);
                println!("node3.dht() successor: {:?}", dht3_successor);

                assert!(
                    dht1_successor.list().contains(
                        &key2.address().into()
                    ),
                    "Expect node1.dht() successor is key2, Found: {:?}",
                    dht1_successor.list()
                );
                assert!(
                    dht2_successor.list().contains(
                        &key3.address().into()
                    ), "{:?}", dht2_successor.list());
                assert!(
                    dht3_successor.list().contains(
                        &key2.address().into()
                    ),
                    "node3.dht() successor is key2"
                );
            }

            assert_eq!(
                transport_1_to_2.ice_connection_state().await,
                Some(RTCIceConnectionState::Connected)
            );
            assert_eq!(
                transport_2_to_3.ice_connection_state().await,
                Some(RTCIceConnectionState::Connected)
            );

            // node1.dht() send msg to node2.dht() ask for connecting node3.dht()
            node1.connect(node3.did()).await.unwrap();
            sleep(Duration::from_millis(10000)).await;

            let transport_1_to_3 = node1.get_transport(node3.did());
            assert!(transport_1_to_3.is_some());
            let transport_1_to_3 = transport_1_to_3.unwrap();
            let both = {
                transport_1_to_3.ice_connection_state().await == Some(RTCIceConnectionState::New) ||
                    transport_1_to_3.ice_connection_state().await == Some(RTCIceConnectionState::Checking) ||
                    transport_1_to_3.ice_connection_state().await == Some(RTCIceConnectionState::Connected)
            };
            assert!(both, "{:?}", transport_1_to_3.ice_connection_state().await);
            transport_1_to_3.wait_for_data_channel_open().await.unwrap();
            assert_eq!(
                transport_1_to_3.ice_connection_state().await,
                Some(RTCIceConnectionState::Connected)
            );
            Ok::<(), Error>(())
        } => {}
    }
    tokio::fs::remove_dir_all("./tmp").await.ok();
    Ok(())
}

#[tokio::test]
async fn test_handle_notify_predecessor() -> Result<()> {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = Arc::new(new_swarm(key1).await?);
    let node2 = Arc::new(new_swarm(key2).await?);
    manually_establish_connection(&node1, &node2).await?;

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
            let transport_1_to_2 = node1.get_transport(node2.did()).unwrap();
            sleep(Duration::from_millis(1000)).await;
            transport_1_to_2.wait_for_data_channel_open().await.unwrap();
            assert!(node1.dht().lock_successor()?.list().contains(&key2.address().into()));
            assert!(node2.dht().lock_successor()?.list().contains(&key1.address().into()));
            assert_eq!(
                transport_1_to_2.ice_connection_state().await,
                Some(RTCIceConnectionState::Connected)
            );
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
            assert!(node1.dht().lock_successor()?.list().contains(&key2.address().into()));
            Ok::<(), Error>(())
        } => {}
    }

    tokio::fs::remove_dir_all("./tmp").await.ok();
    Ok(())
}

#[tokio::test]
async fn test_handle_find_successor_increase() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    if key1.address() > key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = Arc::new(new_swarm(key1).await?);
    let node2 = Arc::new(new_swarm(key2).await?);
    manually_establish_connection(&node1, &node2).await?;

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
            let transport_1_to_2 = node1.get_transport(node2.did()).unwrap();
            sleep(Duration::from_millis(1000)).await;
            transport_1_to_2.wait_for_data_channel_open().await.unwrap();
            assert!(node1.dht().lock_successor()?.list().contains(&key2.address().into()), "{:?}", node1.dht().lock_successor()?.list());
            assert!(node2.dht().lock_successor()?.list().contains(&key1.address().into()));
            assert_eq!(
                transport_1_to_2.ice_connection_state().await,
                Some(RTCIceConnectionState::Connected)
            );
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
            assert!(node1.dht().lock_successor()?.list().contains(&key2.address().into()));

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
            assert!(node2.dht().lock_successor()?.list().contains(&key1.address().into()));
            assert!(node1.dht().lock_successor()?.list().contains(&key2.address().into()));
            Ok::<(), Error>(())
        } => {}
    }
    tokio::fs::remove_dir_all("./tmp").await.ok();
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
    let node1 = Arc::new(new_swarm(key1).await?);
    let node2 = Arc::new(new_swarm(key2).await?);
    manually_establish_connection(&node1, &node2).await?;

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
            let transport_1_to_2 = node1.get_transport(node2.did()).unwrap();
            sleep(Duration::from_millis(1000)).await;
            transport_1_to_2.wait_for_data_channel_open().await.unwrap();
            assert!(node1.dht().lock_successor()?.list().contains(&key2.address().into()));
            assert!(node2.dht().lock_successor()?.list().contains(&key1.address().into()));
            assert!(node1.dht()
                .lock_finger()?
                .contains(Some(key2.address().into())));
            assert!(node2.dht()
                .lock_finger()?
                .contains(Some(key1.address().into())));
            assert_eq!(
                transport_1_to_2.ice_connection_state().await,
                Some(RTCIceConnectionState::Connected)
            );
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
            assert!(node1.dht().lock_successor()?.list().contains(&key2.address().into()));
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
            let dht1_successor = node1.dht().lock_successor()?.clone();
            let dht2_successor = node2.dht().lock_successor()?.clone();
            assert!(dht2_successor.list().contains(&key1.address().into()));
            assert!(dht1_successor.list().contains(&key2.address().into()));
            Ok::<(), Error>(())
        } => {}
    };
    tokio::fs::remove_dir_all("./tmp").await.ok();
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
    let node1 = Arc::new(new_swarm(key1).await?);
    let node2 = Arc::new(new_swarm(key2).await?);
    manually_establish_connection(&node1, &node2).await?;

    let n1 = node1.clone();
    let n2 = node2.clone();
    tokio::spawn(async move { n1.listen().await });
    tokio::spawn(async move { n2.listen().await });

    let transport_1_to_2 = node1.get_transport(node2.did()).unwrap();
    sleep(Duration::from_millis(1000)).await;
    transport_1_to_2.wait_for_data_channel_open().await.unwrap();
    // node1's successor is node2
    // node2's successor is node1
    assert!(node1
        .dht()
        .lock_successor()?
        .list()
        .contains(&key2.address().into()));
    assert!(node2
        .dht()
        .lock_successor()?
        .list()
        .contains(&key1.address().into()));
    assert_eq!(
        transport_1_to_2.ice_connection_state().await,
        Some(RTCIceConnectionState::Connected)
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
        .lock_successor()?
        .list()
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
    let data: Result<Option<VirtualNode>> = node2.dht().storage.get(&(vnode.did)).await;
    assert!(data.is_ok(), "vnode: {:?} not in", vnode.did);
    let data = data.unwrap().unwrap();
    assert_eq!(data.data[0].clone().decode::<String>().unwrap(), message);
    tokio::fs::remove_dir_all("./tmp").await.ok();
    Ok(())
}
