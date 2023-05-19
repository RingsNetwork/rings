use std::str::FromStr;
use std::sync::Arc;

use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;
use web_sys::RtcIceConnectionState;

use super::prepare_node;
use crate::channels::Channel as CbChannel;
use crate::ecc::SecretKey;
use crate::err::Result;
use crate::prelude::RTCSdpType;
use crate::tests::manually_establish_connection;
use crate::transports::Transport;
use crate::types::channel::Channel;
use crate::types::channel::TransportEvent;
use crate::types::ice_transport::IceServer;
use crate::types::ice_transport::IceTransportInterface;
use crate::types::ice_transport::IceTrickleScheme;

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

async fn prepare_transport(channel: Option<Arc<CbChannel<TransportEvent>>>) -> Result<Transport> {
    let ch = match channel {
        Some(c) => Arc::clone(&c),
        None => Arc::new(<CbChannel<TransportEvent> as Channel<TransportEvent>>::new()),
    };
    let mut trans = Transport::new(ch.sender());
    let stun = IceServer::from_str("stun://stun.l.google.com:19302").unwrap();
    trans.start(vec![stun], None).await.unwrap();
    trans.apply_callback().await.unwrap();
    Ok(trans)
}

pub async fn establish_ice_connection(
    transport1: &Transport,
    transport2: &Transport,
) -> Result<()> {
    assert_eq!(
        transport1.ice_connection_state().await,
        Some(RtcIceConnectionState::New)
    );
    assert_eq!(
        transport2.ice_connection_state().await,
        Some(RtcIceConnectionState::New)
    );

    // Generate key pairs for did register
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();

    // Peer 1 try to connect peer 2
    let handshake_info1 = transport1.get_handshake_info(RTCSdpType::Offer).await?;

    // Peer 2 got offer then register
    transport2
        .register_remote_info(&handshake_info1, key1.address().into())
        .await?;

    // Peer 2 create answer
    let handshake_info2 = transport2
        .get_handshake_info(RTCSdpType::Answer)
        .await
        .unwrap();

    // Peer 1 got answer then register
    transport1
        .register_remote_info(&handshake_info2, key2.address().into())
        .await
        .unwrap();

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
    establish_ice_connection(&transport1, &transport2)
        .await
        .unwrap();
}

#[wasm_bindgen_test]
async fn test_message_handler() {
    get_fake_permission().await;

    let key1 = SecretKey::random();
    let key2 = SecretKey::random();

    let (_did1, _dht1, swarm1, _handler1) = prepare_node(key1).await;
    let (_did2, _dht2, swarm2, _handler2) = prepare_node(key2).await;

    manually_establish_connection(&swarm1, &swarm2)
        .await
        .unwrap();
}
