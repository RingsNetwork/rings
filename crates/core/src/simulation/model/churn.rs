//! Weibull session-length and Poisson arrival generator for churn simulations (#773).

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use super::SimAction;
use super::SimNodeId;
use crate::simulation::mix_seed;

/// Minimum Bamboo median session time supported by the generator (1 minute).
pub(crate) const MIN_MEDIAN_SESSION_MS: u64 = 60_000;

/// Maximum Bamboo median session time supported by the generator (3 hours).
pub(crate) const MAX_MEDIAN_SESSION_MS: u64 = 10_800_000;

/// Lower bound of the empirical Stutzbach/Bamboo Weibull shape range.
pub(crate) const MIN_WEIBULL_SHAPE: f64 = 0.34;

/// Upper bound of the empirical Stutzbach/Bamboo Weibull shape range.
pub(crate) const MAX_WEIBULL_SHAPE: f64 = 0.38;

/// Default Weibull shape parameter $k$ within $[0.34, 0.38]$.
pub(crate) const DEFAULT_WEIBULL_SHAPE: f64 = 0.36;

const DEFAULT_MIN_SESSION_MS: u64 = 1_000;
const DEFAULT_MAX_SESSION_MS: u64 = MAX_MEDIAN_SESSION_MS * 4;
const DEFAULT_REJOIN_COOLDOWN_MS: u64 = 15_000;
const SURVIVAL_INTEGRAL_STEPS: usize = 64;

/// Configuration validation errors for [`WeibullChurnConfig`].
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
pub(crate) enum WeibullChurnConfigError {
    /// Target active population $N$ must be at least 1.
    #[error("target_nodes must be at least 1")]
    ZeroTargetPopulation,
    /// Median session length was outside the 1 min ..= 3 h Bamboo range.
    #[error(
        "median_session_ms {median_session_ms} is outside [{MIN_MEDIAN_SESSION_MS}, {MAX_MEDIAN_SESSION_MS}]"
    )]
    MedianSessionOutOfRange {
        /// Supplied median session length in milliseconds.
        median_session_ms: u64,
    },
    /// Weibull shape parameter $k$ was outside $[0.34, 0.38]$.
    #[error("shape_k {shape_k} is outside [{MIN_WEIBULL_SHAPE}, {MAX_WEIBULL_SHAPE}]")]
    InvalidShape {
        /// Supplied Weibull shape parameter.
        shape_k: f64,
    },
    /// Graceful-leave fraction was outside $[0.0, 1.0]$.
    #[error("graceful_leave_fraction {fraction} is outside [0.0, 1.0]")]
    InvalidGracefulLeaveFraction {
        /// Supplied graceful-leave probability.
        fraction: f64,
    },
}

/// Parameters for a stationary Weibull-lifetime / Poisson-arrival churn workload.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct WeibullChurnConfig {
    /// Target stationary live population $N$.
    pub(crate) target_nodes: usize,
    /// Median session lifetime $t_{\text{med}}$ in milliseconds ($1\text{ min} \dots 3\text{ h}$).
    pub(crate) median_session_ms: u64,
    /// Weibull shape parameter $k \in [0.34, 0.38]$.
    pub(crate) shape_k: f64,
    /// Probability that a departing node leaves gracefully rather than crashing.
    pub(crate) graceful_leave_fraction: f64,
    /// Lower clamp on sampled session lifetimes in milliseconds.
    pub(crate) min_session_ms: u64,
    /// Upper clamp on sampled session lifetimes in milliseconds.
    pub(crate) max_session_ms: u64,
    /// Minimum offline time before a departed node is eligible to rejoin.
    pub(crate) rejoin_cooldown_ms: u64,
}

impl WeibullChurnConfig {
    /// Construct a validated churn configuration with [`DEFAULT_WEIBULL_SHAPE`].
    pub(crate) fn new(
        target_nodes: usize,
        median_session_ms: u64,
        graceful_leave_fraction: f64,
    ) -> Result<Self, WeibullChurnConfigError> {
        if target_nodes == 0 {
            return Err(WeibullChurnConfigError::ZeroTargetPopulation);
        }
        if !(MIN_MEDIAN_SESSION_MS..=MAX_MEDIAN_SESSION_MS).contains(&median_session_ms) {
            return Err(WeibullChurnConfigError::MedianSessionOutOfRange { median_session_ms });
        }
        if !(0.0..=1.0).contains(&graceful_leave_fraction) || graceful_leave_fraction.is_nan() {
            return Err(WeibullChurnConfigError::InvalidGracefulLeaveFraction {
                fraction: graceful_leave_fraction,
            });
        }
        Ok(Self {
            target_nodes,
            median_session_ms,
            shape_k: DEFAULT_WEIBULL_SHAPE,
            graceful_leave_fraction,
            min_session_ms: DEFAULT_MIN_SESSION_MS,
            max_session_ms: DEFAULT_MAX_SESSION_MS,
            rejoin_cooldown_ms: DEFAULT_REJOIN_COOLDOWN_MS,
        })
    }

    /// Override the Weibull shape parameter $k \in [0.34, 0.38]$.
    pub(crate) fn with_shape(mut self, shape_k: f64) -> Result<Self, WeibullChurnConfigError> {
        if !(MIN_WEIBULL_SHAPE..=MAX_WEIBULL_SHAPE).contains(&shape_k) || shape_k.is_nan() {
            return Err(WeibullChurnConfigError::InvalidShape { shape_k });
        }
        self.shape_k = shape_k;
        Ok(self)
    }

    /// Override the minimum offline duration before a departed node rejoins.
    pub(crate) const fn with_rejoin_cooldown_ms(mut self, rejoin_cooldown_ms: u64) -> Self {
        self.rejoin_cooldown_ms = rejoin_cooldown_ms;
        self
    }

    /// Weibull scale parameter $\lambda = t_{\text{med}} / (\ln 2)^{1/k}$ in milliseconds.
    pub(crate) fn scale_lambda_ms(&self) -> f64 {
        let median = self.median_session_ms as f64;
        let divisor = std::f64::consts::LN_2.powf(1.0 / self.shape_k);
        median / divisor
    }

    /// Midpoint-rule approximation of $\int_0^1 2^{-x^k}\,dx$.
    pub(crate) fn median_window_survival_integral(&self) -> f64 {
        let steps = SURVIVAL_INTEGRAL_STEPS as f64;
        let mut sum = 0.0_f64;
        for index in 0..SURVIVAL_INTEGRAL_STEPS {
            let midpoint = (index as f64 + 0.5) / steps;
            let exponent = -midpoint.powf(self.shape_k) * std::f64::consts::LN_2;
            sum += exponent.exp();
        }
        sum / steps
    }

    /// Stationary Poisson arrival rate $\Lambda$ (arrivals per millisecond) keeping $E[N(t)] \approx N$.
    pub(crate) fn poisson_arrival_rate_per_ms(&self) -> f64 {
        let n = self.target_nodes as f64;
        let median = self.median_session_ms as f64;
        let survival_integral = self.median_window_survival_integral();
        (0.5 * n) / (median * survival_integral)
    }
}

/// One timestamped churn action produced by [`WeibullSessionGenerator`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ScheduledChurnEvent {
    /// Virtual monotonic timestamp in milliseconds at which the action occurs.
    pub(crate) virtual_ms: u64,
    /// Pure model action (`Join`, `Leave`, or `Crash`).
    pub(crate) action: SimAction,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PendingEventKind {
    Departure {
        node: SimNodeId,
        graceful: bool,
        use_crash_action: bool,
    },
    Arrival,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PendingEvent {
    virtual_ms: u64,
    sequence: u64,
    kind: PendingEventKind,
}

/// Deterministic Weibull-session and Poisson-arrival generator driven by [`mix_seed`].
#[derive(Clone, Debug)]
pub(crate) struct WeibullSessionGenerator {
    rng_state: u64,
    config: WeibullChurnConfig,
}

impl WeibullSessionGenerator {
    /// Construct a deterministic generator from an explicit seed and configuration.
    pub(crate) const fn new(seed: u64, config: WeibullChurnConfig) -> Self {
        Self {
            rng_state: seed ^ 0x5745_4942_554c_4c31,
            config,
        }
    }

    /// Underlying churn configuration.
    pub(crate) const fn config(&self) -> &WeibullChurnConfig {
        &self.config
    }

    /// Draw a deterministic uniform number in the open interval $(0, 1)$.
    pub(crate) fn sample_uniform_open(&mut self) -> f64 {
        self.rng_state = mix_seed(self.rng_state);
        let bits = (self.rng_state >> 11) | 1;
        let scale = (1_u64 << 53) as f64;
        (bits as f64) / scale
    }

    /// Draw one session lifetime from $\text{Weibull}(k, \lambda)$ in milliseconds.
    pub(crate) fn sample_weibull_lifetime_ms(&mut self) -> u64 {
        let u = self.sample_uniform_open();
        let neg_ln = -(1.0 - u).ln();
        let raw_ms = self.config.scale_lambda_ms() * neg_ln.powf(1.0 / self.config.shape_k);
        if !raw_ms.is_finite() || raw_ms <= self.config.min_session_ms as f64 {
            return self.config.min_session_ms;
        }
        if raw_ms >= self.config.max_session_ms as f64 {
            return self.config.max_session_ms;
        }
        raw_ms.round() as u64
    }

    /// Draw one exponential inter-arrival interval in milliseconds from the stationary Poisson process.
    pub(crate) fn sample_poisson_interarrival_ms(&mut self) -> u64 {
        let u = self.sample_uniform_open();
        let neg_ln = -(1.0 - u).ln();
        let rate = self.config.poisson_arrival_rate_per_ms();
        let raw_ms = neg_ln / rate;
        if !raw_ms.is_finite() || raw_ms <= 1.0 {
            return 1;
        }
        if raw_ms >= self.config.max_session_ms as f64 {
            return self.config.max_session_ms;
        }
        raw_ms.round() as u64
    }

    /// Draw whether a departing node leaves gracefully according to `graceful_leave_fraction`.
    pub(crate) fn sample_is_graceful_leave(&mut self) -> bool {
        if self.config.graceful_leave_fraction <= 0.0 {
            return false;
        }
        if self.config.graceful_leave_fraction >= 1.0 {
            return true;
        }
        self.sample_uniform_open() < self.config.graceful_leave_fraction
    }

    /// Generate a complete, replay-deterministic churn schedule over `0..=duration_ms`.
    pub(crate) fn generate_schedule(&mut self, duration_ms: u64) -> Vec<ScheduledChurnEvent> {
        let target = self.config.target_nodes;
        let min_live = target.min(2);
        let max_pool_nodes = target.saturating_mul(2).max(target.saturating_add(4));
        let mut schedule = Vec::new();
        let mut pending = BTreeSet::new();
        let mut live_nodes = BTreeSet::new();
        let mut departed_since: BTreeMap<SimNodeId, u64> = BTreeMap::new();
        let mut next_sequence = 0_u64;
        let mut ungraceful_counter = 0_u64;

        for index in 0..target {
            let node = SimNodeId(index as u16);
            let via = if index == 0 {
                node
            } else {
                self.pick_live_peer(&live_nodes).unwrap_or(SimNodeId(0))
            };
            live_nodes.insert(node);
            schedule.push(ScheduledChurnEvent {
                virtual_ms: 0,
                action: SimAction::Join { node, via },
            });
            let lifetime_ms = self.sample_weibull_lifetime_ms();
            let graceful = self.sample_is_graceful_leave();
            let use_crash_action = if graceful {
                false
            } else {
                let crash = ungraceful_counter.is_multiple_of(2);
                ungraceful_counter = ungraceful_counter.saturating_add(1);
                crash
            };
            if lifetime_ms <= duration_ms {
                pending.insert(PendingEvent {
                    virtual_ms: lifetime_ms,
                    sequence: next_sequence,
                    kind: PendingEventKind::Departure {
                        node,
                        graceful,
                        use_crash_action,
                    },
                });
                next_sequence = next_sequence.saturating_add(1);
            }
        }

        let mut arrival_ms = 0_u64;
        loop {
            let delta_ms = self.sample_poisson_interarrival_ms();
            arrival_ms = arrival_ms.saturating_add(delta_ms);
            if arrival_ms > duration_ms {
                break;
            }
            pending.insert(PendingEvent {
                virtual_ms: arrival_ms,
                sequence: next_sequence,
                kind: PendingEventKind::Arrival,
            });
            next_sequence = next_sequence.saturating_add(1);
        }

        let mut next_node_id = target;
        while let Some(event) = pending.pop_first() {
            match event.kind {
                PendingEventKind::Departure {
                    node,
                    graceful,
                    use_crash_action,
                } => {
                    if !live_nodes.contains(&node) {
                        continue;
                    }
                    if live_nodes.len() <= min_live {
                        let extra_ms = self.sample_weibull_lifetime_ms();
                        let next_ms = event.virtual_ms.saturating_add(extra_ms);
                        if next_ms <= duration_ms {
                            pending.insert(PendingEvent {
                                virtual_ms: next_ms,
                                sequence: next_sequence,
                                kind: PendingEventKind::Departure {
                                    node,
                                    graceful,
                                    use_crash_action,
                                },
                            });
                            next_sequence = next_sequence.saturating_add(1);
                        }
                        continue;
                    }
                    live_nodes.remove(&node);
                    departed_since.insert(node, event.virtual_ms);
                    let action = if graceful {
                        SimAction::Leave {
                            node,
                            graceful: true,
                        }
                    } else if use_crash_action {
                        SimAction::Crash { node }
                    } else {
                        SimAction::Leave {
                            node,
                            graceful: false,
                        }
                    };
                    schedule.push(ScheduledChurnEvent {
                        virtual_ms: event.virtual_ms,
                        action,
                    });
                }
                PendingEventKind::Arrival => {
                    if live_nodes.len() >= max_pool_nodes {
                        continue;
                    }
                    let Some(via) = self.pick_live_peer(&live_nodes) else {
                        continue;
                    };
                    let candidate = self.select_joining_node(
                        event.virtual_ms,
                        &mut departed_since,
                        &mut next_node_id,
                        max_pool_nodes,
                    );
                    let Some(node) = candidate else {
                        continue;
                    };
                    live_nodes.insert(node);
                    schedule.push(ScheduledChurnEvent {
                        virtual_ms: event.virtual_ms,
                        action: SimAction::Join { node, via },
                    });
                    let lifetime_ms = self.sample_weibull_lifetime_ms();
                    let depart_ms = event.virtual_ms.saturating_add(lifetime_ms);
                    if depart_ms <= duration_ms {
                        let graceful = self.sample_is_graceful_leave();
                        let use_crash_action = if graceful {
                            false
                        } else {
                            let crash = ungraceful_counter.is_multiple_of(2);
                            ungraceful_counter = ungraceful_counter.saturating_add(1);
                            crash
                        };
                        pending.insert(PendingEvent {
                            virtual_ms: depart_ms,
                            sequence: next_sequence,
                            kind: PendingEventKind::Departure {
                                node,
                                graceful,
                                use_crash_action,
                            },
                        });
                        next_sequence = next_sequence.saturating_add(1);
                    }
                }
            }
        }

        schedule
    }

    fn pick_live_peer(&mut self, live_nodes: &BTreeSet<SimNodeId>) -> Option<SimNodeId> {
        if live_nodes.is_empty() {
            return None;
        }
        self.rng_state = mix_seed(self.rng_state);
        let index = (self.rng_state as usize) % live_nodes.len();
        live_nodes.iter().nth(index).copied()
    }

    fn select_joining_node(
        &mut self,
        now_ms: u64,
        departed_since: &mut BTreeMap<SimNodeId, u64>,
        next_node_id: &mut usize,
        max_pool_nodes: usize,
    ) -> Option<SimNodeId> {
        let ready: Vec<SimNodeId> = departed_since
            .iter()
            .filter_map(|(&node, &departed_ms)| {
                (now_ms.saturating_sub(departed_ms) >= self.config.rejoin_cooldown_ms)
                    .then_some(node)
            })
            .collect();
        if !ready.is_empty() {
            self.rng_state = mix_seed(self.rng_state);
            let index = (self.rng_state as usize) % ready.len();
            if let Some(&node) = ready.get(index) {
                departed_since.remove(&node);
                return Some(node);
            }
        }
        if *next_node_id < max_pool_nodes {
            let node = SimNodeId(*next_node_id as u16);
            *next_node_id = next_node_id.saturating_add(1);
            return Some(node);
        }
        let oldest = departed_since
            .iter()
            .min_by_key(|(_, &departed_ms)| departed_ms)
            .map(|(&node, _)| node);
        if let Some(node) = oldest {
            departed_since.remove(&node);
        }
        oldest
    }
}
