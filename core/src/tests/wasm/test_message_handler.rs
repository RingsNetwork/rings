use std::str::FromStr;
use std::sync::Arc;

use crate::channels::Channel as CbChannel;
use crate::dht::Chord;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::session::DelegatedSk;
use crate::swarm::Swarm;
use crate::transports::manager::TransportManager;
use crate::transports::Transport;
use crate::types::channel::Channel;
use crate::types::channel::TransportEvent;
use crate::types::ice_transport::IceServer;
use crate::types::ice_transport::IceTransportCallback;
use crate::types::ice_transport::IceTransportInterface;
use crate::types::ice_transport::IceTrickleScheme;
use wasm_bindgen_test::wasm_bindgen_test_configure;
use wasm_bindgen_test::*;
use web_sys::RtcIceConnectionState;
use web_sys::RtcSdpType;

wasm_bindgen_test_configure!(run_in_browser);

fn new_chord(did: Did) -> Chord {
    Chord::new(did)
}

fn new_swarm(key: &SecretKey) -> Swarm {
    let stun = "stun://stun.l.google.com:19302";
    let session = DelegatedSk::new_with_seckey(key).unwrap();
    Swarm::new(stun, key.address(), session)
}

async fn prepare_transport(channel: Option<Arc<CbChannel<TransportEvent>>>) -> Result<Transport> {
    let ch = match channel {
        Some(c) => Arc::clone(&c),
        None => Arc::new(<CbChannel<TransportEvent> as Channel<TransportEvent>>::new(1)),
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

    let session1 = DelegatedSk::new_with_seckey(&key1).unwrap();
    let session2 = DelegatedSk::new_with_seckey(&key2).unwrap();

    // Peer 1 try to connect peer 2
    let handshake_info1 = transport1
        .get_handshake_info(session1, RtcSdpType::Offer)
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
    assert_eq!(
        transport1.ice_connection_state().await,
        Some(RtcIceConnectionState::New)
    );
    assert_eq!(
        transport2.ice_connection_state().await,
        Some(RtcIceConnectionState::New)
    );

    // Peer 2 create answer
    let handshake_info2 = transport2
        .get_handshake_info(session2, RtcSdpType::Answer)
        .await
        .unwrap();

    assert_eq!(
        transport1.ice_connection_state().await,
        Some(RtcIceConnectionState::New)
    );
    assert_eq!(
        transport2.ice_connection_state().await,
        Some(RtcIceConnectionState::Checking)
    );

    // Peer 1 got answer then register
    let addr2 = transport1
        .register_remote_info(handshake_info2)
        .await
        .unwrap();
    // assert_eq!(
    //     transport1.ice_connection_state().await,
    //     Some(RtcIceConnectionState::Checking)
    // );

    assert_eq!(addr2, key2.address());
    // let promise_1 = transport1.connect_success_promise().await.unwrap();
    // let promise_2 = transport2.connect_success_promise().await.unwrap();
    // promise_1.await.unwrap();
    // promise_2.await.unwrap();
    // assert_eq!(
    //     transport1.ice_connection_state().await,
    //     Some(RtcIceConnectionState::Connected)
    // );
    // assert_eq!(
    //     transport2.ice_connection_state().await,
    //     Some(RtcIceConnectionState::Connected)
    // );
    Ok(())
}
