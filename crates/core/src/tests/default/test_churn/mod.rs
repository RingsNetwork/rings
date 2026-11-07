//! Deterministic Weibull-session churn simulator and churn-rate invariants (#773).
//!
//! Each scenario drives production `Swarm`, `PeerRing`, and `Stabilizer` phases
//! (`clean_unavailable_connections`, `converge_fingers_for_simulation`,
//! `probe_peer_liveness_for_simulation`, `correct_stabilize`, and
//! `run_requested_storage_maintenance`) over the controlled dummy transport
//! under virtual simulation time.
//!
//! Session lengths are drawn from $\text{Weibull}(k, \lambda)$ with $k = 0.36$
//! ($k \in [0.34, 0.38]$) and $\lambda = t_{\text{med}} / (\ln 2)^{1/k}$, while
//! arrivals follow a stationary Poisson process parameterized by the Bamboo
//! median session time ($1\text{ min} \dots 3\text{ h}$) and target population $N$.
//!
//! # Recorded Baseline Metrics (`N = 10`, `duration = 300s`, `stabilize_interval = 15s`, `k = 0.36`)
//!
//! At the 5-minute median session baseline:
//! - **Crash-only (`graceful_leave_fraction = 0.0`, seed `0x0773_000a_0005_0000`)**:
//!   - Orphan rate: `0.0185` (`2 / 108` live-node samples)
//!   - Lookup success rate: `0.3750` (`198 / 528` multi-hop `find_successor` probes)
//!   - Held inbox delivery rate: `1.0000` (`8 / 8` returning-node inbox deliveries)
//!   - Maintenance cost: `2581.01 B/node/s` (`4,214,027 B` total)
//! - **Half-graceful (`graceful_leave_fraction = 0.5`, seed `0x0773_000a_0005_0001`)**:
//!   - Orphan rate: `0.0046` (`1 / 218` live-node samples)
//!   - Lookup success rate: `0.9798` (`2138 / 2182` multi-hop `find_successor` probes)
//!   - Held inbox delivery rate: `1.0000` (`4 / 4` returning-node inbox deliveries)
//!   - Maintenance cost: `2596.87 B/node/s` (`8,530,666 B` total)
//!
//! Full `N = 10` CI matrix summary:
//!
//! | Median session | Graceful fraction | Seed | Orphan rate | Lookup success rate | Held inbox delivery | Maintenance B/node/s |
//! | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
//! | `1 min` | `0.0` | `0x0773_000a_0001_0000` | `0.0033` (`1/305`) | `0.9972` (`4653/4666`) | `1.0000` (`22/22`) | `4850.18` |
//! | `1 min` | `0.5` | `0x0773_000a_0001_0001` | `0.0000` (`0/269`) | `0.6881` (`2418/3514`) | `1.0000` (`23/23`) | `6522.38` |
//! | `5 min` | `0.0` | `0x0773_000a_0005_0000` | `0.0185` (`2/108`) | `0.3750` (`198/528`) | `1.0000` (`8/8`) | `2581.01` |
//! | `5 min` | `0.5` | `0x0773_000a_0005_0001` | `0.0046` (`1/218`) | `0.9798` (`2138/2182`) | `1.0000` (`4/4`) | `2596.87` |
//! | `30 min` | `0.0` | `0x0773_000a_001e_0000` | `0.0057` (`1/175`) | `0.9676` (`1316/1360`) | `1.0000` (`2/2`) | `1966.40` |
//! | `30 min` | `0.5` | `0x0773_000a_001e_0001` | `0.0056` (`1/179`) | `0.9697` (`1406/1450`) | `1.0000` (`3/3`) | `2110.18` |

mod cases;
mod driver;
mod runner;

use crate::simulation::model::SimInvariantViolation;
use crate::simulation::model::SimLimits;
use crate::simulation::model::SimState;
use crate::simulation::CONTROL_DEADLINE_MS;

const TEST_EPOCH_MS: u128 = 1_700_000_000_000;
const DEFAULT_CHURN_DURATION_MS: u64 = 300_000;
const LARGE_MATRIX_CHURN_DURATION_MS: u64 = 180_000;
const DEFAULT_STABILIZE_INTERVAL_MS: u64 = 15_000;

const CHURN_MODEL_LIMITS: SimLimits = SimLimits {
    node_bytes: 128 * 1024 * 1024,
    global_bytes: 256 * 1024 * 1024,
    control_deadline_ms: CONTROL_DEADLINE_MS,
    repair_amplification: 4,
};

/// Parameters defining one deterministic Weibull churn scenario.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ChurnScenarioConfig {
    /// Deterministic PRNG seed for the scenario.
    pub(super) seed: u64,
    /// Target stationary live population $N$.
    pub(super) target_nodes: usize,
    /// Median session length $t_{\text{med}}$ in milliseconds.
    pub(super) median_session_ms: u64,
    /// Weibull shape parameter $k \in [0.34, 0.38]$.
    pub(super) shape_k: f64,
    /// Fraction of departures that leave gracefully (`0.0..=1.0`).
    pub(super) graceful_leave_fraction: f64,
    /// Total simulated virtual duration in milliseconds.
    pub(super) duration_ms: u64,
    /// Periodic stabilization tick interval in milliseconds.
    pub(super) stabilize_interval_ms: u64,
}

/// Measured liveness, routing, inbox-delivery, and maintenance-cost metrics.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ChurnMetrics {
    /// Number of live-node samples observed with an empty successor list.
    pub(super) orphan_samples: u64,
    /// Total live-node samples observed across stabilization ticks.
    pub(super) live_node_samples: u64,
    /// Fraction of live-node samples with an empty successor list.
    pub(super) orphan_rate: f64,
    /// Number of multi-hop `find_successor` probes reaching the true live Chord successor.
    pub(super) lookup_successes: u64,
    /// Total multi-hop `find_successor` probes executed.
    pub(super) lookup_attempts: u64,
    /// Fraction of multi-hop `find_successor` probes reaching the true live Chord successor.
    pub(super) lookup_success_rate: f64,
    /// Offline inbox messages held for departed nodes.
    pub(super) held_messages_injected: u64,
    /// Held inbox messages delivered to returning nodes upon rejoin and stabilization.
    pub(super) held_messages_delivered: u64,
    /// Fraction of held inbox messages delivered to returning nodes.
    pub(super) held_inbox_delivery_rate: f64,
    /// Total admitted maintenance wire bytes (`Control` + `Storage` + `Reassembly`).
    pub(super) maintenance_bytes: u64,
    /// Maintenance wire bytes per live node per simulated second.
    pub(super) maintenance_bytes_per_node_sec: f64,
    /// Time-averaged live node count across probe ticks.
    pub(super) mean_live_nodes: f64,
    /// Total `Join` actions applied.
    pub(super) joins: u64,
    /// Total graceful `Leave` actions applied.
    pub(super) graceful_leaves: u64,
    /// Total ungraceful `Leave` actions applied.
    pub(super) ungraceful_leaves: u64,
    /// Total `Crash` actions applied.
    pub(super) crashes: u64,
}

/// Complete outcome of one churn scenario run.
pub(super) struct ChurnScenarioOutcome {
    /// Scenario configuration.
    pub(super) config: ChurnScenarioConfig,
    /// Final pure model state and trace.
    pub(super) state: SimState,
    /// Measured liveness and cost metrics.
    pub(super) metrics: ChurnMetrics,
    /// Any safety or capacity invariant violations detected by [`SimState`].
    pub(super) violations: Vec<SimInvariantViolation>,
}

impl ChurnScenarioOutcome {
    pub(super) fn replay_command(&self) -> String {
        format!(
            "CHURN_SEED={} CHURN_N={} CHURN_MEDIAN_MIN={} CHURN_GRACEFUL_FRACTION={} \
             CHURN_DURATION_SECS={} cargo test -p rings-core --features dummy \
             --no-default-features test_replay_churn_from_env -- --ignored --nocapture",
            self.config.seed,
            self.config.target_nodes,
            self.config.median_session_ms / 60_000,
            self.config.graceful_leave_fraction,
            self.config.duration_ms / 1_000,
        )
    }

    pub(super) fn summary_line(&self) -> String {
        format!(
            "n={} median_min={} graceful={:.1} seed=0x{:016x} mean_live={:.2} \
             joins={} leaves_g={} leaves_u={} crashes={} \
             orphan_rate={:.4} ({}/{}) lookup_success_rate={:.4} ({}/{}) \
             held_inbox={}/{} ({:.4}) maint_bytes_per_node_s={:.2} (total_bytes={})",
            self.config.target_nodes,
            self.config.median_session_ms / 60_000,
            self.config.graceful_leave_fraction,
            self.config.seed,
            self.metrics.mean_live_nodes,
            self.metrics.joins,
            self.metrics.graceful_leaves,
            self.metrics.ungraceful_leaves,
            self.metrics.crashes,
            self.metrics.orphan_rate,
            self.metrics.orphan_samples,
            self.metrics.live_node_samples,
            self.metrics.lookup_success_rate,
            self.metrics.lookup_successes,
            self.metrics.lookup_attempts,
            self.metrics.held_messages_delivered,
            self.metrics.held_messages_injected,
            self.metrics.held_inbox_delivery_rate,
            self.metrics.maintenance_bytes_per_node_sec,
            self.metrics.maintenance_bytes,
        )
    }

    pub(super) fn diagnostic(&self) -> String {
        let digest = self
            .state
            .trace()
            .digest()
            .map(hex::encode)
            .unwrap_or_else(|err| format!("trace-digest-error:{err}"));
        format!(
            "{} snapshot={:?} trace_digest={digest} replay=`{}`",
            self.summary_line(),
            self.state.snapshot(),
            self.replay_command(),
        )
    }
}
