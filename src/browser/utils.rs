use crate::prelude::web_sys::RtcIceConnectionState;

pub fn set_panic_hook() {
    // When the `console_error_panic_hook` feature is enabled, we can call the
    // `set_panic_hook` function at least once during initialization, and then
    // we will get better error messages if our code ever panics.
    //
    // For more details see
    // https://github.com/rustwasm/console_error_panic_hook#readme
    #[cfg(feature = "console_error_panic_hook")]
    console_error_panic_hook::set_once();
}

pub fn from_rtc_ice_connection_state(state: RtcIceConnectionState) -> String {
    match state {
        RtcIceConnectionState::New => "new",
        RtcIceConnectionState::Checking => "checking",
        RtcIceConnectionState::Connected => "connected",
        RtcIceConnectionState::Completed => "completed",
        RtcIceConnectionState::Failed => "failed",
        RtcIceConnectionState::Disconnected => "disconnected",
        RtcIceConnectionState::Closed => "closed",
        _ => "unknown",
    }
    .to_owned()
}

pub fn into_rtc_ice_connection_state(value: &str) -> Option<RtcIceConnectionState> {
    Some(match value {
        "new" => RtcIceConnectionState::New,
        "checking" => RtcIceConnectionState::Checking,
        "connected" => RtcIceConnectionState::Connected,
        "completed" => RtcIceConnectionState::Completed,
        "failed" => RtcIceConnectionState::Failed,
        "disconnected" => RtcIceConnectionState::Disconnected,
        "closed" => RtcIceConnectionState::Closed,
        _ => return None,
    })
}
