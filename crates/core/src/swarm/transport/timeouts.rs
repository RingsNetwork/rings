use std::time::Duration;

/// Timeout budgets governing transport send admission, delivery, and connection retirement.
pub(super) struct TransportTimeoutProfile {
    /// Maximum time to wait for a send to be accepted by the transport layer.
    pub(super) send_accept: Duration,
    /// Maximum time to wait for first-frame admission on an outbound payload.
    pub(super) first_frame_admission: Duration,
    /// Maximum time allowed for an accepted frame to complete delivery.
    pub(super) delivery: Duration,
    /// Maximum time allowed for a tracked payload send before caller cancellation.
    pub(super) tracked_payload: Duration,
    /// Maximum time allowed to close and retire a transport connection.
    pub(super) close: Duration,
}

/// Shortened timeout profile used by native dummy-transport unit tests.
// The dummy tracked deadline is intentionally shorter than delivery. Tests use
// that inversion to exercise caller cancellation while a delivery remains pending.
const DUMMY_NATIVE_TEST_TIMEOUT_PROFILE: TransportTimeoutProfile = TransportTimeoutProfile {
    send_accept: Duration::from_millis(50),
    first_frame_admission: Duration::from_millis(200),
    delivery: Duration::from_millis(500),
    tracked_payload: Duration::from_millis(200),
    close: Duration::from_millis(100),
};

/// Default timeout profile used for production transports.
const PRODUCTION_TRANSPORT_TIMEOUT_PROFILE: TransportTimeoutProfile = TransportTimeoutProfile {
    send_accept: Duration::from_secs(5),
    first_frame_admission: rings_transport::core::transport::IRREVOCABLE_SEND_COMPLETION_TIMEOUT,
    delivery: rings_transport::core::transport::IRREVOCABLE_SEND_COMPLETION_TIMEOUT,
    tracked_payload: rings_transport::core::transport::IRREVOCABLE_SEND_COMPLETION_TIMEOUT,
    close: rings_transport::core::transport::CONNECTION_RETIRE_TIMEOUT,
};

/// Active timeout profile for the current build configuration.
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(super) const TRANSPORT_TIMEOUT_PROFILE: TransportTimeoutProfile =
    DUMMY_NATIVE_TEST_TIMEOUT_PROFILE;

/// Active timeout profile for the current build configuration.
#[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
pub(super) const TRANSPORT_TIMEOUT_PROFILE: TransportTimeoutProfile =
    PRODUCTION_TRANSPORT_TIMEOUT_PROFILE;

/// How long a frame accepted by the transport may take to be delivered or given up: the bound
/// every per-hop delay of this end is judged against.
pub(crate) const DELIVERY_BOUND: Duration = TRANSPORT_TIMEOUT_PROFILE.delivery;

/// Verify compile-time ordering invariants required of every timeout profile.
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
pub(crate) const CHANNEL_SEND_ACCEPT_BUDGET: Duration = TRANSPORT_TIMEOUT_PROFILE.send_accept;

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
