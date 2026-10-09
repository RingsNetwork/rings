use std::pin::pin;

use futures::future::select;
use futures::future::Either;

use super::*;

#[tokio::test(start_paused = true)]
async fn test_legacy_all_disabled_breaks_the_feedback_loop_at_the_barrier() {
    let state = legacy_storm_state().await;
    let violations = state.invariant_violations(MODEL_LIMITS);
    // The loop's first step no longer closes: per-lane credit dispatch bounds the barrier's
    // wait to one hand-over, so the probe is not starved and nothing downstream follows.
    assert!(!violations
        .iter()
        .any(|violation| matches!(violation, SimInvariantViolation::ControlStarvation { .. })));
    assert!(!violations
        .iter()
        .any(|violation| matches!(violation, SimInvariantViolation::FalseDisconnect { .. })));
    assert!(!violations
        .iter()
        .any(|violation| matches!(violation, SimInvariantViolation::RepairStorm { .. })));
    // Credit does not subsume the per-entry yield: a storage batch still runs unyielding.
    assert!(violations
        .iter()
        .any(|violation| matches!(violation, SimInvariantViolation::NoStorageProgress)));
}

#[tokio::test(start_paused = true)]
async fn test_n10_single_ablation_matrix_exposes_each_unsubsumed_proposition() {
    // Each single-layer ablation exposes exactly its proposition, except the barrier control
    // exemption: per-lane credit dispatch bounds the barrier's wait to one hand-over, so
    // without the exemption control is delayed but never starved, and nothing is violated
    // (#927).
    let cases = [
        (
            ProtectionProfile::without_class_reservations(),
            BTreeSet::from([ProtectionLayer::ClassReservations]),
        ),
        (
            ProtectionProfile::without_bounded_control_burst(),
            BTreeSet::from([ProtectionLayer::BoundedControlBurst]),
        ),
        (
            ProtectionProfile::without_barrier_control_exemption(),
            BTreeSet::new(),
        ),
        (
            ProtectionProfile::without_per_entry_yield(),
            BTreeSet::from([ProtectionLayer::PerEntryYield]),
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
                expected,
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

/// Wait until the controlled queue gains an event or a send of `sends` completes, returning the
/// completed send. The waiter is enabled before the sends are polled, so an event they queue
/// while polled here is not missed.
async fn next_enqueue_or_send<S>(sends: &mut S) -> Option<S::Item>
where S: futures::Stream + Unpin {
    let signal = dummy_controlled::enqueue_signal();
    let mut enqueued = pin!(signal.notified());
    enqueued.as_mut().enable();
    match select(enqueued, sends.next()).await {
        Either::Left(((), _)) => None,
        Either::Right((sent, _)) => sent,
    }
}

/// Boundary witness of the credit law (#924): one sender pushes more than a window of
/// application messages to one receiver, which consumes only what the driver delivers.
///
/// The workload is the origin quota's message burst, two windows: as many messages as the
/// receiver admits from one origin at once, so every message is the flow control's to carry and
/// none is the rate limit's to refuse.
///
/// At every step the link carries at most one window of the application lane's frames, and
/// exactly one window before the first delivery, so the lane saturates; the rest of the
/// messages wait at their sender, every message is eventually delivered, and no frame is
/// refused.
#[tokio::test(start_paused = true)]
async fn test_saturated_lane_holds_one_window_and_conserves_every_frame() {
    let runtime = SimulationRuntimeGuard::enter(950, TEST_EPOCH_MS, ProtectionProfile::ALL_ENABLED)
        .expect("credit boundary runtime must install");
    let nodes = build_repair_nodes(2);
    establish_topology(&runtime, &nodes, ScenarioTopology::Ring).await;
    let window = usize::try_from(rings_transport::core::credit::LANE_CREDIT_WINDOW)
        .expect("the credit window must fit usize");
    let messages = usize::try_from(crate::message::DEFAULT_ORIGIN_QUOTA_MESSAGE_BURST)
        .expect("the origin quota burst must fit usize");
    assert!(
        messages > window,
        "the workload must exceed one window to saturate the lane"
    );
    let peer = nodes[1].did();
    let mut sends = (0..messages)
        .map(|index| {
            let swarm = nodes[0].swarm.clone();
            async move {
                let message = Message::custom(format!("saturate-{index}").as_bytes())?;
                swarm.send_direct_message(message, peer).await
            }
            .boxed_local()
        })
        .collect::<FuturesUnordered<_>>();
    let application_in_flight = || {
        runtime
            .pending_deliveries()
            .expect("saturated frames must classify")
            .into_iter()
            .filter(|delivery| delivery.class == ScheduledDeliveryClass::Application)
            .collect::<Vec<_>>()
    };
    // A hang guard, not a pace: a lane that never saturates, or a send that never completes,
    // fails here instead of hanging.
    tokio::time::timeout(Duration::from_secs(60), async {
        // Before the first delivery, woken by each event the sends queue: the lane saturates
        // at one window.
        loop {
            while let std::task::Poll::Ready(Some(sent)) = futures::poll!(sends.next()) {
                sent.expect("a send within the window must complete");
            }
            let in_flight = application_in_flight().len();
            assert!(
                in_flight <= window,
                "flow control must bound the application lane's frames in flight by one window"
            );
            if in_flight == window {
                break;
            }
            if let Some(sent) = next_enqueue_or_send(&mut sends).await {
                sent.expect("a send within the window must complete");
            }
        }
        // Then one frame at a time, woken by each event the sends queue as credit returns.
        loop {
            while let std::task::Poll::Ready(Some(sent)) = futures::poll!(sends.next()) {
                sent.expect("a send waiting for credit must complete once credit returns");
            }
            let in_flight = application_in_flight();
            assert!(
                in_flight.len() <= window,
                "flow control must bound the application lane's frames in flight by one window"
            );
            match in_flight.first() {
                Some(delivery) => assert!(runtime
                    .deliver(delivery)
                    .await
                    .expect("saturated delivery must remain stable")),
                None if sends.is_empty() => break,
                None => {
                    if let Some(sent) = next_enqueue_or_send(&mut sends).await {
                        sent.expect("a send waiting for credit must complete once credit returns");
                    }
                }
            }
        }
    })
    .await
    .expect("every send must complete and every frame be delivered");
    let mut received = 0;
    while let Some(payload) = nodes[1].try_listen_once().await {
        received += usize::from(matches!(
            payload.transaction.data::<Message>(),
            Ok(Message::CustomMessage(_))
        ));
    }
    assert_eq!(received, messages, "every message must be delivered");
    assert_eq!(
        runtime
            .capacity_observations()
            .expect("capacity observations must remain available")
            .rejected_frames(),
        0,
        "no frame may be refused under flow control"
    );
}
