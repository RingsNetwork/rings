//! Deterministic fleet-level laws for finger convergence scheduling.
//!
//! The fixture samples fresh lifecycle, saturated retry, and browser-resume
//! deadlines to bound both per-node timing and aggregate one-second pressure.

use super::*;
use crate::dht::finger_schedule_deadline_for_test;
use crate::dht::finger_schedule_resumed_deadline_for_test;

/// Fleet smoothing window for a fresh lifecycle.
const FINGER_INITIAL_PHASE_MS: u64 = 10_000;
/// Saturated retry floor; the jitter window extends up to twice this value.
const FINGER_MAX_RETRY_FLOOR_MS: u64 = 60_000;
/// Fixture-level bound for one-second buckets across 200 deterministic nodes.
const FINGER_MAX_FIXTURE_NODES_PER_SECOND: usize = 30;

/// Law: lifecycle entropy, rather than a grindable DID alone, selects a
/// deadline inside the explicit per-node initial and retry windows. Browser
/// resume rephases stale work over the same fleet window instead of emitting
/// one immediate request per resumed node. The distribution assertions also
/// bound the number of deterministic fixture nodes sharing any one-second bucket.
#[test]
fn test_finger_convergence_schedule_has_per_node_churn_bounds_and_lifecycle_entropy() {
    // Sets witness spread across exact deadlines; buckets witness per-second
    // fleet pressure instead of uniqueness alone.
    let mut first_lifecycle_deadlines = BTreeSet::new();
    let mut resumed_delays = BTreeSet::new();
    let mut initial_bucket_counts = BTreeMap::<u64, usize>::new();
    let mut resumed_bucket_counts = BTreeMap::<u64, usize>::new();
    // Same DID, different lifecycle UUID. This catches grindable identity-only
    // schedules that would let an operator preselect its maintenance slot.
    let mut entropy_changed_deadline = 0usize;
    for identity in 0..200u32 {
        let local = crate::dht::Did::from(identity);
        let first_boot = uuid::Uuid::from_u128(u128::from(identity).saturating_add(1));
        let second_boot = uuid::Uuid::from_u128(u128::from(identity).saturating_add(10_001));
        let initial = finger_schedule_deadline_for_test(local, first_boot, 0);
        let another_initial = finger_schedule_deadline_for_test(local, second_boot, 0);
        let retry = finger_schedule_deadline_for_test(local, first_boot, u8::MAX);
        let (resumed_at, resumed_deadline) =
            finger_schedule_resumed_deadline_for_test(local, first_boot);

        assert!((1_000..=1_000 + FINGER_INITIAL_PHASE_MS).contains(&initial));
        assert!((1_000..=1_000 + FINGER_INITIAL_PHASE_MS).contains(&another_initial));
        assert!(
            (FINGER_MAX_RETRY_FLOOR_MS..=FINGER_MAX_RETRY_FLOOR_MS.saturating_mul(2))
                .contains(&retry)
        );
        assert!(resumed_deadline > resumed_at);
        assert!((1_000..=1_000 + FINGER_INITIAL_PHASE_MS)
            .contains(&resumed_deadline.saturating_sub(resumed_at)));
        first_lifecycle_deadlines.insert(initial);
        let resumed_delay = resumed_deadline.saturating_sub(resumed_at);
        resumed_delays.insert(resumed_delay);
        *initial_bucket_counts.entry(initial / 1_000).or_default() += 1;
        *resumed_bucket_counts
            .entry(resumed_delay / 1_000)
            .or_default() += 1;
        entropy_changed_deadline =
            entropy_changed_deadline.saturating_add(usize::from(initial != another_initial));
    }
    assert!(
        first_lifecycle_deadlines.len() >= 190,
        "lifecycle fixture clustered 200 nodes into only {} deadlines",
        first_lifecycle_deadlines.len()
    );
    assert!(
        resumed_delays.len() >= 190,
        "resume fixture clustered 200 nodes into only {} delays",
        resumed_delays.len()
    );
    assert!(
        initial_bucket_counts
            .values()
            .chain(resumed_bucket_counts.values())
            .all(|count| *count <= FINGER_MAX_FIXTURE_NODES_PER_SECOND),
        "lifecycle or resume fixture exceeded {FINGER_MAX_FIXTURE_NODES_PER_SECOND} due nodes in one second"
    );
    assert!(
        entropy_changed_deadline >= 190,
        "lifecycle entropy changed only {entropy_changed_deadline} of 200 deadlines"
    );
}
