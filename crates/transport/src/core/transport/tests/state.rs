// SDP parsing (including section semantics) is tested in `crate::core::sdp`; these cover the
// policy `effective_*` layers on top of it (default / no-limit / cap).
use super::effective_max_message_size;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use super::ConnectionStateCell;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use super::ConnectionStateSnapshot;
use super::WebrtcConnectionState;
use super::MAX_DATA_CHANNEL_MESSAGE_SIZE;

/// A data-channel SDP advertising `max-message-size:<value>` in the right media section.
fn sdp_with(value: &str) -> String {
    format!(
        "v=0\r\n\
         m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
         a=max-message-size:{value}\r\n"
    )
}

#[test]
fn effective_absent_defaults_to_cap() {
    let sdp = "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
    assert_eq!(
        effective_max_message_size(sdp),
        MAX_DATA_CHANNEL_MESSAGE_SIZE
    );
}

#[test]
fn effective_zero_means_no_limit_uses_cap() {
    assert_eq!(
        effective_max_message_size(&sdp_with("0")),
        MAX_DATA_CHANNEL_MESSAGE_SIZE
    );
}

#[test]
fn effective_smaller_value_is_honoured() {
    assert_eq!(effective_max_message_size(&sdp_with("16384")), 16384);
}

#[test]
fn effective_larger_value_is_capped() {
    assert_eq!(
        effective_max_message_size(&sdp_with("1048576")),
        MAX_DATA_CHANNEL_MESSAGE_SIZE
    );
}

#[test]
fn effective_exactly_cap_is_cap() {
    assert_eq!(
        effective_max_message_size(&sdp_with("65536")),
        MAX_DATA_CHANNEL_MESSAGE_SIZE
    );
}

#[test]
fn only_negotiating_or_connected_states_occupy_the_peer_slot() {
    let cases = [
        (WebrtcConnectionState::Unspecified, false),
        (WebrtcConnectionState::New, true),
        (WebrtcConnectionState::Connecting, true),
        (WebrtcConnectionState::Connected, true),
        (WebrtcConnectionState::Disconnected, false),
        (WebrtcConnectionState::Failed, false),
        (WebrtcConnectionState::Closed, false),
    ];

    for (state, expected) in cases {
        assert_eq!(state.occupies_peer_slot(), expected, "state: {state:?}");
    }
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
#[test]
fn connection_state_cell_projects_each_complete_observed_state() {
    let state = ConnectionStateCell::new();
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(WebrtcConnectionState::New, false)
    );

    state.observe_outbound_data_channels(true);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(WebrtcConnectionState::New, true)
    );

    state.observe_webrtc(WebrtcConnectionState::Connected);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(WebrtcConnectionState::Connected, true)
    );

    state.close();
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(WebrtcConnectionState::Closed, false)
    );
    state.observe_outbound_data_channels(true);
    state.observe_webrtc(WebrtcConnectionState::Connected);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(WebrtcConnectionState::Closed, false),
        "late transport events cannot reopen a locally closed state"
    );
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
#[test]
fn failed_state_rejects_late_open_and_can_only_advance_to_closed() {
    let state = ConnectionStateCell::new();
    state.observe_outbound_data_channels(true);
    state.observe_webrtc(WebrtcConnectionState::Failed);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(WebrtcConnectionState::Failed, false)
    );

    state.observe_outbound_data_channels(true);
    state.observe_webrtc(WebrtcConnectionState::Connected);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(WebrtcConnectionState::Failed, false)
    );

    state.observe_webrtc(WebrtcConnectionState::Closed);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(WebrtcConnectionState::Closed, false)
    );
}
