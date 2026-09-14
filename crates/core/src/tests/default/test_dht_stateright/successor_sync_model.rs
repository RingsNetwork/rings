//! Production successor-sync token and connection-effect laws.
//!
//! The model is deliberately small: one state value, two reporters, and two
//! UUIDs. It witnesses the claim discipline that protects the real connection
//! effects from stale successor-list reports.

use crate::dht::topology::SuccessorSyncConnectionPlan;
use crate::dht::topology::SuccessorSyncConnectionStep;
use crate::dht::topology::SuccessorSyncState;
use crate::dht::topology::DEFAULT_SUCCESSOR_CAPACITY;
use crate::dht::Did;

#[test]
fn test_production_successor_sync_tokens_are_bounded_exact_and_single_use() {
    let first = Did::from(4u32);
    let second = Did::from(8u32);
    // `stale` is superseded before claim; only `current` may authorize effects.
    let stale = uuid::Uuid::from_u128(1);
    let current = uuid::Uuid::from_u128(2);
    let mut state = SuccessorSyncState::default();

    // Two begins for the same reporter leave exactly one claimable token.
    assert!(state.begin(&[first], first, stale));
    assert!(state.begin(&[first], first, current));
    assert!(!state.claim(&[first], first, stale));
    assert!(state.claim(&[first], first, current));
    assert!(!state.claim(&[first], first, current));
    let mut plan = SuccessorSyncConnectionPlan::new(
        first,
        current,
        (1..=10u32).map(Did::from),
        Did::from(0u32),
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let reserved_before_churn = plan.advance(&state, &[first]);
    assert!(matches!(
        reserved_before_churn,
        SuccessorSyncConnectionStep::Connect(_)
    ));
    state.invalidate();
    // A permit already returned to the caller may finish, but the old report
    // must not authorize any additional candidate after churn.
    let completed_after_churn = usize::from(matches!(
        reserved_before_churn,
        SuccessorSyncConnectionStep::Connect(_)
    ));
    assert_eq!(completed_after_churn, 1);
    assert_eq!(
        plan.advance(&state, &[first]),
        SuccessorSyncConnectionStep::Stale
    );

    assert!(state.begin(&[first], first, current));
    assert!(state.claim(&[first], first, current));
    // The plan can admit at most the configured successor capacity even when
    // the report carries a longer candidate list.
    let mut bounded_plan = SuccessorSyncConnectionPlan::new(
        first,
        current,
        (1..=10u32).map(Did::from),
        Did::from(0u32),
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    for _ in 0..DEFAULT_SUCCESSOR_CAPACITY {
        assert!(matches!(
            bounded_plan.advance(&state, &[first]),
            SuccessorSyncConnectionStep::Connect(_)
        ));
    }
    assert_eq!(
        bounded_plan.advance(&state, &[first]),
        SuccessorSyncConnectionStep::Complete
    );

    assert!(state.begin(&[first], first, stale));
    assert!(state.begin(&[second], second, current));
    // A new current successor reporter replaces the pending proof entirely.
    assert!(state.pending_count() <= 1);
    assert!(!state.claim(&[second], first, stale));
    state.invalidate();
    assert_eq!(
        bounded_plan.advance(&state, &[first]),
        SuccessorSyncConnectionStep::Stale
    );
    assert!(!state.claim(&[second], second, current));
}
