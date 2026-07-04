use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::str::FromStr;
use std::time::Duration;
use std::time::Instant;

use num_bigint::BigUint;
use rings_core::dht::Did;
use serde::Serialize;

const RING_BITS: usize = 160;
const DEFAULT_NODE_COUNT: usize = 1600;
const DEFAULT_LOOKUPS_PER_NODE: usize = 64;
const DEFAULT_FINGER_TABLE_SIZES: &[usize] = &[160];
const DEFAULT_FAILED_NODE_PCTS: &[usize] = &[0, 10, 20];
const DEFAULT_CHURN_RATE_PCTS: &[usize] = &[5, 10, 15, 20, 25, 30, 35, 40];
const DEFAULT_LOOKUP_MODE: &str = "random";
const DHT_SUCCESSOR_CAPACITY: usize = 3;
const LOOKUPS_PER_10K: usize = 10_000;
const MAX_LOOKUP_HOPS: usize = 64;

#[derive(Debug, thiserror::Error)]
enum BenchError {
    #[error("environment variable {name} has invalid usize value {value:?}: {source}")]
    EnvUsize {
        name: &'static str,
        value: String,
        source: std::num::ParseIntError,
    },
    #[error("environment variable {name} must contain at least one usize")]
    EmptyList { name: &'static str },
    #[error("unsupported lookup mode {0:?}; expected random or finger_offsets")]
    UnsupportedLookupMode(String),
    #[error("node count must be greater than one")]
    TooFewNodes,
    #[error("active node set must not be empty")]
    NoActiveNodes,
    #[error("missing routing state for node index {0}")]
    MissingNode(usize),
    #[error("failed to build deterministic DID {value}: {source}")]
    DidParse {
        value: String,
        source: rings_core::error::Error,
    },
    #[error("failed to encode benchmark report: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Copy)]
enum LookupMode {
    Random,
    FingerOffsets,
}

impl LookupMode {
    fn name(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::FingerOffsets => "finger_offsets",
        }
    }
}

#[derive(Clone)]
struct Identity {
    ordinal: usize,
    did: Did,
    value: BigUint,
}

#[derive(Clone, PartialEq, Eq)]
struct SimNode {
    successors: Vec<usize>,
    predecessor: Option<usize>,
    fingers: Vec<Option<usize>>,
}

struct SimNetwork {
    identities: Vec<Identity>,
    nodes: Vec<Option<SimNode>>,
    active: BTreeSet<usize>,
    ring_size: BigUint,
}

#[derive(Default, Serialize)]
struct LookupMetrics {
    total: usize,
    resolved: usize,
    correct: usize,
    failed: usize,
    success_rate: f64,
    correctness_rate: f64,
    avg_hops: f64,
    max_hops: usize,
    timeouts: usize,
    mean_lookup_timeouts: f64,
    lookup_failures_per_10k: f64,
    hop_buckets: Vec<HopBucket>,
}

#[derive(Serialize)]
struct HopBucket {
    hops: usize,
    count: usize,
}

#[derive(Serialize)]
struct FingerMetrics {
    known_slots_min: usize,
    known_slots_avg: f64,
    known_slots_max: usize,
}

#[derive(Serialize)]
struct ScenarioReport {
    report: &'static str,
    backend: &'static str,
    scenario: &'static str,
    node_count: usize,
    total_nodes: usize,
    active_nodes: usize,
    finger_table_size: usize,
    successor_capacity: usize,
    lookup_mode: String,
    lookups_per_node: usize,
    build_elapsed_ms: f64,
    lookup_elapsed_ms: f64,
    topology_matches: usize,
    full_matches: usize,
    failed_node_pct: Option<usize>,
    failed_nodes: Option<usize>,
    join_leave_rate_pct: Option<usize>,
    departed_nodes: Option<usize>,
    joined_nodes: Option<usize>,
    fingers: FingerMetrics,
    lookups: LookupMetrics,
}

enum ScenarioSpec {
    Stable,
    FailedNodes { failed_node_pct: usize },
    Churn { join_leave_rate_pct: usize },
}

impl ScenarioSpec {
    fn name(&self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::FailedNodes { .. } => "failed_nodes",
            Self::Churn { .. } => "churn",
        }
    }
}

struct ScenarioInput {
    total_nodes: usize,
    active_indices: BTreeSet<usize>,
    state_known_sets: BTreeMap<usize, Vec<usize>>,
    failed_nodes: Option<usize>,
    departed_nodes: Option<usize>,
    joined_nodes: Option<usize>,
}

struct LookupTarget {
    value: BigUint,
}

enum LookupOutcome {
    Resolved {
        index: usize,
        hops: usize,
        timeouts: usize,
    },
    Failed {
        timeouts: usize,
    },
}

#[derive(Default)]
struct LookupAccumulator {
    total: usize,
    resolved: usize,
    correct: usize,
    failed: usize,
    total_hops: usize,
    max_hops: usize,
    timeouts: usize,
    hop_buckets: BTreeMap<usize, usize>,
}

fn main() -> Result<(), BenchError> {
    let node_count = env_usize("RINGS_DHT_BENCH_NODES", DEFAULT_NODE_COUNT)?;
    if node_count <= 1 {
        return Err(BenchError::TooFewNodes);
    }
    let finger_table_sizes = finger_table_sizes()?;
    let lookups_per_node = env_usize("RINGS_DHT_BENCH_LOOKUPS_PER_NODE", DEFAULT_LOOKUPS_PER_NODE)?;
    let lookup_mode = lookup_mode()?;
    let failed_node_pcts =
        percent_list("RINGS_DHT_BENCH_FAILED_NODE_PCTS", DEFAULT_FAILED_NODE_PCTS)?;
    let churn_rate_pcts = percent_list("RINGS_DHT_BENCH_CHURN_RATE_PCTS", DEFAULT_CHURN_RATE_PCTS)?;

    for finger_table_size in finger_table_sizes {
        let stable = run_scenario(
            ScenarioSpec::Stable,
            node_count,
            finger_table_size,
            lookups_per_node,
            lookup_mode,
        )?;
        println!("{}", serde_json::to_string(&stable)?);

        for failed_node_pct in &failed_node_pcts {
            if *failed_node_pct == 0 {
                continue;
            }
            let report = run_scenario(
                ScenarioSpec::FailedNodes {
                    failed_node_pct: *failed_node_pct,
                },
                node_count,
                finger_table_size,
                lookups_per_node,
                lookup_mode,
            )?;
            println!("{}", serde_json::to_string(&report)?);
        }

        for join_leave_rate_pct in &churn_rate_pcts {
            if *join_leave_rate_pct == 0 {
                continue;
            }
            let report = run_scenario(
                ScenarioSpec::Churn {
                    join_leave_rate_pct: *join_leave_rate_pct,
                },
                node_count,
                finger_table_size,
                lookups_per_node,
                lookup_mode,
            )?;
            println!("{}", serde_json::to_string(&report)?);
        }
    }

    Ok(())
}

fn run_scenario(
    scenario: ScenarioSpec,
    node_count: usize,
    finger_table_size: usize,
    lookups_per_node: usize,
    lookup_mode: LookupMode,
) -> Result<ScenarioReport, BenchError> {
    let build_started = Instant::now();
    let input = scenario_input(&scenario, node_count)?;
    let identities = generate_identities(input.total_nodes)?;
    let network = build_network(
        identities,
        input.active_indices,
        input.state_known_sets,
        finger_table_size,
    )?;
    let build_elapsed = build_started.elapsed();

    let lookup_started = Instant::now();
    let lookups = lookup_metrics(&network, lookups_per_node, lookup_mode, finger_table_size)?;
    let lookup_elapsed = lookup_started.elapsed();
    let expected = expected_active_network(&network, finger_table_size)?;
    let (topology_matches, full_matches) = topology_match_counts(&network, &expected)?;

    Ok(ScenarioReport {
        report: "dummy_dht_sim",
        backend: "peer_ring_topology",
        scenario: scenario.name(),
        node_count,
        total_nodes: input.total_nodes,
        active_nodes: network.active.len(),
        finger_table_size,
        successor_capacity: DHT_SUCCESSOR_CAPACITY,
        lookup_mode: lookup_mode.name().to_string(),
        lookups_per_node,
        build_elapsed_ms: millis(build_elapsed),
        lookup_elapsed_ms: millis(lookup_elapsed),
        topology_matches,
        full_matches,
        failed_node_pct: match scenario {
            ScenarioSpec::FailedNodes { failed_node_pct } => Some(failed_node_pct),
            _ => None,
        },
        failed_nodes: input.failed_nodes,
        join_leave_rate_pct: match scenario {
            ScenarioSpec::Churn {
                join_leave_rate_pct,
            } => Some(join_leave_rate_pct),
            _ => None,
        },
        departed_nodes: input.departed_nodes,
        joined_nodes: input.joined_nodes,
        fingers: finger_metrics(&network)?,
        lookups,
    })
}

fn scenario_input(scenario: &ScenarioSpec, node_count: usize) -> Result<ScenarioInput, BenchError> {
    let total_nodes = match scenario {
        ScenarioSpec::Churn {
            join_leave_rate_pct,
        } => node_count.saturating_add(selected_count(node_count, *join_leave_rate_pct)),
        _ => node_count,
    };
    let identities = generate_identities(total_nodes)?;
    let index_by_ordinal = index_by_ordinal(&identities, total_nodes)?;
    let initial_indices = ordinals_to_indices(&index_by_ordinal, 0..node_count);
    let mut active_indices = initial_indices.iter().copied().collect::<BTreeSet<_>>();
    let mut state_known_sets = BTreeMap::new();
    let mut failed_nodes = None;
    let mut departed_nodes = None;
    let mut joined_nodes = None;

    match scenario {
        ScenarioSpec::Stable => {
            for index in &active_indices {
                state_known_sets.insert(*index, initial_indices.clone());
            }
        }
        ScenarioSpec::FailedNodes { failed_node_pct } => {
            let failed_ordinals = selected_positions(node_count, *failed_node_pct, 3);
            let failed_indices = failed_ordinals
                .iter()
                .filter_map(|ordinal| index_by_ordinal.get(*ordinal).copied())
                .collect::<BTreeSet<_>>();
            failed_nodes = Some(failed_indices.len());
            active_indices.retain(|index| !failed_indices.contains(index));
            for index in &initial_indices {
                state_known_sets.insert(*index, initial_indices.clone());
            }
        }
        ScenarioSpec::Churn {
            join_leave_rate_pct,
        } => {
            let churn_count = selected_count(node_count, *join_leave_rate_pct);
            let departed_ordinals = selected_positions(node_count, *join_leave_rate_pct, 11);
            let departed_indices = departed_ordinals
                .iter()
                .filter_map(|ordinal| index_by_ordinal.get(*ordinal).copied())
                .collect::<BTreeSet<_>>();
            let joiner_indices = ordinals_to_indices(
                &index_by_ordinal,
                node_count..node_count.saturating_add(churn_count),
            );

            departed_nodes = Some(departed_indices.len());
            joined_nodes = Some(joiner_indices.len());
            active_indices.retain(|index| !departed_indices.contains(index));
            active_indices.extend(joiner_indices.iter().copied());

            let active_known = active_indices.iter().copied().collect::<Vec<_>>();
            for index in initial_indices
                .iter()
                .filter(|index| !departed_indices.contains(index))
            {
                state_known_sets.insert(*index, initial_indices.clone());
            }
            for index in joiner_indices {
                state_known_sets.insert(index, active_known.clone());
            }
        }
    }

    if active_indices.is_empty() {
        return Err(BenchError::NoActiveNodes);
    }

    Ok(ScenarioInput {
        total_nodes,
        active_indices,
        state_known_sets,
        failed_nodes,
        departed_nodes,
        joined_nodes,
    })
}

fn build_network(
    identities: Vec<Identity>,
    active: BTreeSet<usize>,
    state_known_sets: BTreeMap<usize, Vec<usize>>,
    finger_table_size: usize,
) -> Result<SimNetwork, BenchError> {
    let ring_size = ring_size();
    let mut nodes = vec![None; identities.len()];
    for (local_index, known) in state_known_sets {
        let node = build_node(
            local_index,
            &known,
            &identities,
            &ring_size,
            finger_table_size,
        );
        if let Some(slot) = nodes.get_mut(local_index) {
            *slot = Some(node);
        }
    }
    Ok(SimNetwork {
        identities,
        nodes,
        active,
        ring_size,
    })
}

fn expected_active_network(
    network: &SimNetwork,
    finger_table_size: usize,
) -> Result<SimNetwork, BenchError> {
    let known = network.active.iter().copied().collect::<Vec<_>>();
    let state_known_sets = network
        .active
        .iter()
        .copied()
        .map(|index| (index, known.clone()))
        .collect::<BTreeMap<_, _>>();
    build_network(
        network.identities.clone(),
        network.active.clone(),
        state_known_sets,
        finger_table_size,
    )
}

fn build_node(
    local_index: usize,
    known: &[usize],
    identities: &[Identity],
    ring_size: &BigUint,
    finger_table_size: usize,
) -> SimNode {
    let successors = successor_indices(local_index, known, DHT_SUCCESSOR_CAPACITY);
    let predecessor = predecessor_index(local_index, known);
    let fingers = (0..finger_table_size.min(RING_BITS))
        .map(|bit| finger_index(local_index, known, identities, ring_size, bit))
        .collect();
    SimNode {
        successors,
        predecessor,
        fingers,
    }
}

fn successor_indices(local_index: usize, known: &[usize], capacity: usize) -> Vec<usize> {
    let Some(position) = known.iter().position(|index| *index == local_index) else {
        return Vec::new();
    };
    (1..known.len().min(capacity.saturating_add(1)))
        .filter_map(|offset| known.get((position + offset) % known.len()).copied())
        .collect()
}

fn predecessor_index(local_index: usize, known: &[usize]) -> Option<usize> {
    let position = known.iter().position(|index| *index == local_index)?;
    known
        .get((position + known.len().saturating_sub(1)) % known.len())
        .copied()
        .filter(|index| *index != local_index)
}

fn finger_index(
    local_index: usize,
    known: &[usize],
    identities: &[Identity],
    ring_size: &BigUint,
    bit: usize,
) -> Option<usize> {
    let threshold = BigUint::from(1u8) << bit;
    let target = add_mod(&identities.get(local_index)?.value, &threshold, ring_size);
    let candidate = successor_at_or_after(known, identities, &target)?;
    if candidate == local_index {
        return None;
    }
    if clockwise_distance(
        &identities.get(local_index)?.value,
        &identities.get(candidate)?.value,
        ring_size,
    ) < threshold
    {
        return None;
    }
    Some(candidate)
}

fn successor_at_or_after(
    known: &[usize],
    identities: &[Identity],
    target: &BigUint,
) -> Option<usize> {
    if known.is_empty() {
        return None;
    }
    let position = known.partition_point(|index| {
        identities
            .get(*index)
            .map(|identity| identity.value < *target)
            .unwrap_or(false)
    });
    known
        .get(position)
        .copied()
        .or_else(|| known.first().copied())
}

fn topology_match_counts(
    network: &SimNetwork,
    expected: &SimNetwork,
) -> Result<(usize, usize), BenchError> {
    let mut topology_matches = 0usize;
    let mut full_matches = 0usize;
    for index in &network.active {
        let actual = network
            .nodes
            .get(*index)
            .and_then(Option::as_ref)
            .ok_or(BenchError::MissingNode(*index))?;
        let expected = expected
            .nodes
            .get(*index)
            .and_then(Option::as_ref)
            .ok_or(BenchError::MissingNode(*index))?;
        if actual.successors == expected.successors && actual.predecessor == expected.predecessor {
            topology_matches = topology_matches.saturating_add(1);
        }
        if actual == expected {
            full_matches = full_matches.saturating_add(1);
        }
    }
    Ok((topology_matches, full_matches))
}

fn lookup_metrics(
    network: &SimNetwork,
    lookups_per_node: usize,
    lookup_mode: LookupMode,
    finger_table_size: usize,
) -> Result<LookupMetrics, BenchError> {
    let active_indices = network.active.iter().copied().collect::<Vec<_>>();
    let mut accumulator = LookupAccumulator::default();

    for origin in &active_indices {
        for probe in 0..lookups_per_node {
            let target = lookup_target(network, *origin, probe, lookup_mode, finger_table_size)?;
            let Some(expected) =
                successor_at_or_after(&active_indices, &network.identities, &target.value)
            else {
                return Err(BenchError::NoActiveNodes);
            };
            let outcome = resolve_lookup(network, *origin, &target)?;
            accumulator.record(outcome, expected);
        }
    }

    Ok(accumulator.finish())
}

fn lookup_target(
    network: &SimNetwork,
    origin: usize,
    probe: usize,
    lookup_mode: LookupMode,
    finger_table_size: usize,
) -> Result<LookupTarget, BenchError> {
    match lookup_mode {
        LookupMode::Random => {
            let salt = origin
                .saturating_mul(1_000_003)
                .saturating_add(probe)
                .saturating_add(10_000_000);
            let did = deterministic_did(salt)?;
            let value = BigUint::from(did);
            Ok(LookupTarget { value })
        }
        LookupMode::FingerOffsets => {
            let bit = probe % finger_table_size.max(1).min(RING_BITS);
            let did = network
                .identities
                .get(origin)
                .map(|identity| identity.did + Did::power_of_two(bit))
                .ok_or(BenchError::MissingNode(origin))?;
            let value = BigUint::from(did);
            Ok(LookupTarget { value })
        }
    }
}

fn resolve_lookup(
    network: &SimNetwork,
    origin: usize,
    target: &LookupTarget,
) -> Result<LookupOutcome, BenchError> {
    let mut current = origin;
    let mut visited = BTreeSet::new();
    let mut timeouts = 0usize;

    for hops in 1..=MAX_LOOKUP_HOPS {
        if !network.active.contains(&current) || !visited.insert(current) {
            return Ok(LookupOutcome::Failed { timeouts });
        }
        let node = network
            .nodes
            .get(current)
            .and_then(Option::as_ref)
            .ok_or(BenchError::MissingNode(current))?;

        let current_value = &network
            .identities
            .get(current)
            .ok_or(BenchError::MissingNode(current))?
            .value;
        let target_distance = clockwise_distance(current_value, &target.value, &network.ring_size);
        if let Some((successor, missed)) =
            successor_for_terminal_step(node, &network.active, &target_distance, current, network)
        {
            timeouts = timeouts.saturating_add(missed);
            return Ok(LookupOutcome::Resolved {
                index: successor,
                hops,
                timeouts,
            });
        }

        let candidates = forwarding_candidates(node, current, network, &target_distance);
        let mut forwarded = false;
        for candidate in candidates {
            if !network.active.contains(&candidate) {
                timeouts = timeouts.saturating_add(1);
                continue;
            }
            current = candidate;
            forwarded = true;
            break;
        }
        if !forwarded {
            return Ok(LookupOutcome::Failed { timeouts });
        }
    }

    Ok(LookupOutcome::Failed { timeouts })
}

fn successor_for_terminal_step(
    node: &SimNode,
    active: &BTreeSet<usize>,
    target_distance: &BigUint,
    current: usize,
    network: &SimNetwork,
) -> Option<(usize, usize)> {
    let mut missed = 0usize;
    for successor in &node.successors {
        let successor_distance = distance_between_indices(network, current, *successor)?;
        if *target_distance <= successor_distance {
            if active.contains(successor) {
                return Some((*successor, missed));
            }
            missed = missed.saturating_add(1);
        }
    }
    None
}

fn forwarding_candidates(
    node: &SimNode,
    current: usize,
    network: &SimNetwork,
    target_distance: &BigUint,
) -> Vec<usize> {
    let mut scored = Vec::new();
    for candidate in node
        .fingers
        .iter()
        .rev()
        .flatten()
        .chain(node.successors.iter().rev())
    {
        if *candidate == current {
            continue;
        }
        let Some(distance) = distance_between_indices(network, current, *candidate) else {
            continue;
        };
        if distance > BigUint::from(0u8) && distance < *target_distance {
            scored.push((distance, *candidate));
        }
    }
    scored.sort_by(|(left_distance, left), (right_distance, right)| {
        right_distance
            .cmp(left_distance)
            .then_with(|| left.cmp(right))
    });

    let mut seen = BTreeSet::new();
    scored
        .into_iter()
        .filter_map(|(_, index)| {
            if seen.insert(index) {
                Some(index)
            } else {
                None
            }
        })
        .collect()
}

fn distance_between_indices(network: &SimNetwork, from: usize, to: usize) -> Option<BigUint> {
    Some(clockwise_distance(
        &network.identities.get(from)?.value,
        &network.identities.get(to)?.value,
        &network.ring_size,
    ))
}

impl LookupAccumulator {
    fn record(&mut self, outcome: LookupOutcome, expected: usize) {
        self.total = self.total.saturating_add(1);
        match outcome {
            LookupOutcome::Resolved {
                index,
                hops,
                timeouts,
            } => {
                self.resolved = self.resolved.saturating_add(1);
                self.total_hops = self.total_hops.saturating_add(hops);
                self.max_hops = self.max_hops.max(hops);
                self.timeouts = self.timeouts.saturating_add(timeouts);
                let bucket = self.hop_buckets.entry(hops).or_insert(0);
                *bucket = bucket.saturating_add(1);
                if index == expected {
                    self.correct = self.correct.saturating_add(1);
                } else {
                    self.failed = self.failed.saturating_add(1);
                }
            }
            LookupOutcome::Failed { timeouts } => {
                self.failed = self.failed.saturating_add(1);
                self.timeouts = self.timeouts.saturating_add(timeouts);
            }
        }
    }

    fn finish(self) -> LookupMetrics {
        LookupMetrics {
            total: self.total,
            resolved: self.resolved,
            correct: self.correct,
            failed: self.failed,
            success_rate: ratio(self.resolved, self.total),
            correctness_rate: ratio(self.correct, self.total),
            avg_hops: average(self.total_hops as u64, self.resolved),
            max_hops: self.max_hops,
            timeouts: self.timeouts,
            mean_lookup_timeouts: average(self.timeouts as u64, self.total),
            lookup_failures_per_10k: ratio(self.failed.saturating_mul(LOOKUPS_PER_10K), self.total),
            hop_buckets: self
                .hop_buckets
                .into_iter()
                .map(|(hops, count)| HopBucket { hops, count })
                .collect(),
        }
    }
}

fn finger_metrics(network: &SimNetwork) -> Result<FingerMetrics, BenchError> {
    let mut known_slots = Vec::with_capacity(network.active.len());
    for index in &network.active {
        let node = network
            .nodes
            .get(*index)
            .and_then(Option::as_ref)
            .ok_or(BenchError::MissingNode(*index))?;
        known_slots.push(
            node.fingers
                .iter()
                .filter(|finger| finger.is_some())
                .count(),
        );
    }
    let total = known_slots.iter().copied().sum::<usize>();
    Ok(FingerMetrics {
        known_slots_min: known_slots.iter().copied().min().unwrap_or(0),
        known_slots_avg: average(total as u64, known_slots.len()),
        known_slots_max: known_slots.iter().copied().max().unwrap_or(0),
    })
}

fn generate_identities(total: usize) -> Result<Vec<Identity>, BenchError> {
    let mut seen = BTreeSet::new();
    let mut identities = Vec::with_capacity(total);
    let mut ordinal = 0usize;
    while identities.len() < total {
        let did = deterministic_did(ordinal)?;
        if seen.insert(did) {
            identities.push(Identity {
                ordinal,
                value: BigUint::from(did),
                did,
            });
        }
        ordinal = ordinal.saturating_add(1);
    }
    identities.sort_by(|left, right| {
        left.value
            .cmp(&right.value)
            .then_with(|| left.ordinal.cmp(&right.ordinal))
    });
    Ok(identities)
}

fn deterministic_did(seed: usize) -> Result<Did, BenchError> {
    let mut state = seed as u64 ^ 0x9e37_79b9_7f4a_7c15;
    let a = splitmix64(&mut state);
    let b = splitmix64(&mut state);
    let c = splitmix64(&mut state);
    let value = format!("0x{a:016x}{b:016x}{:08x}", c >> 32);
    Did::from_str(&value).map_err(|source| BenchError::DidParse { value, source })
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn index_by_ordinal(identities: &[Identity], total_nodes: usize) -> Result<Vec<usize>, BenchError> {
    let mut index_by_ordinal = vec![usize::MAX; total_nodes];
    for (index, identity) in identities.iter().enumerate() {
        if let Some(slot) = index_by_ordinal.get_mut(identity.ordinal) {
            *slot = index;
        }
    }
    if index_by_ordinal.iter().any(|index| *index == usize::MAX) {
        return Err(BenchError::NoActiveNodes);
    }
    Ok(index_by_ordinal)
}

fn ordinals_to_indices(
    index_by_ordinal: &[usize],
    ordinals: impl Iterator<Item = usize>,
) -> Vec<usize> {
    let mut indices = ordinals
        .filter_map(|ordinal| index_by_ordinal.get(ordinal).copied())
        .collect::<Vec<_>>();
    indices.sort_unstable();
    indices
}

fn selected_positions(total: usize, pct: usize, salt: usize) -> BTreeSet<usize> {
    let count = selected_count(total, pct);
    let mut scored = (0..total)
        .map(|index| (selection_rank(index, salt, total), index))
        .collect::<Vec<_>>();
    scored.sort_by_key(|(rank, index)| (*rank, *index));
    scored
        .into_iter()
        .take(count)
        .map(|(_, index)| index)
        .collect()
}

fn selected_count(total: usize, pct: usize) -> usize {
    total
        .saturating_mul(pct)
        .saturating_add(50)
        .saturating_div(100)
        .min(total.saturating_sub(1))
}

fn selection_rank(index: usize, salt: usize, total: usize) -> usize {
    index.saturating_mul(7).saturating_add(salt) % total.max(1)
}

fn env_usize(name: &'static str, default: usize) -> Result<usize, BenchError> {
    match env::var(name) {
        Ok(value) => value.parse().map_err(|source| BenchError::EnvUsize {
            name,
            value,
            source,
        }),
        Err(_) => Ok(default),
    }
}

fn env_usize_list(name: &'static str) -> Result<Option<Vec<usize>>, BenchError> {
    let Ok(value) = env::var(name) else {
        return Ok(None);
    };
    let mut values = Vec::new();
    for raw in value
        .split(',')
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    {
        values.push(raw.parse().map_err(|source| BenchError::EnvUsize {
            name,
            value: raw.to_string(),
            source,
        })?);
    }
    if values.is_empty() {
        return Err(BenchError::EmptyList { name });
    }
    Ok(Some(values))
}

fn finger_table_sizes() -> Result<Vec<usize>, BenchError> {
    if let Some(values) = env_usize_list("RINGS_DHT_BENCH_FINGER_TABLE_SIZES")? {
        return Ok(values);
    }
    if let Some(values) = env_usize_list("RINGS_DHT_FINGER_TABLE_SIZE")? {
        return Ok(values);
    }
    Ok(DEFAULT_FINGER_TABLE_SIZES.to_vec())
}

fn percent_list(name: &'static str, default: &[usize]) -> Result<Vec<usize>, BenchError> {
    env_usize_list(name).map(|values| values.unwrap_or_else(|| default.to_vec()))
}

fn lookup_mode() -> Result<LookupMode, BenchError> {
    let raw =
        env::var("RINGS_DHT_BENCH_LOOKUP_MODE").unwrap_or_else(|_| DEFAULT_LOOKUP_MODE.to_string());
    match raw.as_str() {
        "random" => Ok(LookupMode::Random),
        "finger_offsets" => Ok(LookupMode::FingerOffsets),
        other => Err(BenchError::UnsupportedLookupMode(other.to_string())),
    }
}

fn add_mod(value: &BigUint, offset: &BigUint, modulus: &BigUint) -> BigUint {
    (value + offset) % modulus
}

fn clockwise_distance(from: &BigUint, to: &BigUint, ring_size: &BigUint) -> BigUint {
    if to >= from {
        to - from
    } else {
        ring_size - (from - to)
    }
}

fn ring_size() -> BigUint {
    BigUint::from(1u8) << RING_BITS
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    numerator as f64 / denominator as f64
}

fn average(total: u64, count: usize) -> f64 {
    if count == 0 {
        return 0.0;
    }
    total as f64 / count as f64
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
