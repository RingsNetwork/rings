#[cfg(test)]
pub mod test {
    use futures::lock::Mutex;
    use rings_core::dht::Stabilization;
    use rings_core::dht::{Did, PeerRing};
    use rings_core::ecc::SecretKey;
    use rings_core::err::Result;
    use rings_core::message::MessageHandler;
    use rings_core::session::SessionManager;
    use rings_core::swarm::Swarm;
    use rings_core::swarm::TransportManager;
    use rings_core::transports::Transport;
    use rings_core::types::ice_transport::IceTransport;
    use rings_core::types::ice_transport::IceTrickleScheme;
    use rings_core::types::message::MessageListener;
    use std::sync::Arc;
    use tokio::time::{sleep, Duration};
    use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
    use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

    fn new_chord(did: Did) -> PeerRing {
        PeerRing::new(did)
    }

    fn new_swarm(key: &SecretKey) -> Swarm {
        let stun = "stun://stun.l.google.com:19302";
        let session = SessionManager::new_with_seckey(key).unwrap();
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
            .get_handshake_info(swarm1.session_manager(), RTCSdpType::Offer)
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
            .get_handshake_info(swarm2.session_manager(), RTCSdpType::Answer)
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

    async fn run_stabilize(chord: Arc<Mutex<PeerRing>>, swarm: Arc<Swarm>) {
        let mut result = Result::<()>::Ok(());
        let stabilization = Stabilization::new(chord, swarm, 5usize);
        let timeout_in_secs = stabilization.get_timeout();
        println!("RUN Stabilization");
        while result.is_ok() {
            let timeout = sleep(Duration::from_secs(timeout_in_secs as u64));
            tokio::pin!(timeout);
            tokio::select! {
                _ = timeout.as_mut() => {
                    result = stabilization
                        .stabilize()
                        .await;
                }
            }
        }
    }

    #[tokio::test]
    async fn test_stabilization_once() -> Result<()> {
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
        println!(
            "swarm1: {:?}, swarm2: {:?}",
            swarm1.address(),
            swarm2.address()
        );

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
                    },
                );
            } => { unreachable!(); }
            _ = async {
                let transport_1_to_2 = swarm1.get_transport(&swarm2.address()).unwrap();
                sleep(Duration::from_millis(1000)).await;
                transport_1_to_2.wait_for_data_channel_open().await.unwrap();
                assert!(dht1.lock().await.successor.list().contains(&key2.address().into()));
                assert!(dht2.lock().await.successor.list().contains(&key1.address().into()));
                let stabilization = Stabilization::new(Arc::clone(&dht1), Arc::clone(&swarm1), 5usize);
                let _ = stabilization.stabilize().await;
                sleep(Duration::from_millis(10000)).await;
                assert_eq!(dht2.lock().await.predecessor, Some(key1.address().into()));
                assert!(dht1.lock().await.successor.list().contains(&key2.address().into()));
            } => {}
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_stabilization() -> Result<()> {
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

        tokio::select! {
            _ = async {
                tokio::join!(
                    async {
                        loop {
                            Arc::new(handler1.clone()).listen().await;
                        }
                    },
                    async {
                        loop {
                            Arc::new(handler2.clone()).listen().await;
                        }
                    },
                    async {
                        run_stabilize(Arc::clone(&dht1), Arc::clone(&swarm1)).await;
                    },
                    async {
                        run_stabilize(Arc::clone(&dht2), Arc::clone(&swarm2)).await;
                    }
                );
            } => { unreachable!(); }
            _ = async {
                let transport_1_to_2 = swarm1.get_transport(&swarm2.address()).unwrap();
                sleep(Duration::from_millis(1000)).await;
                transport_1_to_2.wait_for_data_channel_open().await.unwrap();
                assert!(dht1.lock().await.successor.list().contains(&key2.address().into()));
                assert!(dht2.lock().await.successor.list().contains(&key1.address().into()));
                sleep(Duration::from_millis(10000)).await;
                assert_eq!(dht2.lock().await.predecessor, Some(key1.address().into()));
                assert_eq!(dht1.lock().await.predecessor, Some(key2.address().into()));
            } => {}
        }

        Ok(())
    }
}
