use std::str::FromStr;
use std::sync::Arc;

use futures::lock::Mutex;
use rings_core::channels::Channel as CbChannel;
use rings_core::dht::PeerRing;
use rings_core::ecc::SecretKey;
use rings_core::err::Result;
use rings_core::message::MessageHandler;
use rings_core::prelude::RTCSdpType;
use rings_core::session::SessionManager;
use rings_core::storage::PersistenceStorage;
use rings_core::swarm::Swarm;
use rings_core::swarm::TransportManager;
use rings_core::transports::Transport;
use rings_core::types::channel::Channel;
use rings_core::types::channel::Event;
use rings_core::types::ice_transport::IceServer;
use rings_core::types::ice_transport::IceTransport;
use rings_core::types::ice_transport::IceTransportCallback;
use rings_core::types::ice_transport::IceTrickleScheme;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;
use web_sys::RtcIceConnectionState;
use web_sys::RtcSdpType;

// wasm_bindgen_test_configure!(run_in_browser);

async fn get_fake_permission() {
    let window = web_sys::window().unwrap();
    let nav = window.navigator();
    let media = nav.media_devices().unwrap();
    let mut cons = web_sys::MediaStreamConstraints::new();
    cons.audio(&JsValue::from(true));
    cons.video(&JsValue::from(false));
    cons.fake(true);
    let promise = media.get_user_media_with_constraints(&cons).unwrap();
    JsFuture::from(promise).await.unwrap();
}

async fn prepare_transport(channel: Option<Arc<CbChannel<Event>>>) -> Result<Transport> {
    let ch = match channel {
        Some(c) => Arc::clone(&c),
        None => Arc::new(<CbChannel<Event> as Channel<Event>>::new()),
    };
    let mut trans = Transport::new(ch.sender());
    let stun = IceServer::from_str("stun://stun.l.google.com:19302").unwrap();
    trans.start(&stun).await.unwrap();
    trans.apply_callback().await.unwrap();
    Ok(trans)
}

pub async fn establish_connection(transport1: &Transport, transport2: &Transport) -> Result<()> {
    // Generate key pairs for signing and verification
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();

    let sm1 = SessionManager::new_with_seckey(&key1).unwrap();
    let sm2 = SessionManager::new_with_seckey(&key2).unwrap();

    // Peer 1 try to connect peer 2
    let handshake_info1 = transport1
        .get_handshake_info(&sm1, RtcSdpType::Offer)
        .await
        .unwrap();

    assert_eq!(
        transport1.ice_connection_state().await,
        Some(RtcIceConnectionState::New)
    );

    assert_eq!(
        transport2.ice_connection_state().await,
        Some(RtcIceConnectionState::New)
    );

    // Peer 2 got offer then register
    let addr1 = transport2.register_remote_info(handshake_info1).await?;

    assert_eq!(addr1, key1.address());
    // Peer 2 create answer
    let handshake_info2 = transport2
        .get_handshake_info(&sm2, RtcSdpType::Answer)
        .await
        .unwrap();

    // Peer 1 got answer then register
    let addr2 = transport1
        .register_remote_info(handshake_info2)
        .await
        .unwrap();
    assert_eq!(addr2, key2.address());

    #[cfg(feature = "browser_chrome_test")]
    {
        transport2.wait_for_data_channel_open().await.unwrap();
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RtcIceConnectionState::Connected)
        );
    }
    Ok(())
}

#[wasm_bindgen_test]
async fn test_ice_connection_establish() {
    get_fake_permission().await;
    let transport1 = prepare_transport(None).await.unwrap();
    let transport2 = prepare_transport(None).await.unwrap();
    establish_connection(&transport1, &transport2)
        .await
        .unwrap();
}

#[wasm_bindgen_test]
async fn test_message_handler() {
    get_fake_permission().await;
    let stun = "stun://stun.l.google.com:19302";
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let _addr1 = key1.address();
    let _addr2 = key2.address();

    println!(
        "test with key1:{:?}, key2:{:?}",
        key1.address(),
        key2.address()
    );

    let db1 =
        PersistenceStorage::new_with_cap_and_name(1000, uuid::Uuid::new_v4().to_string().as_str())
            .await
            .unwrap();
    let db2 =
        PersistenceStorage::new_with_cap_and_name(1000, uuid::Uuid::new_v4().to_string().as_str())
            .await
            .unwrap();

    let dht1 = Arc::new(Mutex::new(PeerRing::new_with_storage(
        key1.address().into(),
        Arc::new(db1),
    )));
    let dht2 = Arc::new(Mutex::new(PeerRing::new_with_storage(
        key2.address().into(),
        Arc::new(db2),
    )));

    let sm1 = SessionManager::new_with_seckey(&key1).unwrap();
    let sm2 = SessionManager::new_with_seckey(&key2).unwrap();

    let swarm1 = Arc::new(Swarm::new(stun, key1.pubkey(), sm1.clone()));
    let swarm2 = Arc::new(Swarm::new(stun, key2.pubkey(), sm2.clone()));

    let transport1 = swarm1.new_transport().await.unwrap();
    let transport2 = swarm2.new_transport().await.unwrap();

    let node1 = Arc::new(MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1)));
    let node2 = Arc::new(MessageHandler::new(Arc::clone(&dht2), Arc::clone(&swarm2)));

    // first node1 generate handshake info
    let handshake_info1 = transport1
        .get_handshake_info(&sm1, RTCSdpType::Offer)
        .await
        .unwrap();

    // node3 register handshake from node1
    let addr1 = transport2
        .register_remote_info(handshake_info1)
        .await
        .unwrap();
    // and reponse a Answer
    let handshake_info2 = transport2
        .get_handshake_info(&sm2, RTCSdpType::Answer)
        .await
        .unwrap();

    // node1 accpeted the answer
    let addr2 = transport1
        .register_remote_info(handshake_info2)
        .await
        .unwrap();

    assert_eq!(addr1, key1.address());
    assert_eq!(addr2, key2.address());

    swarm1
        .register(&swarm2.address(), transport1.clone())
        .await
        .unwrap();
    swarm2
        .register(&swarm1.address(), transport2.clone())
        .await
        .unwrap();

    assert!(swarm1.get_transport(&key2.address()).is_some());
    assert!(swarm2.get_transport(&key1.address()).is_some());

    node1.listen_once().await;
    node2.listen_once().await;
}
