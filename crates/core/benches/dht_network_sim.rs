use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::dht::topology;
use rings_core::dht::Chord;
use rings_core::dht::Did;
use rings_core::dht::PeerRing;
use rings_core::dht::PeerRingAction;
use rings_core::ecc::SecretKey;
use rings_core::inspect::DHTInspect;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::session::SessionSk;
use rings_core::storage::MemStorage;
use rings_core::swarm::callback::SwarmCallback;
use rings_core::swarm::Swarm;
use rings_core::swarm::SwarmBuilder;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio::time::Instant;

const DEFAULT_NODE_COUNT: usize = 16;
const DEFAULT_MAX_ROUNDS: usize = 80;
const DEFAULT_LOOKUP_TARGET_BITS: usize = 16;
const DEFAULT_FINGER_TABLE_SIZES: &[usize] = &[4, 16];
const DEFAULT_FAILED_NODE_PCTS: &[usize] = &[0, 10, 20, 50];
const DEFAULT_CHURN_RATE_PCTS: &[usize] = &[5, 10, 20, 40];
const DEFAULT_THROUGHPUT_MESSAGES: usize = 64;
const DEFAULT_THROUGHPUT_PAYLOAD_BYTES: usize = 16 * 1024;
const DEFAULT_WAIT_TIMEOUT_MS: usize = 20_000;
const MAX_LOOKUP_HOPS: usize = 64;
const DHT_SUCCESSOR_CAPACITY: u8 = 3;
const LOOKUPS_PER_10K: usize = 10_000;
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

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
    #[error("unsupported DHT benchmark bootstrap topology {0:?}")]
    UnsupportedBootstrap(String),
    #[error("cannot build deterministic secret key #{index}: {source}")]
    SecretKey {
        index: usize,
        source: rings_core::error::Error,
    },
    #[error("node count must be greater than one")]
    TooFewNodes,
    #[error("missing node at index {0}")]
    MissingNode(usize),
    #[error("timed out waiting for {label} after {timeout_ms} ms")]
    Timeout { label: String, timeout_ms: usize },
    #[error("custom-message callback channel closed")]
    CallbackClosed,
    #[error("rings core error: {0}")]
    Core(#[from] rings_core::error::Error),
    #[error("failed to encode benchmark report: {0}")]
    Json(#[from] serde_json::Error),
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
struct DhtReport {
    report: &'static str,
    scenario: &'static str,
    bootstrap: String,
    node_count: usize,
    active_nodes: usize,
    finger_table_size: usize,
    max_rounds: usize,
    handshake_elapsed_ms: f64,
    connected_directed_edges: usize,
    converged: bool,
    converged_round: Option<usize>,
    topology_matches: usize,
    full_matches: usize,
    failed_node_pct: Option<usize>,
    failed_nodes: Option<usize>,
    join_leave_rate_pct: Option<usize>,
    departed_nodes: Option<usize>,
    joined_nodes: Option<usize>,
    lookups: LookupMetrics,
    stabilize_errors: Vec<String>,
    disconnect_errors: Vec<String>,
}

#[derive(Serialize)]
struct TransportReport {
    report: &'static str,
    node_count: usize,
    finger_table_size: usize,
    payload_bytes: usize,
    messages: usize,
    sent_messages: usize,
    received_messages: usize,
    total_payload_bytes: usize,
    handshake_elapsed_ms: f64,
    send_elapsed_ms: f64,
    receive_elapsed_ms: f64,
    send_mbps: f64,
    receive_mbps: f64,
}

struct BenchNode {
    swarm: Arc<Swarm>,
    custom_rx: Mutex<mpsc::UnboundedReceiver<usize>>,
}

struct BenchCallback {
    custom_tx: mpsc::UnboundedSender<usize>,
}

#[derive(Debug)]
struct CallbackSendClosed;

impl fmt::Display for CallbackSendClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("custom-message receiver is closed")
    }
}

impl StdError for CallbackSendClosed {}

#[async_trait]
impl SwarmCallback for BenchCallback {
    async fn on_inbound(&self, payload: &MessagePayload) -> Result<(), Box<dyn StdError>> {
        if let Ok(Message::CustomMessage(message)) = payload.transaction.data::<Message>() {
            self.custom_tx
                .send(message.0.len())
                .map_err(|_| Box::new(CallbackSendClosed) as Box<dyn StdError>)?;
        }
        Ok(())
    }
}

impl BenchNode {
    fn new(swarm: Arc<Swarm>) -> Result<Self, BenchError> {
        let (custom_tx, custom_rx) = mpsc::unbounded_channel();
        swarm.set_callback(Arc::new(BenchCallback { custom_tx }))?;
        Ok(Self {
            swarm,
            custom_rx: Mutex::new(custom_rx),
        })
    }

    fn did(&self) -> Did {
        self.swarm.did()
    }

    async fn receive_custom_messages(
        &self,
        expected_messages: usize,
        wait_timeout: Duration,
    ) -> Result<usize, BenchError> {
        let deadline = Instant::now() + wait_timeout;
        let mut received_messages = 0usize;
        let mut received_bytes = 0usize;
        let mut rx = self.custom_rx.lock().await;

        while received_messages < expected_messages {
            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                return Err(timeout_error("custom message receipt", wait_timeout));
            };
            match timeout(remaining, rx.recv()).await {
                Ok(Some(bytes)) => {
                    received_messages = received_messages.saturating_add(1);
                    received_bytes = received_bytes.saturating_add(bytes);
                }
                Ok(None) => return Err(BenchError::CallbackClosed),
                Err(_) => return Err(timeout_error("custom message receipt", wait_timeout)),
            }
        }

        Ok(received_bytes)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), BenchError> {
    let node_count = env_usize("RINGS_DHT_BENCH_NODES", DEFAULT_NODE_COUNT)?;
    let max_rounds = env_usize("RINGS_DHT_BENCH_MAX_ROUNDS", DEFAULT_MAX_ROUNDS)?;
    let finger_table_sizes = finger_table_sizes()?;
    let bootstrap = bootstrap_topology()?;
    let failed_node_pcts =
        percent_list("RINGS_DHT_BENCH_FAILED_NODE_PCTS", DEFAULT_FAILED_NODE_PCTS)?;
    let churn_rate_pcts = percent_list("RINGS_DHT_BENCH_CHURN_RATE_PCTS", DEFAULT_CHURN_RATE_PCTS)?;
    let throughput_messages = env_usize(
        "RINGS_DHT_BENCH_THROUGHPUT_MESSAGES",
        DEFAULT_THROUGHPUT_MESSAGES,
    )?;
    let throughput_payload_bytes = env_usize(
        "RINGS_DHT_BENCH_THROUGHPUT_PAYLOAD_BYTES",
        DEFAULT_THROUGHPUT_PAYLOAD_BYTES,
    )?;
    let wait_timeout = Duration::from_millis(env_usize(
        "RINGS_DHT_BENCH_WAIT_TIMEOUT_MS",
        DEFAULT_WAIT_TIMEOUT_MS,
    )? as u64);

    for &finger_table_size in &finger_table_sizes {
        let stable = run_dht_scenario(
            ScenarioSpec::Stable,
            node_count,
            finger_table_size,
            bootstrap,
            max_rounds,
            wait_timeout,
        )
        .await?;
        println!("{}", serde_json::to_string(&stable)?);

        for &failed_node_pct in &failed_node_pcts {
            if failed_node_pct == 0 {
                continue;
            }
            let report = run_dht_scenario(
                ScenarioSpec::FailedNodes { failed_node_pct },
                node_count,
                finger_table_size,
                bootstrap,
                max_rounds,
                wait_timeout,
            )
            .await?;
            println!("{}", serde_json::to_string(&report)?);
        }

        for &join_leave_rate_pct in &churn_rate_pcts {
            if join_leave_rate_pct == 0 {
                continue;
            }
            let report = run_dht_scenario(
                ScenarioSpec::Churn {
                    join_leave_rate_pct,
                },
                node_count,
                finger_table_size,
                bootstrap,
                max_rounds,
                wait_timeout,
            )
            .await?;
            println!("{}", serde_json::to_string(&report)?);
        }

        let transport = run_transport_report(
            finger_table_size,
            throughput_payload_bytes,
            throughput_messages,
            wait_timeout,
        )
        .await?;
        println!("{}", serde_json::to_string(&transport)?);
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum ScenarioSpec {
    Stable,
    FailedNodes { failed_node_pct: usize },
    Churn { join_leave_rate_pct: usize },
}

impl ScenarioSpec {
    fn name(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::FailedNodes { .. } => "failed_nodes",
            Self::Churn { .. } => "churn",
        }
    }
}

async fn run_dht_scenario(
    scenario: ScenarioSpec,
    node_count: usize,
    finger_table_size: usize,
    bootstrap: BootstrapTopology,
    max_rounds: usize,
    wait_timeout: Duration,
) -> Result<DhtReport, BenchError> {
    let churn_count = match scenario {
        ScenarioSpec::Churn {
            join_leave_rate_pct,
        } => selected_count(node_count, join_leave_rate_pct),
        _ => 0,
    };
    let total_nodes = node_count.saturating_add(churn_count);
    let nodes = build_nodes(total_nodes, finger_table_size)?;
    let initial_active = initial_active_indices(node_count);
    let initial_refs = node_refs(&nodes, &initial_active);
    let mut handshake_elapsed = bootstrap_swarms(bootstrap, &initial_refs, wait_timeout).await?;
    let mut disconnect_errors = Vec::new();
    let mut active = initial_active;
    let mut failed_nodes = None;
    let mut departed_nodes = None;
    let mut joined_nodes = None;

    match scenario {
        ScenarioSpec::Stable => {}
        ScenarioSpec::FailedNodes { failed_node_pct } => {
            let failed = selected_positions(node_count, failed_node_pct, 3);
            failed_nodes = Some(failed.len());
            disconnect_errors.extend(disconnect_positions(&nodes, &failed).await);
            active.retain(|index| !failed.contains(index));
        }
        ScenarioSpec::Churn {
            join_leave_rate_pct,
        } => {
            let departed = selected_positions(node_count, join_leave_rate_pct, 11);
            let joined = joiner_positions(node_count, churn_count);
            departed_nodes = Some(departed.len());
            joined_nodes = Some(joined.len());
            disconnect_errors.extend(disconnect_positions(&nodes, &departed).await);
            active.retain(|index| !departed.contains(index));
            active.extend(joined.iter().copied());
            handshake_elapsed += connect_joiners(&nodes, &active, &joined, wait_timeout).await?;
        }
    }

    let active_refs = node_refs(&nodes, &active);
    let dht = stabilize_and_measure(&active_refs, finger_table_size, max_rounds).await?;

    Ok(DhtReport {
        report: "webrtc_dht",
        scenario: scenario.name(),
        bootstrap: bootstrap.name().to_string(),
        node_count: total_nodes,
        active_nodes: active_refs.len(),
        finger_table_size,
        max_rounds,
        handshake_elapsed_ms: millis(handshake_elapsed),
        connected_directed_edges: connected_directed_edges(&active_refs),
        converged: dht.converged_round.is_some(),
        converged_round: dht.converged_round,
        topology_matches: dht.topology_matches,
        full_matches: dht.full_matches,
        failed_node_pct: match scenario {
            ScenarioSpec::FailedNodes { failed_node_pct } => Some(failed_node_pct),
            _ => None,
        },
        failed_nodes,
        join_leave_rate_pct: match scenario {
            ScenarioSpec::Churn {
                join_leave_rate_pct,
            } => Some(join_leave_rate_pct),
            _ => None,
        },
        departed_nodes,
        joined_nodes,
        lookups: dht.lookups,
        stabilize_errors: dht.stabilize_errors,
        disconnect_errors,
    })
}

struct DhtRun {
    converged_round: Option<usize>,
    topology_matches: usize,
    full_matches: usize,
    lookups: LookupMetrics,
    stabilize_errors: Vec<String>,
}

async fn stabilize_and_measure(
    nodes: &[&BenchNode],
    finger_table_size: usize,
    max_rounds: usize,
) -> Result<DhtRun, BenchError> {
    let expected = expected_dhts(nodes, finger_table_size)?;
    let mut stabilize_errors = Vec::new();
    let mut converged_round = None;

    for round in 1..=max_rounds {
        for node in nodes {
            if let Err(error) = node.swarm.stabilizer().stabilize().await {
                stabilize_errors.push(error.to_string());
            }
        }
        sleep(WAIT_POLL_INTERVAL).await;
        let actual = inspect_all(nodes);
        if actual == expected {
            converged_round = Some(round);
            break;
        }
    }

    let actual = inspect_all(nodes);
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

    Ok(DhtRun {
        converged_round,
        topology_matches,
        full_matches,
        lookups: lookup_metrics(nodes, finger_table_size)?,
        stabilize_errors,
    })
}

async fn run_transport_report(
    finger_table_size: usize,
    payload_bytes: usize,
    messages: usize,
    wait_timeout: Duration,
) -> Result<TransportReport, BenchError> {
    let nodes = build_nodes(2, finger_table_size)?;
    let refs = nodes.iter().collect::<Vec<_>>();
    let handshake_elapsed = bootstrap_swarms(BootstrapTopology::Mesh, &refs, wait_timeout).await?;
    let Some((source, rest)) = nodes.split_first() else {
        return Err(BenchError::TooFewNodes);
    };
    let Some(destination) = rest.first() else {
        return Err(BenchError::TooFewNodes);
    };
    let payload = vec![0x5a; payload_bytes];
    let total_payload_bytes = payload_bytes.saturating_mul(messages);
    let send_started = Instant::now();

    for _ in 0..messages {
        source
            .swarm
            .send_message(Message::custom(&payload)?, destination.did())
            .await?;
    }

    let send_elapsed = send_started.elapsed();
    let receive_started = Instant::now();
    let received_bytes = destination
        .receive_custom_messages(messages, wait_timeout)
        .await?;
    let receive_elapsed = receive_started.elapsed();

    Ok(TransportReport {
        report: "webrtc_transport",
        node_count: 2,
        finger_table_size,
        payload_bytes,
        messages,
        sent_messages: messages,
        received_messages: if payload_bytes == 0 {
            messages
        } else {
            received_bytes / payload_bytes
        },
        total_payload_bytes,
        handshake_elapsed_ms: millis(handshake_elapsed),
        send_elapsed_ms: millis(send_elapsed),
        receive_elapsed_ms: millis(receive_elapsed),
        send_mbps: mbps(total_payload_bytes, send_elapsed),
        receive_mbps: mbps(received_bytes, receive_elapsed),
    })
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

fn bootstrap_topology() -> Result<BootstrapTopology, BenchError> {
    let raw = env::var("RINGS_DHT_BENCH_BOOTSTRAP").unwrap_or_else(|_| "star".to_string());
    match raw.as_str() {
        "star" => Ok(BootstrapTopology::Star),
        "mesh" => Ok(BootstrapTopology::Mesh),
        other => Err(BenchError::UnsupportedBootstrap(other.to_string())),
    }
}

fn build_nodes(node_count: usize, finger_table_size: usize) -> Result<Vec<BenchNode>, BenchError> {
    if node_count <= 1 {
        return Err(BenchError::TooFewNodes);
    }

    let mut nodes = Vec::with_capacity(node_count);
    for index in 0..node_count {
        let key = deterministic_secret(index)?;
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let swarm = Arc::new(
            SwarmBuilder::new(
                0,
                "stun://stun.l.google.com:19302",
                Box::new(MemStorage::new()),
                session_sk,
            )
            .dht_finger_table_size(finger_table_size)
            .build(),
        );
        nodes.push(BenchNode::new(swarm)?);
    }

    Ok(nodes)
}

fn deterministic_secret(index: usize) -> Result<SecretKey, BenchError> {
    let key = format!("{:064x}", index.saturating_add(1));
    SecretKey::try_from(key.as_str()).map_err(|source| BenchError::SecretKey { index, source })
}

async fn bootstrap_swarms(
    topology: BootstrapTopology,
    nodes: &[&BenchNode],
    wait_timeout: Duration,
) -> Result<Duration, BenchError> {
    match topology {
        BootstrapTopology::Star => bootstrap_star(nodes, wait_timeout).await,
        BootstrapTopology::Mesh => bootstrap_mesh(nodes, wait_timeout).await,
    }
}

async fn bootstrap_star(
    nodes: &[&BenchNode],
    wait_timeout: Duration,
) -> Result<Duration, BenchError> {
    let Some((hub, leaves)) = nodes.split_first() else {
        return Err(BenchError::TooFewNodes);
    };
    let mut elapsed = Duration::ZERO;
    for leaf in leaves {
        elapsed += establish_connection(hub, leaf, wait_timeout).await?;
    }
    Ok(elapsed)
}

async fn bootstrap_mesh(
    nodes: &[&BenchNode],
    wait_timeout: Duration,
) -> Result<Duration, BenchError> {
    if nodes.len() <= 1 {
        return Err(BenchError::TooFewNodes);
    }

    let mut elapsed = Duration::ZERO;
    for (index, a) in nodes.iter().enumerate() {
        for b in nodes.iter().skip(index.saturating_add(1)) {
            elapsed += establish_connection(a, b, wait_timeout).await?;
        }
    }
    Ok(elapsed)
}

async fn establish_connection(
    a: &BenchNode,
    b: &BenchNode,
    wait_timeout: Duration,
) -> Result<Duration, BenchError> {
    let started = Instant::now();
    let offer = a.swarm.create_offer(b.did()).await?;
    let answer = b.swarm.answer_offer(offer).await?;
    a.swarm.accept_answer(answer).await?;
    wait_for_connected(a, b.did(), wait_timeout).await?;
    wait_for_connected(b, a.did(), wait_timeout).await?;
    Ok(started.elapsed())
}

async fn connect_joiners(
    nodes: &[BenchNode],
    active: &BTreeSet<usize>,
    joined: &BTreeSet<usize>,
    wait_timeout: Duration,
) -> Result<Duration, BenchError> {
    if joined.is_empty() {
        return Ok(Duration::ZERO);
    }
    let active_refs = node_refs(nodes, active);
    let Some(hub) = active_refs.first() else {
        return Err(BenchError::TooFewNodes);
    };
    let mut elapsed = Duration::ZERO;
    for joiner in node_refs(nodes, joined) {
        elapsed += establish_connection(hub, joiner, wait_timeout).await?;
    }
    Ok(elapsed)
}

async fn wait_for_connected(
    node: &BenchNode,
    peer: Did,
    wait_timeout: Duration,
) -> Result<(), BenchError> {
    let peer_id = peer.to_string();
    wait_until("WebRTC connection", wait_timeout, || {
        Ok(node
            .swarm
            .peers()
            .iter()
            .any(|info| info.did == peer_id && info.state == "Connected"))
    })
    .await
}

async fn wait_until(
    label: &str,
    wait_timeout: Duration,
    mut ready: impl FnMut() -> Result<bool, BenchError>,
) -> Result<(), BenchError> {
    let started = Instant::now();
    loop {
        if ready()? {
            return Ok(());
        }
        if started.elapsed() >= wait_timeout {
            return Err(timeout_error(label, wait_timeout));
        }
        sleep(WAIT_POLL_INTERVAL).await;
    }
}

async fn disconnect_positions(nodes: &[BenchNode], positions: &BTreeSet<usize>) -> Vec<String> {
    let failed = node_refs(nodes, positions);
    let mut errors = Vec::new();
    for failed_node in failed {
        for node in nodes {
            if node.did() == failed_node.did() {
                continue;
            }
            if let Err(error) = node.swarm.disconnect(failed_node.did()).await {
                errors.push(error.to_string());
            }
            if let Err(error) = failed_node.swarm.disconnect(node.did()).await {
                errors.push(error.to_string());
            }
        }
    }
    errors
}

fn initial_active_indices(node_count: usize) -> BTreeSet<usize> {
    (0..node_count).collect()
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

fn joiner_positions(node_count: usize, churn_count: usize) -> BTreeSet<usize> {
    (node_count..node_count.saturating_add(churn_count)).collect()
}

fn node_refs<'a>(nodes: &'a [BenchNode], positions: &BTreeSet<usize>) -> Vec<&'a BenchNode> {
    nodes
        .iter()
        .enumerate()
        .filter(|(index, _)| positions.contains(index))
        .map(|(_, node)| node)
        .collect()
}

fn connected_directed_edges(nodes: &[&BenchNode]) -> usize {
    nodes
        .iter()
        .map(|node| {
            node.swarm
                .peers()
                .iter()
                .filter(|peer| peer.state == "Connected")
                .count()
        })
        .sum()
}

fn expected_dhts(
    nodes: &[&BenchNode],
    finger_table_size: usize,
) -> Result<Vec<DHTInspect>, BenchError> {
    let mut expected = Vec::with_capacity(nodes.len());
    for node in nodes {
        let dht = PeerRing::new_with_storage_and_finger_table_size(
            node.did(),
            DHT_SUCCESSOR_CAPACITY,
            Box::new(MemStorage::new()),
            finger_table_size,
        );
        for other in nodes {
            if dht.did != other.did() {
                dht.join(other.did())?;
                dht.notify(other.did())?;
            }
        }
        expected.push(DHTInspect::inspect(&dht));
    }
    Ok(expected)
}

fn inspect_all(nodes: &[&BenchNode]) -> Vec<DHTInspect> {
    nodes
        .iter()
        .map(|node| DHTInspect::inspect(&node.swarm.dht()))
        .collect()
}

fn topology_matches(actual: &DHTInspect, expected: &DHTInspect) -> bool {
    actual.successors == expected.successors && actual.predecessor == expected.predecessor
}

fn lookup_metrics(
    nodes: &[&BenchNode],
    finger_table_size: usize,
) -> Result<LookupMetrics, BenchError> {
    let all_dids = nodes.iter().map(|node| node.did()).collect::<Vec<_>>();
    let index_by_did = all_dids
        .iter()
        .copied()
        .enumerate()
        .map(|(index, did)| (did, index))
        .collect::<BTreeMap<_, _>>();
    let probe_count = finger_table_size.clamp(1, DEFAULT_LOOKUP_TARGET_BITS);
    let mut metrics = LookupAccumulator::default();

    for (origin_index, origin) in nodes.iter().enumerate() {
        for bit in 0..probe_count {
            let target = origin.did() + Did::power_of_two(bit);
            if let Some(expected) = expected_successor(&all_dids, target) {
                let outcome = resolve_lookup(nodes, &index_by_did, origin_index, target)?;
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
    Resolved {
        did: Did,
        hops: usize,
        timeouts: usize,
    },
    Failed {
        timeouts: usize,
    },
}

fn resolve_lookup(
    nodes: &[&BenchNode],
    index_by_did: &BTreeMap<Did, usize>,
    origin_index: usize,
    target: Did,
) -> Result<LookupOutcome, BenchError> {
    let mut current_index = origin_index;
    let mut timeouts = 0usize;
    for hops in 1..=MAX_LOOKUP_HOPS {
        let current = nodes
            .get(current_index)
            .ok_or(BenchError::MissingNode(current_index))?;
        match current.swarm.dht().find_successor(target)? {
            PeerRingAction::Some(did) => {
                return Ok(LookupOutcome::Resolved {
                    did,
                    hops,
                    timeouts,
                })
            }
            PeerRingAction::RemoteAction(next, _) => {
                if next == current.did() {
                    return Ok(LookupOutcome::Failed { timeouts });
                }
                let Some(next_index) = index_by_did.get(&next).copied() else {
                    timeouts = timeouts.saturating_add(1);
                    return Ok(LookupOutcome::Failed { timeouts });
                };
                current_index = next_index;
            }
            _ => return Ok(LookupOutcome::Failed { timeouts }),
        }
    }
    Ok(LookupOutcome::Failed { timeouts })
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

impl LookupAccumulator {
    fn record(&mut self, outcome: LookupOutcome, expected: Did) {
        self.total = self.total.saturating_add(1);
        match outcome {
            LookupOutcome::Resolved {
                did,
                hops,
                timeouts,
            } => {
                self.resolved = self.resolved.saturating_add(1);
                self.total_hops = self.total_hops.saturating_add(hops);
                self.max_hops = self.max_hops.max(hops);
                self.timeouts = self.timeouts.saturating_add(timeouts);
                let bucket = self.hop_buckets.entry(hops).or_insert(0);
                *bucket = bucket.saturating_add(1);
                if did == expected {
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

fn timeout_error(label: &str, wait_timeout: Duration) -> BenchError {
    BenchError::Timeout {
        label: label.to_string(),
        timeout_ms: wait_timeout.as_millis() as usize,
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

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn mbps(bytes: usize, duration: Duration) -> f64 {
    let seconds = duration.as_secs_f64();
    if seconds == 0.0 {
        return 0.0;
    }
    bytes as f64 * 8.0 / seconds / 1_000_000.0
}
