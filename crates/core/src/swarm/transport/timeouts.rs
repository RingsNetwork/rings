use std::time::Duration;

pub(super) struct TransportTimeoutProfile {
    pub(super) send_accept: Duration,
    pub(super) first_frame_admission: Duration,
    pub(super) delivery: Duration,
    pub(super) tracked_payload: Duration,
    pub(super) close: Duration,
}

// The dummy tracked deadline is intentionally shorter than delivery. Tests use
// that inversion to exercise caller cancellation while a delivery remains pending.
const DUMMY_NATIVE_TEST_TIMEOUT_PROFILE: TransportTimeoutProfile = TransportTimeoutProfile {
    send_accept: Duration::from_millis(50),
    first_frame_admission: Duration::from_millis(200),
    delivery: Duration::from_millis(500),
    tracked_payload: Duration::from_millis(200),
    close: Duration::from_millis(100),
};

const PRODUCTION_TRANSPORT_TIMEOUT_PROFILE: TransportTimeoutProfile = TransportTimeoutProfile {
    send_accept: Duration::from_secs(5),
    first_frame_admission: rings_transport::core::transport::IRREVOCABLE_SEND_COMPLETION_TIMEOUT,
    delivery: rings_transport::core::transport::IRREVOCABLE_SEND_COMPLETION_TIMEOUT,
    tracked_payload: rings_transport::core::transport::IRREVOCABLE_SEND_COMPLETION_TIMEOUT,
    close: rings_transport::core::transport::CONNECTION_RETIRE_TIMEOUT,
};

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(super) const TRANSPORT_TIMEOUT_PROFILE: TransportTimeoutProfile =
    DUMMY_NATIVE_TEST_TIMEOUT_PROFILE;

#[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
pub(super) const TRANSPORT_TIMEOUT_PROFILE: TransportTimeoutProfile =
    PRODUCTION_TRANSPORT_TIMEOUT_PROFILE;

const fn assert_common_profile_laws(profile: &TransportTimeoutProfile) {
    assert!(profile.send_accept.as_millis() < profile.first_frame_admission.as_millis());
    assert!(
        profile.send_accept.as_millis() + profile.close.as_millis()
            < profile.first_frame_admission.as_millis()
    );
    assert!(
        profile.send_accept.as_millis() + profile.close.as_millis()
            < profile.tracked_payload.as_millis()
    );
    assert!(profile.close.as_millis() <= profile.delivery.as_millis());
}

const _: () = {
    assert_common_profile_laws(&PRODUCTION_TRANSPORT_TIMEOUT_PROFILE);
    assert_common_profile_laws(&DUMMY_NATIVE_TEST_TIMEOUT_PROFILE);
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
        DUMMY_NATIVE_TEST_TIMEOUT_PROFILE.send_accept.as_millis()
            < PRODUCTION_TRANSPORT_TIMEOUT_PROFILE.send_accept.as_millis()
    );
    assert!(
        DUMMY_NATIVE_TEST_TIMEOUT_PROFILE
            .first_frame_admission
            .as_millis()
            < PRODUCTION_TRANSPORT_TIMEOUT_PROFILE
                .first_frame_admission
                .as_millis()
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
    assert!(
        DUMMY_NATIVE_TEST_TIMEOUT_PROFILE
            .first_frame_admission
            .as_millis()
            < DUMMY_NATIVE_TEST_TIMEOUT_PROFILE.delivery.as_millis()
    );
    assert!(
        DUMMY_NATIVE_TEST_TIMEOUT_PROFILE
            .first_frame_admission
            .as_millis()
            == DUMMY_NATIVE_TEST_TIMEOUT_PROFILE
                .tracked_payload
                .as_millis()
    );
};

/// Cancellation-decision budget for one data-channel send in the active profile.
///
/// Maintenance scheduling uses the same bound so control-plane work and data
/// admission cannot drift onto independent timeout policies. Once this interval
/// expires, an irrevocable send makes its connection generation terminal before
/// bounded close cleanup; the lane remains occupied for at most one additional
/// close interval.
pub(crate) const DATA_CHANNEL_SEND_ACCEPT_BUDGET: Duration = TRANSPORT_TIMEOUT_PROFILE.send_accept;

/// Cleanup grace after an outbound payload's admission deadline expires.
///
/// An already accepted transport send cannot be abandoned. The caller waits
/// for generation terminalization and bounded close before returning.
pub(crate) const OUTBOUND_PAYLOAD_CLEANUP_GRACE: Duration = TRANSPORT_TIMEOUT_PROFILE.close;

/// Maximum configured tracked-payload latency including terminal cleanup.
pub(crate) const TRACKED_PAYLOAD_COMPLETION_BOUND: Duration = TRANSPORT_TIMEOUT_PROFILE
    .tracked_payload
    .saturating_add(OUTBOUND_PAYLOAD_CLEANUP_GRACE)
    .saturating_add(TRANSPORT_TIMEOUT_PROFILE.close);
