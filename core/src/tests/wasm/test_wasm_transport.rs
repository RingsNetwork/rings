use std::sync::Arc;

use rings_transport::core::callback::Callback;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::WebrtcConnectionState;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

use super::prepare_node;
use crate::channels::Channel as CbChannel;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::swarm::callback::InnerSwarmCallback;
use crate::tests::manually_establish_connection;
use crate::types::channel::Channel;
use crate::types::channel::TransportEvent;
use crate::types::Connection;
use crate::types::Transport;

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

async fn prepare_transport(channel: Option<Arc<CbChannel<TransportEvent>>>) -> Transport {
    let ch = match channel {
        Some(c) => Arc::clone(&c),
        None => Arc::new(<CbChannel<TransportEvent> as Channel<TransportEvent>>::new()),
    };
    let callback = InnerSwarmCallback::new(ch.sender()).boxed();
    let trans = Transport::new("stun://stun.l.google.com:19302", None);
    trans
        .new_connection("test", Arc::new(callback))
        .await
        .unwrap();
    trans
}

pub async fn establish_ice_connection(conn1: &Connection, conn2: &Connection) -> Result<()> {
    assert_eq!(conn1.webrtc_connection_state(), WebrtcConnectionState::New);
    assert_eq!(conn2.webrtc_connection_state(), WebrtcConnectionState::New);

    let offer = conn1.webrtc_create_offer().await.unwrap();
    let answer = conn2.webrtc_answer_offer(offer).await.unwrap();
    conn1.webrtc_accept_answer(answer).await.unwrap();

    #[cfg(feature = "browser_chrome_test")]
    {
        conn2.webrtc_wait_for_data_channel_open().await.unwrap();
        assert_eq!(
            conn2.webrtc_connection_state(),
            WebrtcConnectionState::Connected
        );
    }

    Ok(())
}

#[wasm_bindgen_test]
async fn test_ice_connection_establish() {
    get_fake_permission().await;
    let trans1 = prepare_transport(None).await;
    let conn1 = trans1.connection("test").unwrap();
    let trans2 = prepare_transport(None).await;
    let conn2 = trans2.connection("test").unwrap();
    establish_ice_connection(&conn1, &conn2).await.unwrap();
}

#[wasm_bindgen_test]
async fn test_message_handler() {
    get_fake_permission().await;

    let key1 = SecretKey::random();
    let key2 = SecretKey::random();

    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;

    manually_establish_connection(&node1, &node2).await;
}
