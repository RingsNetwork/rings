//! ICE gathering of a browser connection that is closed while it gathers (#960).

use std::rc::Rc;
use std::time::Duration;

use js_sys::Array;
use rings_test_support::with_hang_guard;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test;
use web_sys::RtcConfiguration;
use web_sys::RtcIceServer;
use web_sys::RtcPeerConnection;
use web_sys::RtcSdpType;
use web_sys::RtcSessionDescription;
use web_sys::RtcSessionDescriptionInit;

use super::WebSysWebrtcConnection;
use crate::callback::InnerTransportCallback;
use crate::callback::NodeFramePool;
use crate::core::callback::TransportCallback;
use crate::core::pool::ChannelPool;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateCell;
use crate::error::Error;
use crate::notifier::Notifier;

/// Budget of the test: far below the gathering's own timeout, so a gathering the close does
/// not end fails the test by name.
const GATHER_CLOSE_HANG_GUARD: Duration = Duration::from_secs(20);

/// A transport callback that ignores every event.
struct IgnoredCallback;

#[async_trait::async_trait(?Send)]
impl TransportCallback for IgnoredCallback {}

/// A peer connection whose only ICE server is an unreachable STUN address, so its gathering
/// stays in progress until the STUN request times out, long after the test closes it. The
/// address is in TEST-NET-1 (RFC 5737), which never answers, on the standard STUN port, which
/// no browser blocks; a discard-port address such as `127.0.0.1:9` fails construction instead.
fn stalled_gathering_connection() -> RtcPeerConnection {
    let server = RtcIceServer::new();
    server.set_urls(&Array::of1(&JsValue::from_str("stun:192.0.2.1:3478")));
    let config = RtcConfiguration::new();
    config.set_ice_servers(&Array::of1(&server));
    RtcPeerConnection::new_with_configuration(&config)
        .expect("browser peer connection must construct")
}

/// A local close ends a gathering in progress at once, as `Local connection closed`.
///
/// `RTCPeerConnection.close()` fires no `icegatheringstatechange`, so a gathering that waits
/// only on that event outlives the close until its own timeout; the crossing-offer yield of
/// `connect_with_did` closes a creating offer exactly so, and the browser test hung (#960).
/// `join!` polls the gathering first, so it is waiting when the close runs.
#[wasm_bindgen_test]
async fn test_local_close_ends_a_gathering_in_progress() {
    with_hang_guard(
        "test_local_close_ends_a_gathering_in_progress",
        GATHER_CLOSE_HANG_GUARD,
        local_close_ends_a_gathering_in_progress(),
    )
    .await
}

/// The body of [`test_local_close_ends_a_gathering_in_progress`].
async fn local_close_ends_a_gathering_in_progress() {
    let peer_connection = stalled_gathering_connection();
    let _channel = peer_connection.create_data_channel("gather-close");
    let offer = JsFuture::from(peer_connection.create_offer())
        .await
        .expect("browser offer must be created");
    let local = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    local.set_sdp(&RtcSessionDescription::from(offer).sdp());
    JsFuture::from(peer_connection.set_local_description(&local))
        .await
        .expect("browser local description must apply");
    let connection = WebSysWebrtcConnection::new(
        peer_connection,
        Rc::new(ChannelPool::from_vec(Vec::new())),
        Notifier::default(),
        ConnectionStateCell::new(),
        Rc::new(InnerTransportCallback::new(
            "peer",
            Box::new(IgnoredCallback),
            Notifier::default(),
            NodeFramePool::new(),
        )),
    );

    let (gathered, closed) = futures::join!(connection.webrtc_gather(), connection.close());

    closed.expect("local close succeeds");
    assert!(
        matches!(&gathered, Err(Error::WebrtcLocalSdpGenerationError(message)) if message == "Local connection closed"),
        "{gathered:?}"
    );
}
