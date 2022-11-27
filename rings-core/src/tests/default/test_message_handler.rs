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
use crate::storage::PersistenceStorageOperation;
use crate::storage::PersistenceStorageReadAndWrite;
use crate::swarm::tests::new_swarm;
use crate::tests::manually_establish_connection;
use crate::transports::manager::TransportManager;
use crate::types::ice_transport::IceTransportInterface;
use crate::types::message::MessageListener;

#[tokio::test]
async fn test_handle_join() -> Result<()> {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let swarm1 = Arc::new(new_swarm(key1).await?);
    let swarm2 = Arc::new(new_swarm(key2).await?);
    manually_establish_connection(&swarm1, &swarm2).await?;
    let handle1 = swarm1.create_message_handler(None, None);
    let payload = swarm1.poll_message().await.unwrap();
    match handle1.handle_payload(&payload).await {
        Ok(_) => assert_eq!(true, true),
        Err(e) => {
            println!("{:?}", e);
            assert_eq!(true, false);
        }
    };
    assert!(swarm1
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

    let (_did1, dht1, swarm1, handler1, _path1) = prepare_node(key1).await;
    let (_did2, dht2, swarm2, handler2, _path2) = prepare_node(key2).await;
    let (_did3, dht3, swarm3, handler3, _path3) = prepare_node(key3).await;

    // 2 to 3
    manually_establish_connection(&swarm3, &swarm2).await?;

    // 1 to 2
    manually_establish_connection(&swarm1, &swarm2).await?;

    sleep(Duration::from_secs(3)).await;

    tokio::select! {
        _ = async {
            futures::join!(
                async { Arc::new(handler1.clone()).listen().await },
                async { Arc::new(handler2.clone()).listen().await },
                async { Arc::new(handler3.clone()).listen().await },
            )
        } => {unreachable!();}
        _ = async {
            // handle join dht situation
            println!("wait tranposrt 1 to 2 and transport 2 to 3 connected");
            sleep(Duration::from_millis(1)).await;
            let transport_1_to_2 = swarm1.get_transport(swarm2.did()).unwrap();
            transport_1_to_2.wait_for_data_channel_open().await.unwrap();
            let transport_2_to_3 = swarm2.get_transport(swarm3.did()).unwrap();
            transport_2_to_3.wait_for_data_channel_open().await.unwrap();

            println!("wait events trigger");
            sleep(Duration::from_millis(1)).await;

            println!("swarm1 key address: {:?}", swarm1.did());
            println!("swarm2 key address: {:?}", swarm2.did());
            println!("swarm3 key address: {:?}", swarm3.did());
            {
                let dht1_successor = dht1.lock_successor()?;
                let dht2_successor = dht2.lock_successor()?;
                let dht3_successor = dht3.lock_successor()?;
                println!("dht1 successor: {:?}", dht1_successor);
                println!("dht2 successor: {:?}", dht2_successor);
                println!("dht3 successor: {:?}", dht3_successor);

                assert!(
                    dht1_successor.list().contains(
                        &key2.address().into()
                    ),
                    "Expect dht1 successor is key2, Found: {:?}",
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
                    "dht3 successor is key2"
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

            // dht1 send msg to dht2 ask for connecting dht3
            swarm1.connect(swarm3.did()).await.unwrap();
            sleep(Duration::from_millis(10000)).await;

            let transport_1_to_3 = swarm1.get_transport(swarm3.did());
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
    let swarm1 = Arc::new(new_swarm(key1).await?);
    let swarm2 = Arc::new(new_swarm(key2).await?);
    manually_establish_connection(&swarm1, &swarm2).await?;
    let handler1 = swarm1.create_message_handler(None, None);
    let handler2 = swarm2.create_message_handler(None, None);

    // handle join dht situation
    tokio::select! {
        _ = async {
            futures::join!(
                async {
                    loop {
                        Arc::new(handler1.clone()).listen().await;
                    }
                },
                async {
                    loop {
                        Arc::new(handler2.clone()).listen().await;
                    }
                }
            );
        } => { unreachable!();}
        _ = async {
            let transport_1_to_2 = swarm1.get_transport(swarm2.did()).unwrap();
            sleep(Duration::from_millis(1000)).await;
            transport_1_to_2.wait_for_data_channel_open().await.unwrap();
            assert!(swarm1.dht().lock_successor()?.list().contains(&key2.address().into()));
            assert!(swarm2.dht().lock_successor()?.list().contains(&key1.address().into()));
            assert_eq!(
                transport_1_to_2.ice_connection_state().await,
                Some(RTCIceConnectionState::Connected)
            );
            handler1
                .send_message(
                    Message::NotifyPredecessorSend(message::NotifyPredecessorSend {
                        did: key1.address().into(),
                    }),
                    swarm2.did(),
                    swarm2.did(),
                )
                .await
                .unwrap();
            sleep(Duration::from_millis(1000)).await;
            assert_eq!(*swarm2.dht().lock_predecessor()?, Some(key1.address().into()));
            assert!(swarm1.dht().lock_successor()?.list().contains(&key2.address().into()));
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
    let swarm1 = Arc::new(new_swarm(key1).await?);
    let swarm2 = Arc::new(new_swarm(key2).await?);
    manually_establish_connection(&swarm1, &swarm2).await?;
    let handler1 = swarm1.create_message_handler(None, None);
    let handler2 = swarm2.create_message_handler(None, None);

    tokio::select! {
        _ = async {
            futures::join!(
                async {
                    loop {
                        Arc::new(handler1.clone()).listen().await;
                    }
                },
                async {
                    loop {
                        Arc::new(handler2.clone()).listen().await;
                    }
                }
            );
        } => { unreachable!();}
        _ = async {
            let transport_1_to_2 = swarm1.get_transport(swarm2.did()).unwrap();
            sleep(Duration::from_millis(1000)).await;
            transport_1_to_2.wait_for_data_channel_open().await.unwrap();
            assert!(swarm1.dht().lock_successor()?.list().contains(&key2.address().into()), "{:?}", swarm1.dht().lock_successor()?.list());
            assert!(swarm2.dht().lock_successor()?.list().contains(&key1.address().into()));
            assert_eq!(
                transport_1_to_2.ice_connection_state().await,
                Some(RTCIceConnectionState::Connected)
            );
            handler1
                .send_message(
                    Message::NotifyPredecessorSend(message::NotifyPredecessorSend {
                        did: swarm1.did(),
                    }),
                    swarm2.did(),
                    swarm2.did(),
                )
                .await
                .unwrap();
            sleep(Duration::from_millis(1000)).await;
            assert_eq!(*swarm2.dht().lock_predecessor()?, Some(key1.address().into()));
            assert!(swarm1.dht().lock_successor()?.list().contains(&key2.address().into()));

            println!(
                "swarm1: {:?}, swarm2: {:?}",
                swarm1.did(),
                swarm2.did()
            );
            handler2
                .send_message(
                    Message::FindSuccessorSend(message::FindSuccessorSend {
                        did: swarm2.did(),
                        then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                        strict: true
                    }),
                    swarm1.did(),
                    swarm1.did(),
                )
                .await
                .unwrap();
            sleep(Duration::from_millis(1000)).await;
            assert!(swarm2.dht().lock_successor()?.list().contains(&key1.address().into()));
            assert!(swarm1.dht().lock_successor()?.list().contains(&key2.address().into()));
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
    let swarm1 = Arc::new(new_swarm(key1).await?);
    let swarm2 = Arc::new(new_swarm(key2).await?);
    manually_establish_connection(&swarm1, &swarm2).await?;
    let handler1 = swarm1.create_message_handler(None, None);
    let handler2 = swarm2.create_message_handler(None, None);

    // handle join dht situation
    tokio::select! {
        _ = async {
            futures::join!(
                async {
                    loop {
                        Arc::new(handler1.clone()).listen().await;
                    }
                },
                async {
                    loop {
                        Arc::new(handler2.clone()).listen().await;
                    }
                }
            );
        } => {unreachable!();}
        _ = async {
            let transport_1_to_2 = swarm1.get_transport(swarm2.did()).unwrap();
            sleep(Duration::from_millis(1000)).await;
            transport_1_to_2.wait_for_data_channel_open().await.unwrap();
            assert!(swarm1.dht().lock_successor()?.list().contains(&key2.address().into()));
            assert!(swarm2.dht().lock_successor()?.list().contains(&key1.address().into()));
            assert!(swarm1.dht()
                .lock_finger()?
                .contains(&Some(key2.address().into())));
            assert!(swarm2.dht()
                .lock_finger()?
                .contains(&Some(key1.address().into())));
            assert_eq!(
                transport_1_to_2.ice_connection_state().await,
                Some(RTCIceConnectionState::Connected)
            );
            handler1
                .send_message(
                    Message::NotifyPredecessorSend(message::NotifyPredecessorSend {
                        did: swarm1.did(),
                    }),
                    swarm2.did(),
                    swarm2.did(),
                )
                .await
                .unwrap();
            sleep(Duration::from_millis(1000)).await;
            assert_eq!(*swarm2.dht().lock_predecessor()?, Some(key1.address().into()));
            assert!(swarm1.dht().lock_successor()?.list().contains(&key2.address().into()));
            println!(
                "swarm1: {:?}, swarm2: {:?}",
                swarm1.did(),
                swarm2.did()
            );
            handler2
                .send_message(
                    Message::FindSuccessorSend(message::FindSuccessorSend {
                        did: swarm2.did(),
                        then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                        strict: true
                    }),
                    swarm1.did(),
                    swarm1.did(),
                )
                .await
                .unwrap();
            sleep(Duration::from_millis(1000)).await;
            let dht1_successor = swarm1.dht().lock_successor()?.clone();
            let dht2_successor = swarm2.dht().lock_successor()?.clone();
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
    // random key may faile here, because if key1 is more close to virtual_peer
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
    let swarm1 = Arc::new(new_swarm(key1).await?);
    let swarm2 = Arc::new(new_swarm(key2).await?);
    manually_establish_connection(&swarm1, &swarm2).await?;
    let handler1 = swarm1.create_message_handler(None, None);
    let handler2 = swarm2.create_message_handler(None, None);
    tokio::select! {
         _ = async {
             futures::join!(
                 async {
                     loop {
                         Arc::new(handler1.clone()).listen().await;
                     }
                 },
                 async {
                     loop {
                         Arc::new(handler2.clone()).listen().await;
                     }
                 }
             );
         } => { unreachable!();}
         _ = async {
             let transport_1_to_2 = swarm1.get_transport(swarm2.did()).unwrap();
             sleep(Duration::from_millis(1000)).await;
             transport_1_to_2.wait_for_data_channel_open().await.unwrap();
             // dht1's successor is dht2
             // dht2's successor is dht1
             assert!(swarm1.dht().lock_successor()?.list().contains(&key2.address().into()));
             assert!(swarm2.dht().lock_successor()?.list().contains(&key1.address().into()));
             assert_eq!(
                 transport_1_to_2.ice_connection_state().await,
                 Some(RTCIceConnectionState::Connected)
             );
             handler1
                 .send_message(
                     Message::NotifyPredecessorSend(message::NotifyPredecessorSend {
                         did: swarm1.did()
                     }),
                     swarm2.did(),
                     swarm2.did(),
                 )
                 .await
                 .unwrap();
             sleep(Duration::from_millis(1000)).await;
             assert_eq!(*swarm2.dht().lock_predecessor()?, Some(key1.address().into()));
             assert!(swarm1.dht().lock_successor()?.list().contains(&key2.address().into()));

             assert!(swarm2.dht().storage.count().await.unwrap() == 0);
             let message = String::from("this is a test string");
             let encoded_message = message.encode().unwrap();
             // the vid is hash of string
             let vnode: VirtualNode = encoded_message.try_into().unwrap();
             handler1.send_message(
                 Message::StoreVNode(message::StoreVNode {
                     data: vec![vnode.clone()]
                 }),
                 swarm2.did(),
                 swarm2.did(),
             )
             .await
             .unwrap();
             sleep(Duration::from_millis(5000)).await;
             assert!(swarm1.dht().storage.count().await.unwrap() == 0);
             assert!(swarm2.dht().storage.count().await.unwrap() > 0);
             let data: Result<VirtualNode> = swarm2.dht().storage.get(&(vnode.did)).await;
             assert!(data.is_ok(), "vnode: {:?} not in", vnode.did);
             let data = data.unwrap();
             assert_eq!(data.data[0].clone().decode::<String>().unwrap(), message);
             Ok::<(), Error>(())
         } => {}
    }
    tokio::fs::remove_dir_all("./tmp").await.ok();
    Ok(())
}
