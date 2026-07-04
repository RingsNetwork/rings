use std::collections::BTreeMap;
use std::env;
use std::sync::Arc;

use rings_core::dht::topology;
use rings_core::dht::Chord;
use rings_core::dht::Did;
use rings_core::dht::PeerRing;
use rings_core::dht::PeerRingAction;
use rings_core::ecc::SecretKey;
use rings_core::inspect::DHTInspect;
use rings_core::session::SessionSk;
use rings_core::storage::MemStorage;
use rings_core::swarm::Swarm;
use rings_core::swarm::SwarmBuilder;
use rings_transport::connections::dummy_controlled;
use serde::Serialize;

const DEFAULT_NODE_COUNT: usize = 16;
const DEFAULT_MAX_ROUNDS: usize = 160;
const DEFAULT_DELIVERY_BUDGET: usize = 5_000_000;
const DEFAULT_LOOKUP_TARGET_BITS: usize = 16;
const MAX_LOOKUP_HOPS: usize = 64;
const DEFAULT_FINGER_TABLE_SIZES: &[usize] = &[4, 16];
const DHT_SUCCESSOR_CAPACITY: u8 = 3;

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
    #[error("unsupported DHT benchmark scenario {0:?}")]
    UnsupportedScenario(String),
    #[error("unsupported DHT benchmark bootstrap topology {0:?}")]
    UnsupportedBootstrap(String),
    #[error("cannot build deterministic secret key #{index}: {source}")]
    SecretKey {
        index: usize,
        source: rings_core::error::Error,
    },
    #[error("cannot build session key: {0}")]
    Session(#[from] rings_core::error::Error),
    #[error("node count must be greater than one")]
    TooFewNodes,
    #[error("missing swarm at index {0}")]
    MissingSwarm(usize),
    #[error("lookup forwarded to unknown DHT {0}")]
    UnknownLookupNextHop(Did),
    #[error("failed to encode benchmark report: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Copy)]
enum DeliveryOrder {
    Fifo,
    Lifo,
    Jitter,
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    order: DeliveryOrder,
    drop_every: Option<usize>,
}

#[derive(Default)]
struct ControlledDeliveryGuard;

impl ControlledDeliveryGuard {
    fn enable() -> Self {
        dummy_controlled::enable(true);
        Self
    }
}

impl Drop for ControlledDeliveryGuard {
    fn drop(&mut self) {
        dummy_controlled::enable(false);
    }
}

#[derive(Default)]
struct DrainStats {
    delivered: usize,
    dropped: usize,
    max_pending: usize,
    budget_exhausted: bool,
}

#[derive(Default, Serialize)]
struct DeliveryMetrics {
    delivered: usize,
    dropped: usize,
    max_pending: usize,
    budget_exhausted: bool,
}

#[derive(Serialize)]
struct FingerMetrics {
    known_slots_min: u64,
    known_slots_max: u64,
    known_slots_avg: f64,
}

#[derive(Serialize)]
struct HopBucket {
    hops: usize,
    count: usize,
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
    hop_buckets: Vec<HopBucket>,
}

#[derive(Serialize)]
struct ScenarioReport {
    scenario: String,
    bootstrap: String,
    node_count: usize,
    finger_table_size: usize,
    max_rounds: usize,
    converged: bool,
    converged_round: Option<usize>,
    topology_matches: usize,
    full_matches: usize,
    delivery: DeliveryMetrics,
    fingers: FingerMetrics,
    lookups: LookupMetrics,
    stabilize_errors: Vec<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), BenchError> {
    let node_count = env_usize("RINGS_DHT_BENCH_NODES", DEFAULT_NODE_COUNT)?;
    let max_rounds = env_usize("RINGS_DHT_BENCH_MAX_ROUNDS", DEFAULT_MAX_ROUNDS)?;
    let delivery_budget = env_usize("RINGS_DHT_BENCH_DELIVERY_BUDGET", DEFAULT_DELIVERY_BUDGET)?;
    let finger_table_sizes = finger_table_sizes()?;
    let scenarios = scenarios()?;
    let bootstrap = bootstrap_topology()?;

    for finger_table_size in finger_table_sizes {
        for scenario in &scenarios {
            let report = run_scenario(
                *scenario,
                bootstrap,
                node_count,
                finger_table_size,
                max_rounds,
                delivery_budget,
            )
            .await?;
            println!("{}", serde_json::to_string(&report)?);
        }
    }

    Ok(())
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

fn scenarios() -> Result<Vec<Scenario>, BenchError> {
    let raw = env::var("RINGS_DHT_BENCH_SCENARIOS")
        .unwrap_or_else(|_| "stable,lifo,jitter,loss".to_string());
    let mut scenarios = Vec::new();
    for name in raw
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        scenarios.push(match name {
            "stable" => Scenario {
                name: "stable",
                order: DeliveryOrder::Fifo,
                drop_every: None,
            },
            "lifo" => Scenario {
                name: "lifo",
                order: DeliveryOrder::Lifo,
                drop_every: None,
            },
            "jitter" => Scenario {
                name: "jitter",
                order: DeliveryOrder::Jitter,
                drop_every: None,
            },
            "loss" => Scenario {
                name: "loss",
                order: DeliveryOrder::Jitter,
                drop_every: Some(23),
            },
            other => return Err(BenchError::UnsupportedScenario(other.to_string())),
        });
    }
    if scenarios.is_empty() {
        return Err(BenchError::EmptyList {
            name: "RINGS_DHT_BENCH_SCENARIOS",
        });
    }
    Ok(scenarios)
}

#[derive(Clone, Copy)]
enum BootstrapTopology {
    Star,
    Mesh,
}

impl BootstrapTopology {
    fn name(self) -> &'static str {
        match self {
            Self::Star => "star",
            Self::Mesh => "mesh",
        }
    }
}

fn bootstrap_topology() -> Result<BootstrapTopology, BenchError> {
    let raw = env::var("RINGS_DHT_BENCH_BOOTSTRAP").unwrap_or_else(|_| "mesh".to_string());
    match raw.as_str() {
        "star" => Ok(BootstrapTopology::Star),
        "mesh" => Ok(BootstrapTopology::Mesh),
        other => Err(BenchError::UnsupportedBootstrap(other.to_string())),
    }
}

async fn run_scenario(
    scenario: Scenario,
    bootstrap: BootstrapTopology,
    node_count: usize,
    finger_table_size: usize,
    max_rounds: usize,
    delivery_budget: usize,
) -> Result<ScenarioReport, BenchError> {
    let swarms = build_swarms(node_count, finger_table_size)?;
    let expected = expected_dhts(&swarms, finger_table_size)?;
    let _controlled = ControlledDeliveryGuard::enable();
    let mut delivery = DeliveryMetrics::default();
    let mut event_index = 0usize;
    let mut stabilize_errors = Vec::new();

    bootstrap_swarms(bootstrap, &swarms).await?;
    delivery.merge(drain(scenario, false, delivery_budget, &mut event_index).await);

    let mut converged_round = None;
    for round in 1..=max_rounds {
        for swarm in &swarms {
            if let Err(error) = swarm.stabilizer().stabilize().await {
                stabilize_errors.push(error.to_string());
            }
        }
        delivery.merge(drain(scenario, true, delivery_budget, &mut event_index).await);
        let actual = inspect_all(&swarms);
        if actual == expected {
            converged_round = Some(round);
            break;
        }
        if delivery.budget_exhausted {
            break;
        }
    }

    let actual = inspect_all(&swarms);
    let topology_matches = actual
        .iter()
        .zip(&expected)
        .filter(|(actual, expected)| topology_matches(actual, expected))
        .count();
    let full_matches = actual
        .iter()
        .zip(&expected)
        .filter(|(actual, expected)| actual == expected)
        .count();

    Ok(ScenarioReport {
        scenario: scenario.name.to_string(),
        bootstrap: bootstrap.name().to_string(),
        node_count,
        finger_table_size,
        max_rounds,
        converged: converged_round.is_some(),
        converged_round,
        topology_matches,
        full_matches,
        delivery,
        fingers: finger_metrics(&actual),
        lookups: lookup_metrics(&swarms, finger_table_size)?,
        stabilize_errors,
    })
}

impl DeliveryMetrics {
    fn merge(&mut self, stats: DrainStats) {
        self.delivered = self.delivered.saturating_add(stats.delivered);
        self.dropped = self.dropped.saturating_add(stats.dropped);
        self.max_pending = self.max_pending.max(stats.max_pending);
        self.budget_exhausted |= stats.budget_exhausted;
    }
}

fn build_swarms(
    node_count: usize,
    finger_table_size: usize,
) -> Result<Vec<Arc<Swarm>>, BenchError> {
    if node_count <= 1 {
        return Err(BenchError::TooFewNodes);
    }

    let mut swarms = Vec::with_capacity(node_count);
    for i in 0..node_count {
        let key = deterministic_secret(i)?;
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let swarm = SwarmBuilder::new(
            0,
            "stun://stun.l.google.com:19302",
            Box::new(MemStorage::new()),
            session_sk,
        )
        .dht_finger_table_size(finger_table_size)
        .build();
        swarms.push(Arc::new(swarm));
    }

    Ok(swarms)
}

fn deterministic_secret(index: usize) -> Result<SecretKey, BenchError> {
    let key = format!("{:064x}", index.saturating_add(1));
    SecretKey::try_from(key.as_str()).map_err(|source| BenchError::SecretKey { index, source })
}

async fn bootstrap_star(swarms: &[Arc<Swarm>]) -> Result<(), BenchError> {
    let Some((hub, leaves)) = swarms.split_first() else {
        return Err(BenchError::TooFewNodes);
    };
    for leaf in leaves {
        establish_connection(hub, leaf).await?;
    }
    Ok(())
}

async fn bootstrap_swarms(
    topology: BootstrapTopology,
    swarms: &[Arc<Swarm>],
) -> Result<(), BenchError> {
    match topology {
        BootstrapTopology::Star => bootstrap_star(swarms).await,
        BootstrapTopology::Mesh => bootstrap_mesh(swarms).await,
    }
}

async fn bootstrap_mesh(swarms: &[Arc<Swarm>]) -> Result<(), BenchError> {
    if swarms.len() <= 1 {
        return Err(BenchError::TooFewNodes);
    }

    for (index, a) in swarms.iter().enumerate() {
        for b in swarms.iter().skip(index.saturating_add(1)) {
            establish_connection(a, b).await?;
        }
    }

    Ok(())
}

async fn establish_connection(a: &Swarm, b: &Swarm) -> Result<(), BenchError> {
    let offer = a.create_offer(b.did()).await?;
    let answer = b.answer_offer(offer).await?;
    a.accept_answer(answer).await?;
    Ok(())
}

fn expected_dhts(
    swarms: &[Arc<Swarm>],
    finger_table_size: usize,
) -> Result<Vec<DHTInspect>, BenchError> {
    let mut expected = Vec::with_capacity(swarms.len());
    for swarm in swarms {
        let dht = PeerRing::new_with_storage_and_finger_table_size(
            swarm.did(),
            DHT_SUCCESSOR_CAPACITY,
            Box::new(MemStorage::new()),
            finger_table_size,
        );
        for other in swarms {
            if dht.did != other.did() {
                dht.join(other.did())?;
                dht.notify(other.did())?;
            }
        }
        expected.push(DHTInspect::inspect(&dht));
    }
    Ok(expected)
}

async fn drain(
    scenario: Scenario,
    allow_drop: bool,
    delivery_budget: usize,
    event_index: &mut usize,
) -> DrainStats {
    let mut stats = DrainStats::default();
    while dummy_controlled::pending() > 0 {
        if *event_index >= delivery_budget {
            stats.budget_exhausted = true;
            break;
        }
        let pending = dummy_controlled::pending();
        stats.max_pending = stats.max_pending.max(pending);
        let index = pick_index(scenario.order, pending, *event_index);
        *event_index = event_index.saturating_add(1);

        if should_drop(scenario, allow_drop, *event_index) {
            if dummy_controlled::drop_queued(index) {
                stats.dropped = stats.dropped.saturating_add(1);
            }
            continue;
        }

        if dummy_controlled::deliver(index).await {
            stats.delivered = stats.delivered.saturating_add(1);
        } else {
            stats.dropped = stats.dropped.saturating_add(1);
        }
    }
    stats
}

fn pick_index(order: DeliveryOrder, pending: usize, event_index: usize) -> usize {
    match order {
        DeliveryOrder::Fifo => 0,
        DeliveryOrder::Lifo => pending.saturating_sub(1),
        DeliveryOrder::Jitter => {
            event_index.saturating_mul(7).saturating_add(pending / 2) % pending
        }
    }
}

fn should_drop(scenario: Scenario, allow_drop: bool, event_index: usize) -> bool {
    match (allow_drop, scenario.drop_every) {
        (true, Some(drop_every)) => event_index.is_multiple_of(drop_every),
        _ => false,
    }
}

fn inspect_all(swarms: &[Arc<Swarm>]) -> Vec<DHTInspect> {
    swarms
        .iter()
        .map(|swarm| DHTInspect::inspect(&swarm.dht()))
        .collect()
}

fn topology_matches(actual: &DHTInspect, expected: &DHTInspect) -> bool {
    actual.successors == expected.successors && actual.predecessor == expected.predecessor
}

fn finger_metrics(actual: &[DHTInspect]) -> FingerMetrics {
    let known_slots: Vec<u64> = actual.iter().map(known_finger_slots).collect();
    let total: u64 = known_slots.iter().sum();
    let count = known_slots.len();
    FingerMetrics {
        known_slots_min: known_slots.iter().copied().min().unwrap_or(0),
        known_slots_max: known_slots.iter().copied().max().unwrap_or(0),
        known_slots_avg: average(total, count),
    }
}

fn known_finger_slots(dht: &DHTInspect) -> u64 {
    dht.finger_table
        .iter()
        .filter(|(did, _, _)| did.is_some())
        .map(|(_, start, end)| end.saturating_sub(*start).saturating_add(1))
        .sum()
}

fn lookup_metrics(
    swarms: &[Arc<Swarm>],
    finger_table_size: usize,
) -> Result<LookupMetrics, BenchError> {
    let all_dids: Vec<Did> = swarms.iter().map(|swarm| swarm.did()).collect();
    let index_by_did: BTreeMap<Did, usize> = all_dids
        .iter()
        .copied()
        .enumerate()
        .map(|(index, did)| (did, index))
        .collect();
    let probe_count = finger_table_size.clamp(1, DEFAULT_LOOKUP_TARGET_BITS);
    let mut metrics = LookupAccumulator::default();

    for origin_index in 0..swarms.len() {
        let Some(origin) = swarms.get(origin_index) else {
            return Err(BenchError::MissingSwarm(origin_index));
        };
        for bit in 0..probe_count {
            let target = origin.did() + Did::power_of_two(bit);
            if let Some(expected) = expected_successor(&all_dids, target) {
                let outcome = resolve_lookup(swarms, &index_by_did, origin_index, target)?;
                metrics.record(outcome, expected);
            }
        }
    }

    Ok(metrics.finish())
}

fn expected_successor(all_dids: &[Did], target: Did) -> Option<Did> {
    topology::successors(all_dids, target, 1).into_iter().next()
}

enum LookupOutcome {
    Resolved { did: Did, hops: usize },
    Failed,
}

fn resolve_lookup(
    swarms: &[Arc<Swarm>],
    index_by_did: &BTreeMap<Did, usize>,
    origin_index: usize,
    target: Did,
) -> Result<LookupOutcome, BenchError> {
    let mut current_index = origin_index;
    for hops in 1..=MAX_LOOKUP_HOPS {
        let current = swarms
            .get(current_index)
            .ok_or(BenchError::MissingSwarm(current_index))?;
        match current.dht().find_successor(target)? {
            PeerRingAction::Some(did) => return Ok(LookupOutcome::Resolved { did, hops }),
            PeerRingAction::RemoteAction(next, _) => {
                if next == current.did() {
                    return Ok(LookupOutcome::Failed);
                }
                current_index = *index_by_did
                    .get(&next)
                    .ok_or(BenchError::UnknownLookupNextHop(next))?;
            }
            _ => return Ok(LookupOutcome::Failed),
        }
    }
    Ok(LookupOutcome::Failed)
}

#[derive(Default)]
struct LookupAccumulator {
    total: usize,
    resolved: usize,
    correct: usize,
    failed: usize,
    total_hops: usize,
    max_hops: usize,
    hop_buckets: BTreeMap<usize, usize>,
}

impl LookupAccumulator {
    fn record(&mut self, outcome: LookupOutcome, expected: Did) {
        self.total = self.total.saturating_add(1);
        match outcome {
            LookupOutcome::Resolved { did, hops } => {
                self.resolved = self.resolved.saturating_add(1);
                self.total_hops = self.total_hops.saturating_add(hops);
                self.max_hops = self.max_hops.max(hops);
                let bucket = self.hop_buckets.entry(hops).or_insert(0);
                *bucket = bucket.saturating_add(1);
                if did == expected {
                    self.correct = self.correct.saturating_add(1);
                }
            }
            LookupOutcome::Failed => {
                self.failed = self.failed.saturating_add(1);
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
            hop_buckets: self
                .hop_buckets
                .into_iter()
                .map(|(hops, count)| HopBucket { hops, count })
                .collect(),
        }
    }
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
