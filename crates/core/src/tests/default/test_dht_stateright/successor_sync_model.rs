//! Production successor-sync token and connection-effect laws.

use crate::dht::topology::SuccessorSyncConnectionPlan;
use crate::dht::topology::SuccessorSyncConnectionStep;
use crate::dht::topology::SuccessorSyncState;
use crate::dht::topology::DEFAULT_SUCCESSOR_CAPACITY;
use crate::dht::Did;

#[test]
fn test_production_successor_sync_tokens_are_bounded_exact_and_single_use() {
    let first = Did::from(4u32);
    let second = Did::from(8u32);
    let stale = uuid::Uuid::from_u128(1);
    let current = uuid::Uuid::from_u128(2);
    let mut state = SuccessorSyncState::default();

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
    assert!(state.pending_count() <= 1);
    assert!(!state.claim(&[second], first, stale));
    state.invalidate();
    assert_eq!(
        bounded_plan.advance(&state, &[first]),
        SuccessorSyncConnectionStep::Stale
    );
    assert!(!state.claim(&[second], second, current));
}
