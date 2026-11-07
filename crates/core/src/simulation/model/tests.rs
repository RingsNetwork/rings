use super::*;

const LIMITS: SimLimits = SimLimits {
    node_bytes: 100,
    global_bytes: 200,
    control_deadline_ms: 25,
    repair_amplification: 1,
};

struct DispatchFixture {
    transfer_id: u64,
    node: SimNodeId,
    class: SimTransferClass,
    identity: SimFrameIdentity,
    bytes: u64,
    deadline: Option<u64>,
}

#[test]
fn transition_is_pure_and_trace_digest_is_replay_stable() {
    let initial = SimState::default();
    let actions = valid_delivery_actions();
    let first = replay(initial.clone(), &actions);
    let second = replay(initial.clone(), &actions);

    assert!(initial.trace().events().is_empty());
    assert_eq!(first, second);
    assert_eq!(
        first.trace().digest().expect("trace must serialize"),
        second.trace().digest().expect("trace must serialize")
    );
    assert!(first.invariant_violations(LIMITS).is_empty());
    assert_eq!(first.class_deliveries(SimTransferClass::Control), 1);
}

#[test]
fn legacy_feedback_loop_requires_one_typed_barrier_causal_chain() {
    let (mut state, reverse_lifecycle) = bidirectional_state(10);
    let (next, _) = submit_admit_dispatch(
        state,
        DispatchFixture {
            transfer_id: 1,
            node: SimNodeId(1),
            class: SimTransferClass::Reassembly,
            identity: reverse_identity(1),
            bytes: 220,
            deadline: None,
        },
        reverse_lifecycle,
    );
    let (next, probe_dispatch) = submit_admit_dispatch(
        next,
        DispatchFixture {
            transfer_id: 2,
            node: SimNodeId(1),
            class: SimTransferClass::Control,
            identity: reverse_identity(2),
            bytes: 1,
            deadline: Some(25),
        },
        reverse_lifecycle,
    );
    let (next, barrier) = transition(next, SimAction::ObserveReassemblyBarrier {
        node: SimNodeId(1),
        control_transfer_id: 2,
        generation: "generation-2".to_owned(),
        transaction_id: Some(transaction_id(2)),
        blocked_control: true,
        causal_parent: Some(probe_dispatch),
    });
    state = apply(next, SimAction::AdvanceVirtualTime { delta_ms: 30 });
    let (next, verdict) = transition(state, SimAction::RunLiveness {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        generation: "generation-1".to_owned(),
        transaction_id: Some(transaction_id(2)),
        removed: true,
        causal_parent: Some(barrier),
    });
    let (next, disconnect) = transition(next, SimAction::Disconnect {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        causal_parent: Some(verdict),
    });
    state = apply(next, SimAction::ScheduleRepair {
        node: SimNodeId(0),
        entries: 11,
        causal_parent: Some(disconnect),
    });

    let violations = state.invariant_violations(LIMITS);
    assert!(has_violation(&violations, |item| matches!(
        item,
        SimInvariantViolation::ControlStarvation { .. }
    )));
    assert!(has_violation(&violations, |item| matches!(
        item,
        SimInvariantViolation::FalseDisconnect { .. }
    )));
    assert!(has_violation(&violations, |item| matches!(
        item,
        SimInvariantViolation::RepairStorm { .. }
    )));
    assert!(violations.contains(&SimInvariantViolation::NoStorageProgress));
}

#[test]
fn explicit_control_deadline_is_enforced_before_the_scenario_limit() {
    let generous_limits = SimLimits {
        control_deadline_ms: 100,
        ..LIMITS
    };
    let state = submit_and_admit(
        active_state(0),
        9,
        SimTransferClass::Control,
        1,
        Some(25),
        SimEventId(1),
    );
    let state = apply(state, SimAction::AdvanceVirtualTime { delta_ms: 30 });

    assert_eq!(state.invariant_violations(generous_limits), vec![
        SimInvariantViolation::ControlStarvation {
            observed_ms: 30,
            deadline_ms: 25,
        }
    ]);
}

#[test]
fn missing_submit_wrong_endpoint_and_duplicate_lifecycle_are_rejected() {
    let state = active_state(0);
    let missing_submit = state.clone().transition(admit_action(
        7,
        SimNodeId(0),
        SimTransferClass::Control,
        fixture_identity(7),
        SimEventId(1),
    ));
    assert!(matches!(
        missing_submit,
        Err(SimTransitionError::InvalidTransferPhase { .. })
    ));

    let mut wrong_identity = fixture_identity(8);
    wrong_identity.peer_did = "did:ring:other".to_owned();
    let wrong_endpoint = state.clone().transition(SimAction::SubmitFrame {
        transfer_id: 8,
        node: SimNodeId(0),
        class: SimTransferClass::Control,
        identity: wrong_identity,
        causal_parent: Some(SimEventId(1)),
    });
    assert!(matches!(
        wrong_endpoint,
        Err(SimTransitionError::InvalidTransferPhase { .. })
    ));

    let duplicate = state.transition(active_lifecycle("generation-2", SimEventId(1)));
    assert!(matches!(
        duplicate,
        Err(SimTransitionError::InvalidLifecycle { .. })
    ));
}

#[test]
fn dispatch_parent_and_closed_generation_are_checked() {
    let state = active_state(0);
    let identity = fixture_identity(1);
    let (state, submit) = transition(state, SimAction::SubmitFrame {
        transfer_id: 1,
        node: SimNodeId(0),
        class: SimTransferClass::Control,
        identity: identity.clone(),
        causal_parent: Some(SimEventId(1)),
    });
    let (state, admission) = transition(
        state,
        admit_action(1, SimNodeId(0), SimTransferClass::Control, identity, submit),
    );
    let wrong_parent = state.clone().transition(SimAction::DispatchFrame {
        transfer_id: 1,
        causal_parent: Some(SimEventId(1)),
    });
    assert!(matches!(
        wrong_parent,
        Err(SimTransitionError::UnexpectedCausalParent { .. })
    ));

    let state = apply(state, closed_lifecycle(admission));
    let after_close = state.transition(SimAction::DispatchFrame {
        transfer_id: 1,
        causal_parent: Some(admission),
    });
    assert!(matches!(
        after_close,
        Err(SimTransitionError::InvalidTransferPhase { .. })
    ));
}

#[test]
fn reassembly_class_duplicate_and_barrier_backlog_are_checked() {
    let (control_only, control_dispatch) = submit_admit_dispatch(
        active_state(0),
        DispatchFixture {
            transfer_id: 1,
            node: SimNodeId(0),
            class: SimTransferClass::Control,
            identity: fixture_identity(1),
            bytes: 1,
            deadline: Some(25),
        },
        SimEventId(1),
    );
    let wrong_class = control_only
        .clone()
        .transition(SimAction::AdvanceReassembly {
            node: SimNodeId(0),
            transfer_id: 1,
            causal_parent: Some(control_dispatch),
        });
    assert!(matches!(
        wrong_class,
        Err(SimTransitionError::InvalidTransferPhase { .. })
    ));
    let no_backlog = control_only.transition(SimAction::ObserveReassemblyBarrier {
        node: SimNodeId(0),
        control_transfer_id: 1,
        generation: "generation-1".to_owned(),
        transaction_id: Some(transaction_id(1)),
        blocked_control: true,
        causal_parent: Some(control_dispatch),
    });
    assert!(matches!(
        no_backlog,
        Err(SimTransitionError::InvalidTransferPhase { .. })
    ));

    let (state, dispatch) = submit_admit_dispatch(
        active_state(0),
        DispatchFixture {
            transfer_id: 2,
            node: SimNodeId(0),
            class: SimTransferClass::Reassembly,
            identity: fixture_identity(2),
            bytes: 8,
            deadline: None,
        },
        SimEventId(1),
    );
    let (state, advance) = transition(state, SimAction::AdvanceReassembly {
        node: SimNodeId(0),
        transfer_id: 2,
        causal_parent: Some(dispatch),
    });
    let duplicate = state.transition(SimAction::AdvanceReassembly {
        node: SimNodeId(0),
        transfer_id: 2,
        causal_parent: Some(advance),
    });
    assert!(matches!(
        duplicate,
        Err(SimTransitionError::InvalidTransferPhase { .. })
    ));
}

#[test]
fn liveness_requires_exact_delivered_probe_and_rejects_repeat() {
    let (state, reverse_lifecycle) = bidirectional_state(0);
    let (state, dispatch) = submit_admit_dispatch(
        state,
        DispatchFixture {
            transfer_id: 3,
            node: SimNodeId(1),
            class: SimTransferClass::Control,
            identity: reverse_identity(3),
            bytes: 1,
            deadline: Some(25),
        },
        reverse_lifecycle,
    );
    let premature = state.clone().transition(healthy_liveness(3, dispatch));
    assert!(matches!(
        premature,
        Err(SimTransitionError::InvalidLivenessVerdict { .. })
    ));
    let (state, delivered) = transition(state, SimAction::DeliverFrame {
        transfer_id: 3,
        causal_parent: Some(dispatch),
    });
    let wrong_parent = state.clone().transition(healthy_liveness(3, dispatch));
    assert!(matches!(
        wrong_parent,
        Err(SimTransitionError::UnexpectedCausalParent { .. })
    ));
    let (state, verdict) = transition(state, healthy_liveness(3, delivered));
    let repeated = state.transition(healthy_liveness(3, verdict));
    assert!(matches!(
        repeated,
        Err(SimTransitionError::InvalidLivenessVerdict { .. })
    ));
}

#[test]
fn disconnect_and_repair_require_exact_causal_parents() {
    let (state, verdict) = removed_liveness_state();
    let wrong_disconnect = state.clone().transition(SimAction::Disconnect {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        causal_parent: Some(SimEventId(1)),
    });
    assert!(matches!(
        wrong_disconnect,
        Err(SimTransitionError::UnexpectedCausalParent { .. })
    ));
    let (state, disconnect) = transition(state, SimAction::Disconnect {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        causal_parent: Some(verdict),
    });
    let wrong_repair = state.transition(SimAction::ScheduleRepair {
        node: SimNodeId(0),
        entries: 1,
        causal_parent: Some(verdict),
    });
    assert!(matches!(
        wrong_repair,
        Err(SimTransitionError::UnexpectedCausalParent {
            expected,
            observed: Some(observed),
        }) if expected == disconnect && observed == verdict
    ));
}

#[test]
fn failure_and_disconnect_rejections_preserve_domain_semantics() {
    let inactive = SimState::default().transition(SimAction::InjectPeerFailure {
        node: SimNodeId(0),
        peer: SimNodeId(1),
    });
    assert!(matches!(
        inactive,
        Err(SimTransitionError::InvalidFailureInjection {
            node: SimNodeId(0),
            peer: SimNodeId(1),
        })
    ));

    let state = active_state(0);
    let healthy = state.clone().transition(SimAction::Disconnect {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        causal_parent: Some(SimEventId(1)),
    });
    assert!(matches!(
        healthy,
        Err(SimTransitionError::InvalidDisconnect {
            reason: SimDisconnectReason::HealthyPeer,
            ..
        })
    ));

    let (state, failure) = transition(state, SimAction::InjectPeerFailure {
        node: SimNodeId(0),
        peer: SimNodeId(1),
    });
    let (state, _) = transition(state, SimAction::Disconnect {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        causal_parent: Some(failure),
    });
    let duplicate = state.transition(SimAction::Disconnect {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        causal_parent: Some(failure),
    });
    assert!(matches!(
        duplicate,
        Err(SimTransitionError::InvalidDisconnect {
            reason: SimDisconnectReason::Duplicate,
            ..
        })
    ));

    let mut missing_cause = active_state(0);
    missing_cause
        .peer_health
        .insert((SimNodeId(0), SimNodeId(1)), SimPeerHealth::Failed);
    let missing_cause = missing_cause.transition(SimAction::Disconnect {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        causal_parent: Some(SimEventId(1)),
    });
    assert!(matches!(
        missing_cause,
        Err(SimTransitionError::InvalidDisconnect {
            reason: SimDisconnectReason::MissingCause,
            ..
        })
    ));
}

#[test]
fn stop_repair_and_yield_preconditions_are_checked() {
    let state = active_state(0);
    let (state, stop) = transition(state, SimAction::StopStorm {
        node: SimNodeId(0),
        causal_parent: Some(SimEventId(1)),
    });
    let duplicate_stop = state.clone().transition(SimAction::StopStorm {
        node: SimNodeId(0),
        causal_parent: Some(stop),
    });
    assert!(matches!(
        duplicate_stop,
        Err(SimTransitionError::ActionAfterStop { .. })
    ));
    let late_submit = state.clone().transition(SimAction::SubmitFrame {
        transfer_id: 1,
        node: SimNodeId(0),
        class: SimTransferClass::Control,
        identity: fixture_identity(1),
        causal_parent: Some(stop),
    });
    assert!(matches!(
        late_submit,
        Err(SimTransitionError::ActionAfterStop { .. })
    ));
    assert!(matches!(
        state.clone().transition(SimAction::ScheduleRepair {
            node: SimNodeId(0),
            entries: 1,
            causal_parent: Some(stop),
        }),
        Err(SimTransitionError::MissingRepairIntent { .. })
    ));
    assert!(matches!(
        state.transition(SimAction::YieldActor {
            node: SimNodeId(0),
            causal_parent: Some(stop),
        }),
        Err(SimTransitionError::YieldWithoutPersistence { .. })
    ));
}

fn removed_liveness_state() -> (SimState, SimEventId) {
    let (state, reverse_lifecycle) = bidirectional_state(0);
    let (state, _) = submit_admit_dispatch(
        state,
        DispatchFixture {
            transfer_id: 1,
            node: SimNodeId(1),
            class: SimTransferClass::Reassembly,
            identity: reverse_identity(1),
            bytes: 8,
            deadline: None,
        },
        reverse_lifecycle,
    );
    let (state, probe) = submit_admit_dispatch(
        state,
        DispatchFixture {
            transfer_id: 2,
            node: SimNodeId(1),
            class: SimTransferClass::Control,
            identity: reverse_identity(2),
            bytes: 1,
            deadline: Some(25),
        },
        reverse_lifecycle,
    );
    let (state, barrier) = transition(state, SimAction::ObserveReassemblyBarrier {
        node: SimNodeId(1),
        control_transfer_id: 2,
        generation: "generation-2".to_owned(),
        transaction_id: Some(transaction_id(2)),
        blocked_control: true,
        causal_parent: Some(probe),
    });
    let state = apply(state, SimAction::AdvanceVirtualTime { delta_ms: 30 });
    transition(state, SimAction::RunLiveness {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        generation: "generation-1".to_owned(),
        transaction_id: Some(transaction_id(2)),
        removed: true,
        causal_parent: Some(barrier),
    })
}

fn valid_delivery_actions() -> Vec<SimAction> {
    let identity = fixture_identity(7);
    vec![
        SimAction::InjectSync {
            node: SimNodeId(0),
            entries: 2,
        },
        active_lifecycle("generation-1", SimEventId(0)),
        SimAction::SubmitFrame {
            transfer_id: 7,
            node: SimNodeId(0),
            class: SimTransferClass::Control,
            identity: identity.clone(),
            causal_parent: Some(SimEventId(1)),
        },
        admit_action(
            7,
            SimNodeId(0),
            SimTransferClass::Control,
            identity,
            SimEventId(2),
        ),
        SimAction::AdvanceVirtualTime { delta_ms: 5 },
        SimAction::DispatchFrame {
            transfer_id: 7,
            causal_parent: Some(SimEventId(3)),
        },
        SimAction::DeliverFrame {
            transfer_id: 7,
            causal_parent: Some(SimEventId(5)),
        },
        SimAction::PersistOneEntry {
            node: SimNodeId(0),
            causal_parent: Some(SimEventId(6)),
        },
        SimAction::YieldActor {
            node: SimNodeId(0),
            causal_parent: Some(SimEventId(7)),
        },
    ]
}

fn bidirectional_state(entries: u64) -> (SimState, SimEventId) {
    let state = active_state(entries);
    transition(state, SimAction::ConnectionLifecycle {
        node: SimNodeId(1),
        peer: SimNodeId(0),
        local_did: "did:ring:peer".to_owned(),
        peer_did: "did:ring:local".to_owned(),
        generation: "generation-2".to_owned(),
        state: SimConnectionState::Active,
        causal_parent: Some(SimEventId(1)),
    })
}

fn active_state(entries: u64) -> SimState {
    replay(SimState::default(), &[
        SimAction::InjectSync {
            node: SimNodeId(0),
            entries,
        },
        active_lifecycle("generation-1", SimEventId(0)),
    ])
}

fn active_lifecycle(generation: &str, parent: SimEventId) -> SimAction {
    SimAction::ConnectionLifecycle {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        local_did: "did:ring:local".to_owned(),
        peer_did: "did:ring:peer".to_owned(),
        generation: generation.to_owned(),
        state: SimConnectionState::Active,
        causal_parent: Some(parent),
    }
}

fn closed_lifecycle(parent: SimEventId) -> SimAction {
    SimAction::ConnectionLifecycle {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        local_did: "did:ring:local".to_owned(),
        peer_did: "did:ring:peer".to_owned(),
        generation: "generation-1".to_owned(),
        state: SimConnectionState::Closed,
        causal_parent: Some(parent),
    }
}

fn submit_and_admit(
    state: SimState,
    transfer_id: u64,
    class: SimTransferClass,
    bytes: u64,
    deadline: Option<u64>,
    parent: SimEventId,
) -> SimState {
    let identity = fixture_identity(transfer_id);
    let (state, submit) = transition(state, SimAction::SubmitFrame {
        transfer_id,
        node: SimNodeId(0),
        class,
        identity: identity.clone(),
        causal_parent: Some(parent),
    });
    apply(state, SimAction::AdmitFrame {
        transfer_id,
        node: SimNodeId(0),
        class,
        bytes,
        enqueued_virtual_ms: 0,
        deadline_virtual_ms: deadline,
        identity,
        causal_parent: Some(submit),
    })
}

fn submit_admit_dispatch(
    state: SimState,
    fixture: DispatchFixture,
    parent: SimEventId,
) -> (SimState, SimEventId) {
    let DispatchFixture {
        transfer_id,
        node,
        class,
        identity,
        bytes,
        deadline,
    } = fixture;
    let (state, submit) = transition(state, SimAction::SubmitFrame {
        transfer_id,
        node,
        class,
        identity: identity.clone(),
        causal_parent: Some(parent),
    });
    let (state, admission) = transition(state, SimAction::AdmitFrame {
        transfer_id,
        node,
        class,
        bytes,
        enqueued_virtual_ms: 0,
        deadline_virtual_ms: deadline,
        identity,
        causal_parent: Some(submit),
    });
    transition(state, SimAction::DispatchFrame {
        transfer_id,
        causal_parent: Some(admission),
    })
}

fn admit_action(
    transfer_id: u64,
    node: SimNodeId,
    class: SimTransferClass,
    identity: SimFrameIdentity,
    parent: SimEventId,
) -> SimAction {
    SimAction::AdmitFrame {
        transfer_id,
        node,
        class,
        bytes: 1,
        enqueued_virtual_ms: 0,
        deadline_virtual_ms: Some(25),
        identity,
        causal_parent: Some(parent),
    }
}

fn healthy_liveness(sequence: u64, parent: SimEventId) -> SimAction {
    SimAction::RunLiveness {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        generation: "generation-1".to_owned(),
        transaction_id: Some(transaction_id(sequence)),
        removed: false,
        causal_parent: Some(parent),
    }
}

fn replay(mut state: SimState, actions: &[SimAction]) -> SimState {
    for action in actions {
        state = apply(state, action.clone());
    }
    state
}

fn transition(state: SimState, action: SimAction) -> (SimState, SimEventId) {
    let (state, output) = state
        .transition(action)
        .expect("fixture transition must be structurally valid");
    (state, output.event_id)
}

fn apply(state: SimState, action: SimAction) -> SimState {
    transition(state, action).0
}

fn fixture_identity(sequence: u64) -> SimFrameIdentity {
    SimFrameIdentity {
        local_did: "did:ring:local".to_owned(),
        peer_did: "did:ring:peer".to_owned(),
        connection_generation: "generation-1".to_owned(),
        transaction_id: Some(transaction_id(sequence)),
    }
}

fn reverse_identity(sequence: u64) -> SimFrameIdentity {
    SimFrameIdentity {
        local_did: "did:ring:peer".to_owned(),
        peer_did: "did:ring:local".to_owned(),
        connection_generation: "generation-2".to_owned(),
        transaction_id: Some(transaction_id(sequence)),
    }
}

const fn transaction_id(sequence: u64) -> uuid::Uuid {
    uuid::Uuid::from_u128(sequence as u128 + 1)
}

fn has_violation(
    violations: &[SimInvariantViolation],
    predicate: impl Fn(&SimInvariantViolation) -> bool,
) -> bool {
    violations.iter().any(predicate)
}

#[test]
fn join_leave_crash_and_rejoin_lifecycle_transitions_are_checked() {
    let bad_seed = SimState::default().transition(SimAction::Join {
        node: SimNodeId(0),
        via: SimNodeId(1),
    });
    assert!(matches!(
        bad_seed,
        Err(SimTransitionError::InactiveBootstrapPeer {
            node: SimNodeId(0),
            via: SimNodeId(1),
        })
    ));

    let (state, join_0) = transition(SimState::default(), SimAction::Join {
        node: SimNodeId(0),
        via: SimNodeId(0),
    });
    assert!(matches!(
        state.clone().transition(SimAction::Join {
            node: SimNodeId(0),
            via: SimNodeId(0),
        }),
        Err(SimTransitionError::NodeAlreadyLive { node: SimNodeId(0) })
    ));
    assert!(matches!(
        state.clone().transition(SimAction::Join {
            node: SimNodeId(1),
            via: SimNodeId(2),
        }),
        Err(SimTransitionError::InactiveBootstrapPeer {
            node: SimNodeId(1),
            via: SimNodeId(2),
        })
    ));

    let (state, _) = transition(state, SimAction::Join {
        node: SimNodeId(1),
        via: SimNodeId(0),
    });
    let state = apply(state, active_lifecycle("generation-1", join_0));
    let (state, leave_1) = transition(state, SimAction::Leave {
        node: SimNodeId(1),
        graceful: true,
    });
    assert!(matches!(
        state
            .clone()
            .transition(SimAction::Crash { node: SimNodeId(1) }),
        Err(SimTransitionError::NodeNotLive { node: SimNodeId(1) })
    ));
    let (state, disconnect_1) = transition(state, SimAction::Disconnect {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        causal_parent: Some(leave_1),
    });
    let state = apply(state, closed_lifecycle(disconnect_1));
    assert!(state.invariant_violations(LIMITS).is_empty());

    let reused_gen = state
        .clone()
        .transition(active_lifecycle("generation-1", disconnect_1));
    assert!(matches!(
        reused_gen,
        Err(SimTransitionError::InvalidLifecycle { .. })
    ));

    let (state, rejoin_1) = transition(state, SimAction::Join {
        node: SimNodeId(1),
        via: SimNodeId(0),
    });
    let state = apply(state, active_lifecycle("generation-2", rejoin_1));
    let (state, crash_1) = transition(state, SimAction::Crash { node: SimNodeId(1) });
    let (state, _) = transition(state, SimAction::Disconnect {
        node: SimNodeId(0),
        peer: SimNodeId(1),
        causal_parent: Some(crash_1),
    });
    assert!(state.invariant_violations(LIMITS).is_empty());
    assert!(state.is_population_conserved());
    assert_eq!(state.joins(), 3);
    assert_eq!(state.graceful_leaves(), 1);
    assert_eq!(state.crashes(), 1);
}

#[test]
fn weibull_session_generator_matches_median_and_preserves_stationarity() {
    assert!(matches!(
        WeibullChurnConfig::new(0, 300_000, 0.5),
        Err(WeibullChurnConfigError::ZeroTargetPopulation)
    ));
    assert!(matches!(
        WeibullChurnConfig::new(10, 30_000, 0.5),
        Err(WeibullChurnConfigError::MedianSessionOutOfRange { .. })
    ));
    assert!(matches!(
        WeibullChurnConfig::new(10, 300_000, 0.5).and_then(|cfg| cfg.with_shape(0.25)),
        Err(WeibullChurnConfigError::InvalidShape { .. })
    ));

    let median_ms = 300_000_u64;
    let config = WeibullChurnConfig::new(10, median_ms, 0.5)
        .expect("5 min median session config must be valid");
    let mut generator = WeibullSessionGenerator::new(0x7730_0001, config);
    assert_eq!(*generator.config(), config);
    let mut samples: Vec<u64> = (0..2_001)
        .map(|_| generator.sample_weibull_lifetime_ms())
        .collect();
    samples.sort_unstable();
    let empirical_median = samples.get(samples.len() / 2).copied().unwrap_or_default();
    let relative_error = (empirical_median as f64 - median_ms as f64).abs() / (median_ms as f64);
    assert!(
        relative_error < 0.12,
        "empirical Weibull median {empirical_median} ms drifted too far from {median_ms} ms"
    );

    let mut schedule_gen_a = WeibullSessionGenerator::new(0x7730_0002, config);
    let mut schedule_gen_b = WeibullSessionGenerator::new(0x7730_0002, config);
    let schedule_a = schedule_gen_a.generate_schedule(600_000);
    let schedule_b = schedule_gen_b.generate_schedule(600_000);
    assert_eq!(schedule_a, schedule_b);

    let mut state = SimState::default();
    let mut cursor_ms = 0_u64;
    let mut population_samples = Vec::new();
    for event in &schedule_a {
        if event.virtual_ms > cursor_ms {
            state = apply(state, SimAction::AdvanceVirtualTime {
                delta_ms: event.virtual_ms - cursor_ms,
            });
            cursor_ms = event.virtual_ms;
            population_samples.push(state.live_nodes().len());
        }
        state = apply(state, event.action.clone());
        assert!(state.is_population_conserved());
    }
    assert!(state.joins() > 10);
    assert!(state.graceful_leaves() > 0);
    assert!(state.crashes() + state.ungraceful_leaves() > 0);
    let mean_population = (population_samples.iter().sum::<usize>() as f64)
        / (population_samples.len().max(1) as f64);
    assert!(
        (7.0..=13.5).contains(&mean_population),
        "stationary population mean {mean_population:.2} drifted from target N=10"
    );
}
