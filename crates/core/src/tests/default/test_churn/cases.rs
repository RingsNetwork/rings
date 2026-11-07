//! Scenario matrix and deterministic replay entrypoints for the Weibull churn simulator (#773).

use super::runner::run_churn_scenario;
use super::ChurnScenarioConfig;
use super::ChurnScenarioOutcome;
use super::DEFAULT_CHURN_DURATION_MS;
use super::DEFAULT_STABILIZE_INTERVAL_MS;
use super::LARGE_MATRIX_CHURN_DURATION_MS;
use crate::simulation::model::DEFAULT_WEIBULL_SHAPE;
use crate::simulation::model::MAX_MEDIAN_SESSION_MS;
use crate::simulation::model::MAX_WEIBULL_SHAPE;
use crate::simulation::model::MIN_MEDIAN_SESSION_MS;
use crate::simulation::model::MIN_WEIBULL_SHAPE;

const SMALL_MATRIX_N: usize = 10;
const LARGE_MATRIX_N: usize = 100;
const MEDIAN_SESSIONS_MIN: [u64; 3] = [1, 5, 30];
const GRACEFUL_LEAVE_FRACTIONS: [f64; 2] = [0.0, 0.5];

const fn scenario_seed(target_nodes: usize, median_session_min: u64, graceful_index: usize) -> u64 {
    0x0773_0000_0000_0000
        ^ ((target_nodes as u64) << 32)
        ^ (median_session_min << 16)
        ^ (graceful_index as u64)
}

fn assert_churn_safety_and_liveness(outcome: &ChurnScenarioOutcome) {
    let diag = outcome.diagnostic();
    assert!(
        outcome.violations.is_empty(),
        "churn scenario violated safety invariants {:?}: {diag}",
        outcome.violations
    );
    assert!(
        outcome.metrics.live_node_samples > 0,
        "churn scenario must record live node samples: {diag}"
    );
    assert!(
        outcome.metrics.lookup_attempts > 0,
        "churn scenario must record multi-hop lookup probes: {diag}"
    );
    assert!(
        outcome.metrics.maintenance_bytes > 0
            && outcome.metrics.maintenance_bytes_per_node_sec > 0.0,
        "churn scenario must account for maintenance traffic cost: {diag}"
    );
    if outcome.metrics.held_messages_injected > 0 {
        assert_eq!(
            outcome.metrics.held_messages_delivered, outcome.metrics.held_messages_injected,
            "returning nodes must receive held inbox messages: {diag}"
        );
    }
}

/// CI small scenario matrix (`N = 10`, median session in `{1, 5, 30}` min,
/// graceful-leave fraction in `{0.0, 0.5}`), including deterministic seed replay
/// and pinned baseline verification at the 5-minute median session.
#[tokio::test(start_paused = true)]
async fn test_churn_small_matrix_n10() {
    assert_eq!(MIN_MEDIAN_SESSION_MS, 60_000);
    assert_eq!(MAX_MEDIAN_SESSION_MS, 10_800_000);
    assert!((MIN_WEIBULL_SHAPE..=MAX_WEIBULL_SHAPE).contains(&DEFAULT_WEIBULL_SHAPE));

    for &median_min in &MEDIAN_SESSIONS_MIN {
        for (g_idx, &graceful_fraction) in GRACEFUL_LEAVE_FRACTIONS.iter().enumerate() {
            let config = ChurnScenarioConfig {
                seed: scenario_seed(SMALL_MATRIX_N, median_min, g_idx),
                target_nodes: SMALL_MATRIX_N,
                median_session_ms: median_min * 60_000,
                shape_k: DEFAULT_WEIBULL_SHAPE,
                graceful_leave_fraction: graceful_fraction,
                duration_ms: DEFAULT_CHURN_DURATION_MS,
                stabilize_interval_ms: DEFAULT_STABILIZE_INTERVAL_MS,
            };
            let outcome = run_churn_scenario(config).await;
            println!("SMALL_MATRIX: {}", outcome.summary_line());
            assert_churn_safety_and_liveness(&outcome);

            if median_min == 5 && g_idx == 0 {
                let replay = run_churn_scenario(config).await;
                assert_eq!(
                    outcome
                        .state
                        .trace()
                        .digest()
                        .expect("first trace must serialize"),
                    replay
                        .state
                        .trace()
                        .digest()
                        .expect("replay trace must serialize"),
                    "churn trace digest must be replay-identical for the same seed"
                );
                assert_eq!(outcome.metrics, replay.metrics);
                assert_eq!(
                    (
                        outcome.metrics.orphan_samples,
                        outcome.metrics.live_node_samples
                    ),
                    (2, 108),
                    "5m crash-only baseline orphan samples changed: {}",
                    outcome.diagnostic()
                );
                assert_eq!(
                    (
                        outcome.metrics.lookup_successes,
                        outcome.metrics.lookup_attempts
                    ),
                    (198, 528),
                    "5m crash-only baseline lookup samples changed: {}",
                    outcome.diagnostic()
                );
            } else if median_min == 5 && g_idx == 1 {
                assert_eq!(
                    (
                        outcome.metrics.orphan_samples,
                        outcome.metrics.live_node_samples
                    ),
                    (1, 218),
                    "5m half-graceful baseline orphan samples changed: {}",
                    outcome.diagnostic()
                );
                assert_eq!(
                    (
                        outcome.metrics.lookup_successes,
                        outcome.metrics.lookup_attempts
                    ),
                    (2138, 2182),
                    "5m half-graceful baseline lookup samples changed: {}",
                    outcome.diagnostic()
                );
            }
        }
    }
}

/// Opt-in large scenario matrix: `N = 100`, median session in `{1, 5, 30}` min,
/// graceful-leave fraction in `{0.0, 0.5}`.
#[tokio::test(start_paused = true)]
#[ignore = "large N=100 churn matrix is opt-in"]
async fn test_churn_large_matrix_n100() {
    for &median_min in &MEDIAN_SESSIONS_MIN {
        for (g_idx, &graceful_fraction) in GRACEFUL_LEAVE_FRACTIONS.iter().enumerate() {
            let config = ChurnScenarioConfig {
                seed: scenario_seed(LARGE_MATRIX_N, median_min, g_idx),
                target_nodes: LARGE_MATRIX_N,
                median_session_ms: median_min * 60_000,
                shape_k: DEFAULT_WEIBULL_SHAPE,
                graceful_leave_fraction: graceful_fraction,
                duration_ms: LARGE_MATRIX_CHURN_DURATION_MS,
                stabilize_interval_ms: DEFAULT_STABILIZE_INTERVAL_MS,
            };
            let outcome = run_churn_scenario(config).await;
            println!("LARGE_MATRIX: {}", outcome.summary_line());
            assert_churn_safety_and_liveness(&outcome);
        }
    }
}

/// Replay a single churn scenario from environment variables (`CHURN_SEED`,
/// `CHURN_N`, `CHURN_MEDIAN_MIN`, `CHURN_GRACEFUL_FRACTION`, `CHURN_DURATION_SECS`).
#[tokio::test(start_paused = true)]
#[ignore = "manual replay entry point configured by CHURN_* environment variables"]
async fn test_replay_churn_from_env() {
    let target_nodes = std::env::var("CHURN_N")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(SMALL_MATRIX_N);
    let median_min = std::env::var("CHURN_MEDIAN_MIN")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(5);
    let graceful_fraction = std::env::var("CHURN_GRACEFUL_FRACTION")
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(0.5);
    let default_seed = scenario_seed(
        target_nodes,
        median_min,
        usize::from(graceful_fraction >= 0.25),
    );
    let seed = std::env::var("CHURN_SEED")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(default_seed);
    let duration_ms = std::env::var("CHURN_DURATION_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1_000))
        .unwrap_or(DEFAULT_CHURN_DURATION_MS);

    let config = ChurnScenarioConfig {
        seed,
        target_nodes,
        median_session_ms: median_min.saturating_mul(60_000),
        shape_k: DEFAULT_WEIBULL_SHAPE,
        graceful_leave_fraction: graceful_fraction,
        duration_ms,
        stabilize_interval_ms: DEFAULT_STABILIZE_INTERVAL_MS,
    };

    let first = run_churn_scenario(config).await;
    let second = run_churn_scenario(config).await;
    assert_eq!(
        first
            .state
            .trace()
            .digest()
            .expect("first replay trace must serialize"),
        second
            .state
            .trace()
            .digest()
            .expect("second replay trace must serialize")
    );
    println!("REPLAY_SUMMARY: {}", first.summary_line());
    assert_churn_safety_and_liveness(&first);
}
