use std::time::Duration;

pub(super) struct TransportTimeoutProfile {
    pub(super) send_accept: Duration,
    pub(super) delivery: Duration,
    pub(super) tracked_payload: Duration,
    pub(super) close: Duration,
}

const DUMMY_NATIVE_TEST_TIMEOUT_PROFILE: TransportTimeoutProfile = TransportTimeoutProfile {
    send_accept: Duration::from_millis(50),
    delivery: Duration::from_millis(500),
    tracked_payload: Duration::from_millis(100),
    close: Duration::from_millis(100),
};

const PRODUCTION_TRANSPORT_TIMEOUT_PROFILE: TransportTimeoutProfile = TransportTimeoutProfile {
    send_accept: Duration::from_secs(5),
    delivery: Duration::from_secs(25),
    tracked_payload: Duration::from_secs(25),
    close: Duration::from_secs(5),
};

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(super) const TRANSPORT_TIMEOUT_PROFILE: TransportTimeoutProfile =
    DUMMY_NATIVE_TEST_TIMEOUT_PROFILE;

#[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
pub(super) const TRANSPORT_TIMEOUT_PROFILE: TransportTimeoutProfile =
    PRODUCTION_TRANSPORT_TIMEOUT_PROFILE;

const _: () = {
    assert!(
        PRODUCTION_TRANSPORT_TIMEOUT_PROFILE.send_accept.as_millis()
            < PRODUCTION_TRANSPORT_TIMEOUT_PROFILE.delivery.as_millis()
    );
    assert!(
        PRODUCTION_TRANSPORT_TIMEOUT_PROFILE
            .tracked_payload
            .as_millis()
            >= PRODUCTION_TRANSPORT_TIMEOUT_PROFILE.delivery.as_millis()
    );
    assert!(
        PRODUCTION_TRANSPORT_TIMEOUT_PROFILE.close.as_millis()
            <= PRODUCTION_TRANSPORT_TIMEOUT_PROFILE.delivery.as_millis()
    );
    assert!(
        DUMMY_NATIVE_TEST_TIMEOUT_PROFILE.send_accept.as_millis()
            < PRODUCTION_TRANSPORT_TIMEOUT_PROFILE.send_accept.as_millis()
    );
    assert!(
        DUMMY_NATIVE_TEST_TIMEOUT_PROFILE.delivery.as_millis()
            < PRODUCTION_TRANSPORT_TIMEOUT_PROFILE.delivery.as_millis()
    );
    assert!(
        DUMMY_NATIVE_TEST_TIMEOUT_PROFILE
            .tracked_payload
            .as_millis()
            < PRODUCTION_TRANSPORT_TIMEOUT_PROFILE
                .tracked_payload
                .as_millis()
    );
    assert!(
        DUMMY_NATIVE_TEST_TIMEOUT_PROFILE.close.as_millis()
            < PRODUCTION_TRANSPORT_TIMEOUT_PROFILE.close.as_millis()
    );
};

/// Admission budget for one data-channel send in the active transport profile.
///
/// Maintenance scheduling uses the same bound so control-plane work and data
/// admission cannot drift onto independent timeout policies.
pub(crate) const DATA_CHANNEL_SEND_ACCEPT_BUDGET: Duration = TRANSPORT_TIMEOUT_PROFILE.send_accept;
