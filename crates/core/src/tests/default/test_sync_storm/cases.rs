use super::*;

#[tokio::test(start_paused = true)]
async fn test_legacy_all_disabled_reproduces_complete_feedback_loop() {
    let state = legacy_feedback_loop_state().await;
    let violations = state.invariant_violations(MODEL_LIMITS);
    assert!(violations
        .iter()
        .any(|violation| matches!(violation, SimInvariantViolation::ControlStarvation { .. })));
    assert!(violations
        .iter()
        .any(|violation| matches!(violation, SimInvariantViolation::FalseDisconnect { .. })));
    assert!(violations
        .iter()
        .any(|violation| matches!(violation, SimInvariantViolation::RepairStorm { .. })));
    assert!(violations
        .iter()
        .any(|violation| matches!(violation, SimInvariantViolation::NoStorageProgress)));
}

#[tokio::test(start_paused = true)]
async fn test_n10_single_ablation_matrix_violates_each_protected_proposition() {
    let cases = [
        (
            ProtectionProfile::without_class_reservations(),
            ProtectionLayer::ClassReservations,
        ),
        (
            ProtectionProfile::without_bounded_control_burst(),
            ProtectionLayer::BoundedControlBurst,
        ),
        (
            ProtectionProfile::without_barrier_control_exemption(),
            ProtectionLayer::BarrierControlExemption,
        ),
        (
            ProtectionProfile::without_per_entry_yield(),
            ProtectionLayer::PerEntryYield,
        ),
    ];

    for (offset, (profile, expected)) in cases.into_iter().enumerate() {
        for (topology_offset, topology) in [ScenarioTopology::Ring, ScenarioTopology::Hotspot]
            .into_iter()
            .enumerate()
        {
            let outcome = run_scenario(
                10,
                topology,
                1_000 + (offset * 2 + topology_offset) as u64,
                profile,
                DeliveryStrategy::Fifo,
            )
            .await;
            assert_eq!(
                outcome.protection_violations,
                BTreeSet::from([expected]),
                "{}",
                outcome.diagnostic(),
            );
            assert_eq!(
                outcome.persisted_entries,
                outcome.expected_entries,
                "single-layer ablation should expose only its named proposition; {}",
                outcome.diagnostic(),
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn test_ring_five_all_enabled_replays_identically_thirty_times() {
    let reference = run_scenario(
        5,
        ScenarioTopology::Ring,
        686,
        ProtectionProfile::ALL_ENABLED,
        DeliveryStrategy::Seeded,
    )
    .await;
    let expected = reference.canonical_replay_json();
    assert_enabled_outcome(&reference);

    for replay in 1..30 {
        let outcome = run_scenario(
            5,
            ScenarioTopology::Ring,
            686,
            ProtectionProfile::ALL_ENABLED,
            DeliveryStrategy::Seeded,
        )
        .await;
        assert_enabled_outcome(&outcome);
        assert_eq!(
            outcome.canonical_replay_json(),
            expected,
            "replay {replay} diverged; {}",
            outcome.diagnostic(),
        );
    }
}

#[tokio::test(start_paused = true)]
async fn test_different_seeds_explore_different_legal_delivery_orders() {
    let first = run_scenario(
        5,
        ScenarioTopology::Ring,
        686,
        ProtectionProfile::ALL_ENABLED,
        DeliveryStrategy::Seeded,
    )
    .await;
    let second = run_scenario(
        5,
        ScenarioTopology::Ring,
        687,
        ProtectionProfile::ALL_ENABLED,
        DeliveryStrategy::Seeded,
    )
    .await;
    assert_enabled_outcome(&first);
    assert_enabled_outcome(&second);
    assert_ne!(delivery_order(&first.state), delivery_order(&second.state));
}

fn delivery_order(state: &SimState) -> Vec<u64> {
    state
        .trace()
        .events()
        .iter()
        .filter_map(|event| match &event.action {
            SimAction::DeliverFrame { transfer_id, .. } => Some(*transfer_id),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn test_hotspot_five_all_enabled_replays_real_sync_protocol() {
    let outcome = run_scenario(
        5,
        ScenarioTopology::Hotspot,
        687,
        ProtectionProfile::ALL_ENABLED,
        DeliveryStrategy::Fifo,
    )
    .await;
    assert_enabled_outcome(&outcome);
}

#[tokio::test(start_paused = true)]
async fn test_lifo_strategy_is_committed_and_converges() {
    let outcome = run_scenario(
        5,
        ScenarioTopology::Ring,
        6_880,
        ProtectionProfile::ALL_ENABLED,
        DeliveryStrategy::Lifo,
    )
    .await;
    assert_enabled_outcome(&outcome);
}

#[tokio::test(start_paused = true)]
async fn test_adversarial_control_last_strategy_is_committed_and_converges() {
    let outcome = run_scenario(
        5,
        ScenarioTopology::Hotspot,
        6_881,
        ProtectionProfile::ALL_ENABLED,
        DeliveryStrategy::AdversarialControlLast,
    )
    .await;
    assert_enabled_outcome(&outcome);
}

#[tokio::test(start_paused = true)]
#[ignore = "explicit SYNC_STORM_* replay entrypoint"]
async fn test_replay_sync_storm_from_env() {
    let count = replay_env("SYNC_STORM_N")
        .parse::<usize>()
        .expect("SYNC_STORM_N must be a positive integer");
    let seed = replay_env("SYNC_STORM_SEED")
        .parse::<u64>()
        .expect("SYNC_STORM_SEED must be a u64");
    let topology = parse_topology(&replay_env("SYNC_STORM_TOPOLOGY"));
    let profile = parse_profile(&replay_env("SYNC_STORM_PROFILE"));
    let strategy = parse_strategy(&replay_env("SYNC_STORM_STRATEGY"));
    let outcome = run_scenario(count, topology, seed, profile, strategy).await;

    if profile == ProtectionProfile::ALL_ENABLED {
        assert_enabled_outcome(&outcome);
    } else {
        assert_eq!(
            outcome.protection_violations,
            profile.disabled_layers(),
            "{}",
            outcome.diagnostic(),
        );
    }
    println!("{}", outcome.diagnostic());
}

fn replay_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for explicit replay"))
}

fn parse_topology(value: &str) -> ScenarioTopology {
    match value {
        "ring" => ScenarioTopology::Ring,
        "hotspot" => ScenarioTopology::Hotspot,
        _ => panic!("unsupported SYNC_STORM_TOPOLOGY {value}"),
    }
}

fn parse_profile(value: &str) -> ProtectionProfile {
    match value {
        "all-enabled" => ProtectionProfile::ALL_ENABLED,
        "no-class-reservations" => ProtectionProfile::without_class_reservations(),
        "no-bounded-control-burst" => ProtectionProfile::without_bounded_control_burst(),
        "no-barrier-control-exemption" => ProtectionProfile::without_barrier_control_exemption(),
        "no-per-entry-yield" => ProtectionProfile::without_per_entry_yield(),
        "legacy-all-disabled" => ProtectionProfile::LEGACY_ALL_DISABLED,
        _ => panic!("unsupported SYNC_STORM_PROFILE {value}"),
    }
}

fn parse_strategy(value: &str) -> DeliveryStrategy {
    match value {
        "fifo" => DeliveryStrategy::Fifo,
        "lifo" => DeliveryStrategy::Lifo,
        "seeded" => DeliveryStrategy::Seeded,
        "adversarial-control-last" => DeliveryStrategy::AdversarialControlLast,
        _ => panic!("unsupported SYNC_STORM_STRATEGY {value}"),
    }
}

#[tokio::test(start_paused = true)]
#[ignore = "explicit PR sync-storm size matrix"]
async fn test_pr_ring_and_hotspot_size_matrix() {
    for count in [10, 25, 50] {
        assert_and_report_enabled(
            run_scenario(
                count,
                ScenarioTopology::Ring,
                700 + count as u64,
                ProtectionProfile::ALL_ENABLED,
                DeliveryStrategy::Fifo,
            )
            .await,
        );
        assert_and_report_enabled(
            run_scenario(
                count,
                ScenarioTopology::Hotspot,
                800 + count as u64,
                ProtectionProfile::ALL_ENABLED,
                DeliveryStrategy::Fifo,
            )
            .await,
        );
    }
}

#[tokio::test(start_paused = true)]
#[ignore = "nightly extended N=50 seed matrix"]
async fn test_nightly_extended_seed_matrix() {
    for seed in nightly_seeds() {
        for topology in [ScenarioTopology::Ring, ScenarioTopology::Hotspot] {
            assert_and_report_enabled(
                run_scenario(
                    50,
                    topology,
                    seed,
                    ProtectionProfile::ALL_ENABLED,
                    DeliveryStrategy::Seeded,
                )
                .await,
            );
        }
    }
}

fn assert_and_report_enabled(outcome: ScenarioOutcome) {
    println!("{}", outcome.diagnostic());
    assert_enabled_outcome(&outcome);
}

fn nightly_seeds() -> [u64; 16] {
    let mut seeds = [0_u64; 16];
    let mut state = 10_686;
    for seed in &mut seeds {
        state = crate::simulation::mix_seed(state);
        *seed = state;
    }
    seeds
}
