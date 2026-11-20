#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use super::ConnectionStateCell;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use super::ConnectionStateSnapshot;
use super::TransportConnectionState;

#[test]
fn test_only_negotiating_or_connected_states_occupy_the_peer_slot() {
    let cases = [
        (TransportConnectionState::Unspecified, false),
        (TransportConnectionState::New, true),
        (TransportConnectionState::Connecting, true),
        (TransportConnectionState::Connected, true),
        (TransportConnectionState::Disconnected, false),
        (TransportConnectionState::Failed, false),
        (TransportConnectionState::Closed, false),
    ];

    for (state, expected) in cases {
        assert_eq!(state.occupies_peer_slot(), expected, "state: {state:?}");
    }
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
#[test]
fn test_connection_state_cell_projects_each_complete_observed_state() {
    let state = ConnectionStateCell::new();
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(TransportConnectionState::New, false)
    );

    state.observe_outbound_data_channels(true);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(TransportConnectionState::New, true)
    );

    state.observe_webrtc(TransportConnectionState::Connected);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(TransportConnectionState::Connected, true)
    );

    state.close();
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(TransportConnectionState::Closed, false)
    );
    state.observe_outbound_data_channels(true);
    state.observe_webrtc(TransportConnectionState::Connected);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(TransportConnectionState::Closed, false),
        "late transport events cannot reopen a locally closed state"
    );
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
#[test]
fn test_failed_state_rejects_late_open_and_can_only_advance_to_closed() {
    let state = ConnectionStateCell::new();
    state.observe_outbound_data_channels(true);
    state.observe_webrtc(TransportConnectionState::Failed);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(TransportConnectionState::Failed, false)
    );

    state.observe_outbound_data_channels(true);
    state.observe_webrtc(TransportConnectionState::Connected);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(TransportConnectionState::Failed, false)
    );

    state.observe_webrtc(TransportConnectionState::Closed);
    assert_eq!(
        state.snapshot(),
        ConnectionStateSnapshot::new(TransportConnectionState::Closed, false)
    );
}
