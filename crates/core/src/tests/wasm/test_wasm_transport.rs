use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::WebrtcConnectionState;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

use super::prepare_node;
use crate::ecc::SecretKey;
use crate::tests::manually_establish_connection;
use crate::transport::Transport;

struct DefaultCallback;
impl TransportCallback for DefaultCallback {}

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

async fn prepare_transport() -> Transport {
    let trans = Transport::new("stun://stun.l.google.com:19302", None);
    trans
        .new_connection("test", Box::new(DefaultCallback))
        .await
        .unwrap();
    trans
}

#[wasm_bindgen_test]
async fn test_ice_connection_establish() {
    get_fake_permission().await;
    let trans1 = prepare_transport().await;
    let conn1 = trans1.connection("test").unwrap();
    let trans2 = prepare_transport().await;
    let conn2 = trans2.connection("test").unwrap();

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
