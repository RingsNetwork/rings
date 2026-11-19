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

use super::gather_settle_delay;
use super::wait_for_candidates_settled;
use super::GatherCandidateProgress;
use super::WebSysWebrtcConnection;
use super::WEBRTC_GATHER_REFLEXIVE_SETTLE;
use crate::callback::InnerTransportCallback;
use crate::callback::NodeFramePool;
use crate::core::callback::TransportCallback;
use crate::core::pool::ChannelPool;
use crate::core::sdp::IceCandidateTier;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateCell;
use crate::error::Error;
use crate::ice_server::parse_ice_servers_or_warn;
use crate::ice_server::IceServer;
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
        matches!(&gathered, Err(Error::LocalHandshakeGenerationError(message)) if message == "Local connection closed"),
        "{gathered:?}"
    );
}

#[wasm_bindgen_test]
fn test_gather_settle_delay_classifies_reflexive_host_and_empty_text() {
    assert_eq!(
        gather_settle_delay(
            "candidate:1 1 udp 1677729535 14.155.204.30 51628 typ srflx raddr 0.0.0.0 rport 0",
            IceCandidateTier::Reflexive,
        ),
        Some(WEBRTC_GATHER_REFLEXIVE_SETTLE)
    );
    assert_eq!(
        gather_settle_delay(
            "candidate:1 1 udp 1677729535 14.155.204.30 51628 typ srflx raddr 0.0.0.0 rport 0",
            IceCandidateTier::Relay,
        ),
        None
    );
    assert_eq!(
        gather_settle_delay(
            "candidate:2 1 udp 16777215 203.0.113.5 54000 typ relay raddr 0.0.0.0 rport 0",
            IceCandidateTier::Reflexive,
        ),
        Some(WEBRTC_GATHER_REFLEXIVE_SETTLE)
    );
    assert_eq!(
        gather_settle_delay(
            "candidate:2 1 udp 16777215 203.0.113.5 54000 typ relay raddr 0.0.0.0 rport 0",
            IceCandidateTier::Relay,
        ),
        Some(WEBRTC_GATHER_REFLEXIVE_SETTLE)
    );
    assert_eq!(
        gather_settle_delay(
            "candidate:4 1 udp 1853824767 198.51.100.7 61000 typ prflx raddr 10.0.0.2 rport 56555",
            IceCandidateTier::Reflexive,
        ),
        Some(WEBRTC_GATHER_REFLEXIVE_SETTLE)
    );
    assert_eq!(
        gather_settle_delay(
            "candidate:4 1 udp 1853824767 198.51.100.7 61000 typ prflx raddr 10.0.0.2 rport 56555",
            IceCandidateTier::Relay,
        ),
        None
    );
    assert_eq!(
        gather_settle_delay(
            "candidate:3 1 udp 2113937151 test.local 56555 typ host",
            IceCandidateTier::Reflexive,
        ),
        None
    );
    assert_eq!(
        gather_settle_delay(
            "candidate:3 1 udp 2113937151 test.local 56555 typ host",
            IceCandidateTier::Host,
        ),
        Some(WEBRTC_GATHER_REFLEXIVE_SETTLE)
    );
    assert_eq!(
        gather_settle_delay(
            "v=0\r\na=candidate:3 1 udp 2113937151 10.0.0.2 56555 typ host\r\n",
            IceCandidateTier::Reflexive,
        ),
        None
    );
    assert_eq!(
        gather_settle_delay(
            "v=0\r\na=candidate:3 1 udp 2113937151 10.0.0.2 56555 typ host\r\n",
            IceCandidateTier::Host,
        ),
        Some(WEBRTC_GATHER_REFLEXIVE_SETTLE)
    );
    assert_eq!(
        gather_settle_delay(
            "v=0\r\na=candidate:1 1 udp 2113937151 10.0.0.2 56555 typ host\r\na=candidate:2 1 udp 1677729535 14.155.204.30 51628 typ srflx raddr 0.0.0.0 rport 0\r\n",
            IceCandidateTier::Reflexive,
        ),
        Some(WEBRTC_GATHER_REFLEXIVE_SETTLE)
    );
    assert_eq!(
        gather_settle_delay(
            "v=0\r\na=candidate:1 1 udp 2113937151 10.0.0.2 56555 typ host\r\na=candidate:2 1 udp 1677729535 14.155.204.30 51628 typ srflx raddr 0.0.0.0 rport 0\r\n",
            IceCandidateTier::Relay,
        ),
        None
    );
    assert_eq!(
        gather_settle_delay("v=0\r\ns=-\r\n", IceCandidateTier::Reflexive),
        None
    );

    let no_servers = parse_ice_servers_or_warn("", "test");
    assert_eq!(
        IceServer::target_candidate_tier(&no_servers),
        IceCandidateTier::Host
    );
    let stun_only = parse_ice_servers_or_warn("stun://stun.l.google.com:19302", "test");
    assert_eq!(
        IceServer::target_candidate_tier(&stun_only),
        IceCandidateTier::Reflexive
    );
    let with_turn = parse_ice_servers_or_warn(
        "stun://stun.l.google.com:19302;turn://u:p@turn.example.com:3478",
        "test",
    );
    assert_eq!(
        IceServer::target_candidate_tier(&with_turn),
        IceCandidateTier::Relay
    );

    // When TURN is configured (`IceCandidateTier::Relay`), lower-tier candidates must not arm the
    // settle timer before a `typ relay` candidate arrives, but once a `typ relay` candidate has
    // armed the timer, subsequent lower-tier candidates must advance `epoch` to reset the quiet
    // window.
    let relay_progress = GatherCandidateProgress::new(IceCandidateTier::Relay);
    assert!(
        !relay_progress.observe_text("candidate:1 1 udp 2113937151 10.0.0.2 56555 typ host"),
        "host candidate must not arm settle timer while relay tier is outstanding"
    );
    assert!(
        !relay_progress.observe_text(
            "candidate:2 1 udp 1677729535 14.155.204.30 51628 typ srflx raddr 0.0.0.0 rport 0"
        ),
        "srflx candidate must not arm settle timer while relay tier is outstanding"
    );
    assert!(
        relay_progress.observe_text(
            "candidate:3 1 udp 16777215 203.0.113.5 54000 typ relay raddr 0.0.0.0 rport 0"
        ),
        "relay candidate must arm the 300 ms settle timer"
    );
    assert!(
        relay_progress.observe_text(
            "candidate:4 1 udp 1677729535 14.155.204.30 51629 typ srflx raddr 0.0.0.0 rport 0"
        ),
        "subsequent srflx candidate after relay must reset the armed quiet window"
    );
}

/// If a host candidate arrives first (leaving the settle timer unarmed while waiting for STUN) and
/// a reflexive candidate arrives shortly after, `GatherCandidateProgress::observe_text` must wake
/// the in-flight wait and settle on the 300 ms reflexive window.
#[wasm_bindgen_test]
async fn test_reflexive_candidate_shortens_in_flight_host_settle_window() {
    with_hang_guard(
        "test_reflexive_candidate_shortens_in_flight_host_settle_window",
        Duration::from_millis(1200),
        async {
            let progress = Rc::new(GatherCandidateProgress::new(IceCandidateTier::Reflexive));
            assert!(!progress.observe_text("candidate:1 1 udp 2113937151 10.0.0.2 56555 typ host"));
            let progress_for_srflx = Rc::clone(&progress);
            let inject_srflx = async move {
                rings_runtime::sleep(Duration::from_millis(50))
                    .await
                    .expect("browser timer must fire");
                assert!(progress_for_srflx.observe_text(
                    "candidate:2 1 udp 1677729535 14.155.204.30 51628 typ srflx raddr 0.0.0.0 rport 0"
                ));
            };
            let (settled, ()) =
                futures::join!(wait_for_candidates_settled(progress), inject_srflx);
            settled.expect("settled wait must complete within the shortened reflexive window");
        },
    )
    .await
}

/// Even when a peer connection's `iceGatheringState` remains in `"gathering"` past the settle
/// window (as WebKit does on iOS/macOS when secondary interfaces or straggler probes do not finish),
/// `webrtc_gather` resolves once a candidate reaching the connection's target tier settles rather
/// than waiting out the full 60 s gather timeout.
#[wasm_bindgen_test]
async fn test_stalled_gathering_settles_once_candidates_arrive() {
    with_hang_guard(
        "test_stalled_gathering_settles_once_candidates_arrive",
        Duration::from_secs(10),
        stalled_gathering_settles_once_candidates_arrive(),
    )
    .await
}

async fn stalled_gathering_settles_once_candidates_arrive() {
    let peer_connection = stalled_gathering_connection();
    let _channel = peer_connection.create_data_channel("gather-settle");
    let connection = WebSysWebrtcConnection::new(
        peer_connection.clone(),
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
    connection.target_tier.set(IceCandidateTier::Host);

    let sdp = connection
        .create_offer()
        .await
        .expect("offer gathering must settle before the 60 s timeout");
    assert!(
        sdp.contains("a=candidate:"),
        "settled offer SDP must contain at least one gathered candidate: {sdp}"
    );
    let _ = connection.close().await;
}
