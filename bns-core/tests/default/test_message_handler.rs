#[cfg(test)]
pub mod test {
    use bns_core::dht::{Chord, Did};
    use bns_core::ecc::SecretKey;
    use bns_core::err::Result;
    use bns_core::message;
    use bns_core::message::handler::MessageHandler;
    use bns_core::message::{Message, MessageRelayMethod};
    use bns_core::session::SessionManager;
    use bns_core::swarm::Swarm;
    use bns_core::swarm::TransportManager;
    use bns_core::transports::Transport;
    use bns_core::types::ice_transport::IceTransport;
    use bns_core::types::ice_transport::IceTrickleScheme;
    use futures::lock::Mutex;
    use std::sync::Arc;
    use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
    use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

    fn new_chord(did: Did) -> Chord {
        Chord::new(did)
    }

    fn new_swarm(key: &SecretKey) -> Swarm {
        let stun = "stun://stun.l.google.com:19302";
        let session = SessionManager::new_with_seckey(&key).unwrap();
        Swarm::new(stun, key.address(), session)
    }

    pub async fn establish_connection(
        swarm1: Arc<Swarm>,
        swarm2: Arc<Swarm>,
    ) -> Result<(Arc<Transport>, Arc<Transport>)> {
        assert!(swarm1.get_transport(&swarm2.address()).is_none());
        assert!(swarm2.get_transport(&swarm1.address()).is_none());

        let transport1 = swarm1.new_transport().await.unwrap();
        let transport2 = swarm2.new_transport().await.unwrap();

        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );

        // Peer 1 try to connect peer 2
        let handshake_info1 = transport1
            .get_handshake_info(swarm1.session(), RTCSdpType::Offer)
            .await?;
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );

        // Peer 2 got offer then register
        let addr1 = transport2.register_remote_info(handshake_info1).await?;
        assert_eq!(addr1, swarm1.address());
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );

        // Peer 2 create answer
        let handshake_info2 = transport2
            .get_handshake_info(swarm2.session(), RTCSdpType::Answer)
            .await?;
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::Checking)
        );

        // Peer 1 got answer then register
        let addr2 = transport1.register_remote_info(handshake_info2).await?;
        assert_eq!(addr2, swarm2.address());
        let promise_1 = transport1.connect_success_promise().await?;
        let promise_2 = transport2.connect_success_promise().await?;
        promise_1.await?;
        promise_2.await?;
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );
        swarm1
            .register(&swarm2.address(), transport1.clone())
            .await?;
        swarm2
            .register(&swarm1.address(), transport2.clone())
            .await?;
        let transport_1_to_2 = swarm1.get_transport(&swarm2.address()).unwrap();
        let transport_2_to_1 = swarm2.get_transport(&swarm1.address()).unwrap();

        assert!(Arc::ptr_eq(&transport_1_to_2, &transport1));
        assert!(Arc::ptr_eq(&transport_2_to_1, &transport2));

        Ok((transport1, transport2))
    }

    #[tokio::test]
    async fn test_handle_join() -> Result<()> {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();
        let dht1 = Arc::new(Mutex::new(new_chord(key1.address().into())));
        let swarm1 = Arc::new(new_swarm(&key1));
        let swarm2 = Arc::new(new_swarm(&key2));
        let (_, _) = establish_connection(Arc::clone(&swarm1), Arc::clone(&swarm2)).await?;
        let handle1 = MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1));
        let relay_message = match swarm1.poll_message().await {
            Some(relay_message) => relay_message,
            None => {
                assert_eq!(false, true);
                unreachable!();
            }
        };
        match handle1
            .handle_message_relay(&relay_message, &key2.address().into())
            .await
        {
            Ok(_) => assert_eq!(true, true),
            Err(e) => {
                println!("{:?}", e);
                assert_eq!(true, false);
            }
        };
        assert_eq!(dht1.lock().await.successor, key2.address().into());
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_connect_node() -> Result<()> {
        let mut key1 = SecretKey::random();
        let mut key2 = SecretKey::random();
        let mut key3 = SecretKey::random();
        let mut v = vec![key1, key2, key3];
        v.sort_by(|a, b| {
            if a.address() < b.address() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        });
        (key1, key2, key3) = (v[0], v[1], v[2]);
        assert!(key1.address() < key2.address(), "key1 < key2");
        assert!(key2.address() < key3.address(), "key2 < key3");
        assert!(key1.address() < key3.address(), "key1 < key3");
        let swarm1 = Arc::new(new_swarm(&key1));
        let swarm2 = Arc::new(new_swarm(&key2));
        let swarm3 = Arc::new(new_swarm(&key3));
        let dht1 = Arc::new(Mutex::new(new_chord(key1.address().into())));
        let dht2 = Arc::new(Mutex::new(new_chord(key2.address().into())));
        let dht3 = Arc::new(Mutex::new(new_chord(key3.address().into())));

        let (_, _) = establish_connection(Arc::clone(&swarm1), Arc::clone(&swarm2)).await?;
        let (_, _) = establish_connection(Arc::clone(&swarm2), Arc::clone(&swarm3)).await?;

        let transport_1_to_3 = swarm1.new_transport().await.unwrap();
        let handshake_info13 = transport_1_to_3
            .get_handshake_info(swarm1.session(), RTCSdpType::Offer)
            .await?;
        swarm1
            .register(&swarm3.address(), Arc::clone(&transport_1_to_3))
            .await?;

        let handler1 = MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1));
        let handler2 = MessageHandler::new(Arc::clone(&dht2), Arc::clone(&swarm2));
        let handler3 = MessageHandler::new(Arc::clone(&dht3), Arc::clone(&swarm3));

        // handle join dht situation
        handler1.listen_once().await;
        handler2.listen_once().await;

        handler3.listen_once().await;
        handler2.listen_once().await;
        let transport_1_to_2 = swarm1.get_transport(&swarm2.address()).unwrap();
        let transport_2_to_3 = swarm2.get_transport(&swarm3.address()).unwrap();
        assert_eq!(
            dht1.lock().await.successor,
            key2.address().into(),
            "dht1 successor is key2"
        );
        assert_eq!(
            dht2.lock().await.successor,
            key3.address().into(),
            "dht2 successor is key3"
        );
        assert_eq!(
            dht3.lock().await.successor,
            key2.address().into(),
            "dht3 successor is key2"
        );

        transport_1_to_2.wait_for_data_channel_open().await?;
        transport_2_to_3.wait_for_data_channel_open().await?;
        assert_eq!(
            transport_1_to_2.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );
        assert_eq!(
            transport_2_to_3.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );
        handler1
            .send_message(
                &swarm2.address(),
                None,
                None,
                MessageRelayMethod::SEND,
                Message::ConnectNode(message::ConnectNode {
                    sender_id: swarm1.address().into(),
                    target_id: swarm3.address().into(),
                    handshake_info: handshake_info13.to_string(),
                }),
            )
            .await?;
        handler2.listen_once().await;
        handler3.listen_once().await;
        handler2.listen_once().await;
        handler3.listen_once().await;
        handler2.listen_once().await;
        handler1.listen_once().await;
        handler3.listen_once().await;
        handler3.listen_once().await;
        handler2.listen_once().await;
        handler2.listen_once().await;
        handler1.listen_once().await;
        let transport_1_to_3 = swarm1.get_transport(&swarm3.address());
        assert!(transport_1_to_3.is_some());
        let transport_1_to_3 = transport_1_to_3.unwrap();
        assert_eq!(
            transport_1_to_3.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        handler3.listen_once().await;
        handler1.listen_once().await;
        transport_1_to_3.wait_for_data_channel_open().await?;
        assert_eq!(
            transport_1_to_3.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_notify_predecessor() -> Result<()> {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();
        let dht1 = Arc::new(Mutex::new(new_chord(key1.address().into())));
        let dht2 = Arc::new(Mutex::new(new_chord(key2.address().into())));
        let swarm1 = Arc::new(new_swarm(&key1));
        let swarm2 = Arc::new(new_swarm(&key2));
        let (_, _) = establish_connection(Arc::clone(&swarm1), Arc::clone(&swarm2)).await?;
        let handler1 = MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1));
        let handler2 = MessageHandler::new(Arc::clone(&dht2), Arc::clone(&swarm2));

        // handle join dht situation
        handler1.listen_once().await;
        handler2.listen_once().await;

        let transport_1_to_2 = swarm1.get_transport(&swarm2.address()).unwrap();
        assert_eq!(dht1.lock().await.successor, key2.address().into());
        assert_eq!(dht2.lock().await.successor, key1.address().into());
        assert_eq!(
            transport_1_to_2.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );
        transport_1_to_2.wait_for_data_channel_open().await?;
        handler1
            .send_message(
                &swarm2.address(),
                None,
                None,
                MessageRelayMethod::SEND,
                Message::NotifyPredecessor(message::NotifyPredecessor {
                    id: key1.address().into(),
                }),
            )
            .await?;
        handler2.listen_once().await;
        assert_eq!(dht2.lock().await.predecessor, Some(key1.address().into()));
        handler1.listen_once().await;
        assert_eq!(dht1.lock().await.successor, key2.address().into());
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_find_successor_increase() -> Result<()> {
        let mut key1 = SecretKey::random();
        let mut key2 = SecretKey::random();
        if key1.address() > key2.address() {
            (key1, key2) = (key2, key1)
        }
        let dht1 = Arc::new(Mutex::new(new_chord(key1.address().into())));
        let dht2 = Arc::new(Mutex::new(new_chord(key2.address().into())));
        let swarm1 = Arc::new(new_swarm(&key1));
        let swarm2 = Arc::new(new_swarm(&key2));
        let (_, _) = establish_connection(Arc::clone(&swarm1), Arc::clone(&swarm2)).await?;

        let handler1 = MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1));
        let handler2 = MessageHandler::new(Arc::clone(&dht2), Arc::clone(&swarm2));

        // handle join dht situation
        handler1.listen_once().await;
        handler2.listen_once().await;
        let transport_1_to_2 = swarm1.get_transport(&swarm2.address()).unwrap();
        assert_eq!(dht1.lock().await.successor, key2.address().into());
        assert_eq!(dht2.lock().await.successor, key1.address().into());
        assert_eq!(
            transport_1_to_2.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );
        transport_1_to_2.wait_for_data_channel_open().await?;
        handler1
            .send_message(
                &swarm2.address(),
                None,
                None,
                MessageRelayMethod::SEND,
                Message::NotifyPredecessor(message::NotifyPredecessor {
                    id: swarm1.address().into(),
                }),
            )
            .await?;
        handler2.listen_once().await;
        assert_eq!(dht2.lock().await.predecessor, Some(key1.address().into()));
        handler1.listen_once().await;
        assert_eq!(dht1.lock().await.successor, key2.address().into());

        println!(
            "swarm1: {:?}, swarm2: {:?}",
            swarm1.address(),
            swarm2.address()
        );
        handler2
            .send_message(
                &swarm1.address(),
                None,
                None,
                MessageRelayMethod::SEND,
                Message::FindSuccessor(message::FindSuccessor {
                    id: swarm2.address().into(),
                    for_fix: false,
                }),
            )
            .await?;
        handler1.listen_once().await;
        handler2.listen_once().await;
        //assert_eq!(dht2.lock().await.successor, key2.address().into());
        //assert_eq!(dht1.lock().await.successor, key2.address().into());
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
        let dht1 = Arc::new(Mutex::new(new_chord(key1.address().into())));
        let dht2 = Arc::new(Mutex::new(new_chord(key2.address().into())));
        let swarm1 = Arc::new(new_swarm(&key1));
        let swarm2 = Arc::new(new_swarm(&key2));
        let (_, _) = establish_connection(Arc::clone(&swarm1), Arc::clone(&swarm2)).await?;

        let handler1 = MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1));
        let handler2 = MessageHandler::new(Arc::clone(&dht2), Arc::clone(&swarm2));

        // handle join dht situation
        handler1.listen_once().await;
        handler2.listen_once().await;
        let transport_1_to_2 = swarm1.get_transport(&swarm2.address()).unwrap();
        assert_eq!(dht1.lock().await.successor, key2.address().into());
        assert_eq!(dht2.lock().await.successor, key1.address().into());

        assert!(dht1
            .lock()
            .await
            .finger
            .contains(&Some(key2.address().into())));
        assert!(dht2
            .lock()
            .await
            .finger
            .contains(&Some(key1.address().into())));

        assert_eq!(
            transport_1_to_2.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );
        transport_1_to_2.wait_for_data_channel_open().await?;
        handler1
            .send_message(
                &swarm2.address(),
                None,
                None,
                MessageRelayMethod::SEND,
                Message::NotifyPredecessor(message::NotifyPredecessor {
                    id: swarm1.address().into(),
                }),
            )
            .await?;
        handler2.listen_once().await;
        assert_eq!(dht2.lock().await.predecessor, Some(key1.address().into()));
        handler1.listen_once().await;
        assert_eq!(dht1.lock().await.successor, key2.address().into());

        println!(
            "swarm1: {:?}, swarm2: {:?}",
            swarm1.address(),
            swarm2.address()
        );
        handler2
            .send_message(
                &swarm1.address(),
                None,
                None,
                MessageRelayMethod::SEND,
                Message::FindSuccessor(message::FindSuccessor {
                    id: swarm2.address().into(),
                    for_fix: false,
                }),
            )
            .await?;
        handler1.listen_once().await;
        //        handler2.listen_once().await;
        assert_eq!(dht2.lock().await.successor, key1.address().into());
        assert_eq!(dht1.lock().await.successor, key2.address().into());
        Ok(())
    }
}
