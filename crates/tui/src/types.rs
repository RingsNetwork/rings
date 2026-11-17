//! Data transfer objects, combined node snapshots, and telemetry time-series buffers.

use std::collections::VecDeque;

use rings_rpc::protos::rings_node::NodeInfoResponse;
use rings_rpc::protos::rings_node::OnionExitDescriptorInfo;
use rings_rpc::protos::rings_node::OnlineNodeDescriptorInfo;
use rings_rpc::protos::rings_node::PeerInfo;
use rings_rpc::protos::rings_node::PeerMeasurementInfo;
use rings_rpc::protos::rings_node::PeerReliabilityInfo;
use rings_rpc::protos::rings_node::SwarmInfo;
use serde::Deserialize;
use serde::Serialize;
use sha1::Digest;
use sha1::Sha1;

/// Maximum number of telemetry samples kept in the sparkline ring buffer.
pub const TELEMETRY_HISTORY_CAPACITY: usize = 60;

/// Compact status response returned by `GET /status?view=observability`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ObservabilityStatusDto {
    /// Running Rings node version string.
    pub version: String,
    /// Structured operator observability snapshot.
    pub observability: OperatorSnapshotDto,
}

/// Complete JSON payload inside `observability` from `/status?view=observability`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OperatorSnapshotDto {
    /// Schema version of the observability export.
    pub schema_version: u16,
    /// Unix epoch millisecond when the snapshot was generated.
    pub generated_at_ms: u128,
    /// Unix epoch millisecond when the node process started.
    pub process_started_at_ms: u128,
    /// Scope label for message and lookup counters.
    pub counter_scope: String,
    /// Process-lifetime message totals.
    pub messages: MessageTotalsDto,
    /// Bounded recent message event ring (oldest-first from node).
    pub recent_messages: Vec<RecentMessageEventDto>,
    /// Active session delegation lifecycle state.
    pub session_key: SessionKeySnapshotDto,
    /// Aggregate relay-mailbox retention counts.
    pub mailboxes: MailboxSnapshotDto,
    /// Process-lifetime DHT lookup counters and latency histogram.
    pub dht_lookups: DhtLookupSnapshotDto,
    /// Bounded local peer reliability and credit ratings.
    pub peer_ratings: Vec<PeerRatingSnapshotDto>,
    /// Process API and overlay readiness propositions.
    pub health: HealthSnapshotDto,
}

/// Process-lifetime message counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTotalsDto {
    /// Locally originated messages accepted by next hop.
    pub sent: u64,
    /// Inbound messages accepted by this node.
    pub received: u64,
    /// Relayed messages accepted by next hop.
    pub forwarded: u64,
    /// Offline messages stored in a relay mailbox.
    pub stored: u64,
    /// Failed message operations across all actions.
    pub failed: u64,
}

/// One privacy-safe recent message event record.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentMessageEventDto {
    /// Monotonic process-local event sequence number.
    pub sequence: u64,
    /// Unix epoch millisecond when observed.
    pub observed_at_ms: u128,
    /// Activity: `sent`, `received`, `forwarded`, or `stored`.
    pub action: String,
    /// Traffic scheduling category (`dht_control`, `storage`, `e2e`, `application`).
    pub category: String,
    /// Protocol variant class name.
    pub message_class: String,
    /// Operation outcome (`succeeded` or `failed`).
    pub outcome: String,
}

/// Session delegation key lifecycle status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionKeySnapshotDto {
    /// Whether the active delegation is currently valid.
    pub valid: bool,
    /// Delegation creation timestamp in Unix epoch milliseconds.
    pub created_at_ms: u128,
    /// Delegation expiration timestamp in Unix epoch milliseconds.
    pub expires_at_ms: u128,
    /// Remaining lifetime in milliseconds.
    pub remaining_ms: u128,
    /// Successful runtime key rotations in this process lifetime.
    pub rotation_succeeded_total: u64,
    /// Failed runtime key rotations in this process lifetime.
    pub rotation_failed_total: u64,
    /// Whether in-process delegation renewal is configured.
    pub runtime_rotation_supported: bool,
}

impl SessionKeySnapshotDto {
    /// Compute the remaining delegation lifetime ratio in `[0.0, 1.0]`.
    pub fn remaining_ratio(&self) -> f64 {
        let total = self.expires_at_ms.saturating_sub(self.created_at_ms);
        if total == 0 {
            return if self.valid { 1.0 } else { 0.0 };
        }
        let clamped = self.remaining_ms.min(total);
        (clamped as f64 / total as f64).clamp(0.0, 1.0)
    }
}

/// Aggregate local relay-mailbox state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxSnapshotDto {
    /// Live relay-inbox carriers retained by this node.
    pub registered: u64,
    /// Live held messages across all retained inboxes.
    pub held_messages: u64,
    /// Cumulative holds during this process lifetime.
    pub stored_total: u64,
}

/// DHT lookup counters and cumulative latency distribution.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DhtLookupSnapshotDto {
    /// Total lookup rounds started.
    pub total: u64,
    /// Lookups that succeeded.
    pub succeeded: u64,
    /// Lookups that failed explicitly.
    pub failed: u64,
    /// Lookups that timed out.
    pub timed_out: u64,
    /// Lookups currently in flight.
    pub in_flight: u64,
    /// Sum of completed lookup latencies in milliseconds.
    pub latency_sum_ms: u128,
    /// Number of completed lookups in the latency distribution.
    pub latency_count: u64,
    /// Cumulative latency buckets (`le_ms`, `count`).
    pub latency_buckets: Vec<LatencyBucketDto>,
}

impl DhtLookupSnapshotDto {
    /// Average terminal lookup latency in milliseconds, or `0.0` when none have completed.
    pub fn avg_latency_ms(&self) -> f64 {
        if self.latency_count == 0 {
            0.0
        } else {
            self.latency_sum_ms as f64 / self.latency_count as f64
        }
    }

    /// Success rate in `[0.0, 100.0]` over terminal lookups.
    pub fn success_rate_pct(&self) -> f64 {
        let terminal = self
            .succeeded
            .saturating_add(self.failed)
            .saturating_add(self.timed_out);
        if terminal == 0 {
            100.0
        } else {
            (self.succeeded as f64 * 100.0) / (terminal as f64)
        }
    }

    /// Convert cumulative histogram buckets into non-cumulative per-bucket counts.
    pub fn non_cumulative_buckets(&self) -> Vec<(String, u64)> {
        let mut prev = 0_u64;
        let mut out = Vec::with_capacity(self.latency_buckets.len());
        for bucket in &self.latency_buckets {
            let delta = bucket.count.saturating_sub(prev);
            prev = bucket.count;
            let label = if bucket.le_ms >= 1_000 {
                format!("≤{}s", bucket.le_ms / 1_000)
            } else {
                format!("≤{}ms", bucket.le_ms)
            };
            out.push((label, delta));
        }
        out
    }
}

/// Cumulative latency histogram bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyBucketDto {
    /// Upper bound in milliseconds.
    pub le_ms: u64,
    /// Cumulative count of lookups with latency `<= le_ms`.
    pub count: u64,
}

/// Structured local peer rating from observability snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerRatingSnapshotDto {
    /// Peer DID.
    pub peer: String,
    /// Reliability class (`healthy`, `unknown`, `degraded`).
    pub reliability: String,
    /// Local credit score multiplier.
    pub credit_score: f64,
    /// Recent successful sends.
    pub sent: u64,
    /// Recent failed sends.
    pub failed_to_send: u64,
    /// Recent successful receives.
    pub received: u64,
    /// Recent failed receives.
    pub failed_to_receive: u64,
}

/// Process API and overlay readiness propositions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSnapshotDto {
    /// Whether the HTTP API is serving.
    pub process_api_healthy: bool,
    /// Count of admitted transport peers.
    pub admitted_peer_count: u64,
    /// Whether at least one admitted transport peer is connected.
    pub has_admitted_peer: bool,
    /// Whether the local DHT ring has a successor.
    pub has_successor: bool,
    /// Whether the local DHT ring has a predecessor.
    pub has_predecessor: bool,
    /// Conservative overall readiness indicator.
    pub overlay_ready: bool,
}

/// Gateway health snapshot returned by `GET /gateway/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayStatusDto {
    /// External health classification (`inactive`, `transitioning`, `active`, `degraded`, `failed`).
    pub health: String,
    /// Platform TUN interface name, when active.
    pub interface: Option<String>,
    /// Destination CIDR prefixes captured by the gateway.
    #[serde(default)]
    pub capture_routes: Vec<String>,
    /// Compatible onion exit availability (`unknown`, `available`, `unavailable`).
    pub exit_availability: String,
    /// Number of currently tracked TCP flows.
    pub active_flows: usize,
    /// Diagnostic reason when degraded or failed.
    pub reason: Option<String>,
    /// Most recent flow reset reason, if any.
    pub last_flow_reset: Option<serde_json::Value>,
}

/// Explicit probe outcome for `/gateway/status`, distinguishing "not mounted (404)" from
/// authentication or transport errors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum GatewayProbeState {
    /// Initial state before the first poll completes.
    #[default]
    Unknown,
    /// `/gateway/status` returned HTTP 404 (gateway is not mounted on this node).
    NotMounted,
    /// `/gateway/status` returned a valid snapshot.
    Available(GatewayStatusDto),
    /// `/gateway/status` failed with an HTTP or transport error.
    Failed(String),
}

/// Combined state gathered from a running `rings-node` during a refresh cycle.
#[derive(Clone, Debug, Default)]
pub struct NodeSnapshot {
    /// Local node DID.
    pub did: Option<String>,
    /// Local node wire protocol version.
    pub wire_version: Option<u16>,
    /// Local node software version.
    pub version: Option<String>,
    /// Full swarm, DHT, and storage inspection data.
    pub swarm: Option<SwarmInfo>,
    /// Connected peer list.
    pub peers: Vec<PeerInfo>,
    /// Per-peer transport measurement counters and byte credits.
    pub measurements: Vec<PeerMeasurementInfo>,
    /// Structured observability snapshot (`/status?view=observability`).
    pub observability: Option<OperatorSnapshotDto>,
    /// Probe status of the native TUN gateway (`/gateway/status`).
    pub gateway: GatewayProbeState,
    /// Discovered online-node directory entries.
    pub online_nodes: Vec<OnlineNodeDescriptorInfo>,
    /// Error from the most recent `lookupOnlineNodes` call, if it failed.
    pub online_nodes_error: Option<String>,
    /// Discovered onion-exit directory entries.
    pub onion_exits: Vec<OnionExitDescriptorInfo>,
    /// Error from the most recent `lookupOnionExits` call, if it failed.
    pub onion_exits_error: Option<String>,
    /// Whether this snapshot was queried with `include_expired = true`.
    pub include_expired_directory: bool,
    /// Non-fatal subquery warnings encountered while assembling this snapshot.
    pub warnings: Vec<String>,
    /// Unix epoch milliseconds when this snapshot was collected.
    pub fetched_at_ms: u128,
    /// Round-trip duration of the poll in milliseconds.
    pub poll_latency_ms: u64,
}

impl NodeSnapshot {
    /// Populate `version`, `swarm`, and `peers` from a [`NodeInfoResponse`].
    pub fn apply_node_info(&mut self, info: NodeInfoResponse) {
        self.version = Some(info.version);
        if let Some(swarm) = info.swarm {
            if self.peers.is_empty() {
                self.peers.clone_from(&swarm.peers);
            }
            if self.did.is_none() {
                self.did = swarm.dht.as_ref().map(|dht| dht.did.clone());
            }
            self.swarm = Some(swarm);
        }
    }

    /// Compute the node uptime in seconds from the observability snapshot, if available.
    pub fn uptime_secs(&self) -> Option<u64> {
        let obs = self.observability.as_ref()?;
        let elapsed_ms = obs
            .generated_at_ms
            .saturating_sub(obs.process_started_at_ms);
        u64::try_from(elapsed_ms / 1_000).ok()
    }

    /// Describe a peer's role in the local Chord DHT ring (`Succ #1`, `Pred`, `Finger`, or `Peer`).
    pub fn peer_chord_role(&self, peer_did: &str) -> String {
        let Some(dht) = self.swarm.as_ref().and_then(|s| s.dht.as_ref()) else {
            return "Peer".to_string();
        };
        let mut roles = Vec::new();
        if let Some(pos) = dht
            .successors
            .iter()
            .position(|d| d.eq_ignore_ascii_case(peer_did))
        {
            roles.push(format!("Succ#{}", pos + 1));
        }
        if dht
            .predecessor
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(peer_did))
        {
            roles.push("Pred".to_string());
        }
        if roles.is_empty() {
            if let Some(range) = dht.finger_table_ranges.iter().find(|r| {
                r.did
                    .as_deref()
                    .is_some_and(|d| d.eq_ignore_ascii_case(peer_did))
            }) {
                roles.push(format!("Finger[{}..{}]", range.start, range.end));
            }
        }
        if roles.is_empty() {
            "Direct Peer".to_string()
        } else {
            roles.join("+")
        }
    }

    /// Build a unified, source-merged list of peer traffic and measurement rows, combining
    /// connected peers (`listPeers`), local transport measurements (`listPeerMeasurements`),
    /// and observability peer ratings (`peer_ratings`).
    pub fn peer_traffic_rows(&self) -> Vec<PeerTrafficRow> {
        let local_did = self.did.as_deref().unwrap_or_default();
        let mut rows = Vec::new();

        for peer in &self.peers {
            let meas = self
                .measurements
                .iter()
                .find(|m| m.did.eq_ignore_ascii_case(&peer.did));
            let rating = self.observability.as_ref().and_then(|o| {
                o.peer_ratings
                    .iter()
                    .find(|r| r.peer.eq_ignore_ascii_case(&peer.did))
            });

            let reliability = meas
                .map(|m| match m.reliability {
                    PeerReliabilityInfo::Healthy => "healthy",
                    PeerReliabilityInfo::Unknown => "unknown",
                    PeerReliabilityInfo::Degraded => "degraded",
                })
                .or_else(|| rating.map(|r| r.reliability.as_str()))
                .unwrap_or("unknown")
                .to_string();

            let credit_score = meas
                .and_then(|m| m.credit.as_ref().map(|c| c.score))
                .or_else(|| rating.map(|r| r.credit_score));

            let has_measurement = meas.is_some() || rating.is_some();
            let (sent_ok, sent_fail, recv_ok, recv_fail) = match (meas, rating) {
                (Some(m), _) => (
                    m.counters.sent,
                    m.counters.failed_to_send,
                    m.counters.received,
                    m.counters.failed_to_receive,
                ),
                (None, Some(r)) => (r.sent, r.failed_to_send, r.received, r.failed_to_receive),
                (None, None) => (0, 0, 0, 0),
            };

            let (bytes_tx, bytes_rx) = meas
                .and_then(|m| {
                    m.credit
                        .as_ref()
                        .map(|c| (Some(c.bytes_sent_to_peer), Some(c.bytes_received_from_peer)))
                })
                .unwrap_or((None, None));

            rows.push(PeerTrafficRow {
                peer_did: peer.did.clone(),
                chord_role: self.peer_chord_role(&peer.did),
                transport_state: peer.state.clone(),
                reliability,
                credit_score,
                has_measurement,
                sent_ok,
                sent_fail,
                recv_ok,
                recv_fail,
                bytes_tx,
                bytes_rx,
            });
        }

        for m in &self.measurements {
            if m.did.eq_ignore_ascii_case(local_did)
                || rows.iter().any(|r| r.peer_did.eq_ignore_ascii_case(&m.did))
            {
                continue;
            }
            let reliability = match m.reliability {
                PeerReliabilityInfo::Healthy => "healthy",
                PeerReliabilityInfo::Unknown => "unknown",
                PeerReliabilityInfo::Degraded => "degraded",
            }
            .to_string();
            let (bytes_tx, bytes_rx) = m
                .credit
                .as_ref()
                .map(|c| (Some(c.bytes_sent_to_peer), Some(c.bytes_received_from_peer)))
                .unwrap_or((None, None));

            rows.push(PeerTrafficRow {
                peer_did: m.did.clone(),
                chord_role: "Retained".to_string(),
                transport_state: "Offline (Measured)".to_string(),
                reliability,
                credit_score: m.credit.as_ref().map(|c| c.score),
                has_measurement: true,
                sent_ok: m.counters.sent,
                sent_fail: m.counters.failed_to_send,
                recv_ok: m.counters.received,
                recv_fail: m.counters.failed_to_receive,
                bytes_tx,
                bytes_rx,
            });
        }

        rows
    }

    /// Return the peer DID at `index` in the exact same order as [`Self::peer_traffic_rows`],
    /// ensuring UI table selection and command modal pre-filling always target the same peer.
    pub fn peer_did_at(&self, index: usize) -> Option<&str> {
        if let Some(peer) = self.peers.get(index) {
            return Some(peer.did.as_str());
        }
        let local_did = self.did.as_deref().unwrap_or_default();
        let mut retained_idx = index.saturating_sub(self.peers.len());
        let mut seen_measurements: Vec<&str> = Vec::new();
        for m in &self.measurements {
            if m.did.eq_ignore_ascii_case(local_did)
                || self
                    .peers
                    .iter()
                    .any(|p| p.did.eq_ignore_ascii_case(&m.did))
                || seen_measurements
                    .iter()
                    .any(|prev| prev.eq_ignore_ascii_case(&m.did))
            {
                continue;
            }
            seen_measurements.push(m.did.as_str());
            if retained_idx == 0 {
                return Some(m.did.as_str());
            }
            retained_idx -= 1;
        }
        None
    }

    /// Find the row index of `target_did` in [`Self::peer_traffic_rows`].
    pub fn position_of_peer_did(&self, target_did: &str) -> Option<usize> {
        self.peer_traffic_rows()
            .iter()
            .position(|r| r.peer_did.eq_ignore_ascii_case(target_did))
    }

    /// List DHT VNode storage entries currently hosted in this node's persistent or cache storage.
    ///
    /// Note: `entry_did` is the 160-bit Chord ring key (`Entry.did`) of the stored virtual node,
    /// not a remote peer identity.
    pub fn hosted_vnode_rows(&self) -> Vec<HostedVNodeRow> {
        let mut rows = Vec::new();
        let Some(swarm) = &self.swarm else {
            return rows;
        };
        for (tier, st_opt) in [
            ("Persistent", swarm.persistence_storage.as_ref()),
            ("Cache", swarm.cache_storage.as_ref()),
        ] {
            let Some(st) = st_opt else {
                continue;
            };
            for item in &st.items {
                let (entry_did, kind, element_count) = match &item.value {
                    Some(v) => (v.did.clone(), v.kind.clone(), v.data.len()),
                    None => ("-".to_string(), "Empty".to_string(), 0),
                };
                rows.push(HostedVNodeRow {
                    storage_tier: tier.to_string(),
                    storage_key: item.key.clone(),
                    entry_did,
                    entry_kind: kind,
                    element_count,
                });
            }
        }
        rows
    }
}

/// Merged transport, Chord role, reliability, and byte-credit summary for one peer.
#[derive(Clone, Debug, PartialEq)]
pub struct PeerTrafficRow {
    /// Remote peer DID.
    pub peer_did: String,
    /// Role in the local Chord ring (`Succ#1`, `Pred`, `Finger[..]`, `Direct Peer`, or `Retained`).
    pub chord_role: String,
    /// Transport connection state (`Connected`, `Offline (Measured)`, etc.).
    pub transport_state: String,
    /// Local reliability class (`healthy`, `unknown`, `degraded`).
    pub reliability: String,
    /// Local resource-priority credit score, if measured.
    pub credit_score: Option<f64>,
    /// Whether transport counter measurements or ratings were loaded for this peer.
    pub has_measurement: bool,
    /// Successful sends to this peer.
    pub sent_ok: u64,
    /// Failed sends to this peer.
    pub sent_fail: u64,
    /// Successful receives from this peer.
    pub recv_ok: u64,
    /// Failed receives from this peer.
    pub recv_fail: u64,
    /// Verified payload bytes sent to this peer, when credit measurement is available.
    pub bytes_tx: Option<u64>,
    /// Verified payload bytes received from this peer, when credit measurement is available.
    pub bytes_rx: Option<u64>,
}

/// Summary row for a DHT VNode entry hosted in the local node's storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedVNodeRow {
    /// Storage backend tier (`Persistent` or `Cache`).
    pub storage_tier: String,
    /// Storage map key reported by `nodeInfo`.
    pub storage_key: String,
    /// 160-bit Chord ring key (`Entry.did`) of the stored entry.
    pub entry_did: String,
    /// Entry kind (`Data`, `RelayMessage`, `Append`, `Map`).
    pub entry_kind: String,
    /// Number of elements held in this entry.
    pub element_count: usize,
}

/// Classification of a user-supplied DHT lookup query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DhtQueryTarget {
    /// Human-readable topic or service name whose 160-bit Chord entry key is `SHA-1(topic)`.
    TopicKey {
        /// Original topic or service name string.
        topic: String,
        /// Derived 160-bit Chord ring entry key (`0x` + 40 lowercase hex chars).
        derived_entry_did: String,
    },
    /// Explicit 160-bit Chord ring entry key (`0x` + 40 hex chars).
    ///
    /// Because `fetchTopicMessages` hashes its topic parameter on the server, raw entry DIDs are
    /// inspected against local persistent/cache VNode storage without double-hashing over RPC.
    RawEntryDid {
        /// Canonical lowercase `0x` + 40 hex char entry key.
        entry_did: String,
    },
}

impl DhtQueryTarget {
    /// Parse a user query string into either a [`Self::RawEntryDid`] (when it is a 20-byte hex
    /// identifier) or a [`Self::TopicKey`] (deriving its SHA-1 Chord ring key).
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        let hex_part = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"));
        if let Some(hex) = hex_part {
            if hex.len() == 40 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Self::RawEntryDid {
                    entry_did: format!("0x{}", hex.to_ascii_lowercase()),
                };
            }
        }
        let mut hasher = Sha1::new();
        hasher.update(trimmed.as_bytes());
        let digest = hasher.finalize();
        let mut hex_out = String::with_capacity(42);
        hex_out.push_str("0x");
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(&mut hex_out, "{byte:02x}");
        }
        Self::TopicKey {
            topic: trimmed.to_string(),
            derived_entry_did: hex_out,
        }
    }

    /// Return the 160-bit Chord ring entry key (`0x...`) targeted by this query.
    pub fn target_entry_did(&self) -> &str {
        match self {
            Self::TopicKey {
                derived_entry_did, ..
            } => derived_entry_did.as_str(),
            Self::RawEntryDid { entry_did } => entry_did.as_str(),
        }
    }

    /// Describe the query mode in human-readable form.
    pub const fn mode_label(&self) -> &'static str {
        match self {
            Self::TopicKey { .. } => "Topic/Service Name (Live DHT Fetch + Local Storage)",
            Self::RawEntryDid { .. } => "Raw Entry DID (Local VNode Storage Inspection)",
        }
    }
}

/// Outcome status of a DHT key lookup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DhtLookupStatus {
    /// At least one element or VNode entry was found.
    Found,
    /// Queries succeeded, and no entry was present for the key.
    #[default]
    Absent,
    /// One sub-query failed while another succeeded; results may be incomplete.
    PartialError,
}

impl DhtLookupStatus {
    /// Short human-readable label for this lookup status.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Found => "Found",
            Self::Absent => "Absent (Empty)",
            Self::PartialError => "Partial Error",
        }
    }
}

/// Structured result of a Chord DHT key lookup operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DhtKeyLookupReport {
    /// Raw key string or DID queried by the user.
    pub query_key: String,
    /// Query mode description (`Topic/Service Name` vs `Raw Entry DID`).
    pub query_mode: String,
    /// Resolved 160-bit Chord ring target entry key (`0x...`).
    pub target_did: String,
    /// Lookup outcome classification.
    pub status: DhtLookupStatus,
    /// Authoritative sources where the entry was found (`Live DHT Topic Fetch`, `Persistent Storage`, `Cache Storage`).
    pub sources: Vec<String>,
    /// Entry kind (`Data`, `RelayMessage`, `Append`, `Map`, or `Absent`).
    pub record_kind: String,
    /// Decoded elements stored under this key.
    pub elements: Vec<String>,
    /// Diagnostic warning if a sub-query failed or raw DID mode skipped topic RPC.
    pub warning: Option<String>,
    /// Lookup latency in milliseconds.
    pub duration_ms: u64,
}

/// Resolve any key string (either a hex DID `0x...` or an arbitrary topic/service string) into
/// its canonical 160-bit Chord ring `Did` hex string (`0x...`).
pub fn resolve_dht_key_did(key: &str) -> String {
    DhtQueryTarget::parse(key).target_entry_did().to_string()
}

/// Decode a base64-encoded `StorageValue` element into a human-readable string (UTF-8 or hex).
pub fn decode_storage_element(b64: &str) -> String {
    let Ok(bytes) = base64::decode(b64) else {
        return format!("<base64: {b64}>");
    };
    if let Ok(utf8) = String::from_utf8(bytes.clone()) {
        return utf8;
    }
    let hex_preview = bytes
        .iter()
        .take(32)
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if bytes.len() > 32 {
        format!("0x{hex_preview}... ({} bytes)", bytes.len())
    } else {
        format!("0x{hex_preview} ({} bytes)", bytes.len())
    }
}

/// Rolling telemetry history for rendering sparklines in the Monitor view.
#[derive(Clone, Debug)]
pub struct TelemetryHistory {
    /// Sent message deltas per poll cycle.
    pub sent_deltas: VecDeque<u64>,
    /// Received message deltas per poll cycle.
    pub received_deltas: VecDeque<u64>,
    /// Forwarded message deltas per poll cycle.
    pub forwarded_deltas: VecDeque<u64>,
    /// Failed message deltas per poll cycle.
    pub failed_deltas: VecDeque<u64>,
    /// Connected peer count samples.
    pub peer_counts: VecDeque<u64>,
    /// Poll latency samples in milliseconds.
    pub poll_latencies_ms: VecDeque<u64>,
    /// Node DID associated with the current baseline.
    tracked_did: Option<String>,
    /// Process start timestamp associated with the current baseline.
    tracked_process_started_at_ms: Option<u128>,
    /// Monotonic `generated_at_ms` of the latest recorded observability snapshot.
    last_generated_at_ms: Option<u128>,
    /// Previous cumulative message totals used to compute per-interval deltas.
    last_totals: Option<MessageTotalsDto>,
}

impl Default for TelemetryHistory {
    fn default() -> Self {
        Self {
            sent_deltas: VecDeque::with_capacity(TELEMETRY_HISTORY_CAPACITY),
            received_deltas: VecDeque::with_capacity(TELEMETRY_HISTORY_CAPACITY),
            forwarded_deltas: VecDeque::with_capacity(TELEMETRY_HISTORY_CAPACITY),
            failed_deltas: VecDeque::with_capacity(TELEMETRY_HISTORY_CAPACITY),
            peer_counts: VecDeque::with_capacity(TELEMETRY_HISTORY_CAPACITY),
            poll_latencies_ms: VecDeque::with_capacity(TELEMETRY_HISTORY_CAPACITY),
            tracked_did: None,
            tracked_process_started_at_ms: None,
            last_generated_at_ms: None,
            last_totals: None,
        }
    }
}

impl TelemetryHistory {
    /// Reset the counter baseline when switching nodes or when a node process restarts.
    pub fn reset_baseline(&mut self) {
        self.last_totals = None;
        self.last_generated_at_ms = None;
    }

    /// Record a new [`NodeSnapshot`] into the rolling time series, ignoring out-of-order samples
    /// and resetting the counter baseline if the node identity or process start timestamp changed.
    pub fn record(&mut self, snapshot: &NodeSnapshot) {
        if snapshot.did.is_some() && self.tracked_did.is_some() && snapshot.did != self.tracked_did
        {
            self.reset_baseline();
        }
        if snapshot.did.is_some() {
            self.tracked_did.clone_from(&snapshot.did);
        }

        if let Some(obs) = &snapshot.observability {
            if self
                .tracked_process_started_at_ms
                .is_some_and(|prev_start| prev_start != obs.process_started_at_ms)
            {
                self.reset_baseline();
            }
            self.tracked_process_started_at_ms = Some(obs.process_started_at_ms);

            if self
                .last_generated_at_ms
                .is_some_and(|prev_gen| obs.generated_at_ms < prev_gen)
            {
                // Ignore stale out-of-order observability sample.
                return;
            }
            self.last_generated_at_ms = Some(obs.generated_at_ms);

            let current = obs.messages;
            let (sent_d, recv_d, fwd_d, fail_d) = match self.last_totals {
                Some(prev) => (
                    current.sent.saturating_sub(prev.sent),
                    current.received.saturating_sub(prev.received),
                    current.forwarded.saturating_sub(prev.forwarded),
                    current.failed.saturating_sub(prev.failed),
                ),
                None => (0, 0, 0, 0),
            };
            self.last_totals = Some(current);
            push_bounded(&mut self.sent_deltas, sent_d);
            push_bounded(&mut self.received_deltas, recv_d);
            push_bounded(&mut self.forwarded_deltas, fwd_d);
            push_bounded(&mut self.failed_deltas, fail_d);
        }

        let peer_count = snapshot
            .observability
            .as_ref()
            .map(|o| o.health.admitted_peer_count)
            .unwrap_or_else(|| u64::try_from(snapshot.peers.len()).unwrap_or(0));
        push_bounded(&mut self.peer_counts, peer_count);
        push_bounded(&mut self.poll_latencies_ms, snapshot.poll_latency_ms);
    }

    /// Return a `Vec<u64>` slice copy suitable for `ratatui::widgets::Sparkline`.
    pub fn sent_series(&self) -> Vec<u64> {
        self.sent_deltas.iter().copied().collect()
    }

    /// Return received deltas as a `Vec<u64>` for `Sparkline`.
    pub fn received_series(&self) -> Vec<u64> {
        self.received_deltas.iter().copied().collect()
    }

    /// Return forwarded deltas as a `Vec<u64>` for `Sparkline`.
    pub fn forwarded_series(&self) -> Vec<u64> {
        self.forwarded_deltas.iter().copied().collect()
    }

    /// Return failed deltas as a `Vec<u64>` for `Sparkline`.
    pub fn failed_series(&self) -> Vec<u64> {
        self.failed_deltas.iter().copied().collect()
    }
}

fn push_bounded(queue: &mut VecDeque<u64>, value: u64) {
    if queue.len() >= TELEMETRY_HISTORY_CAPACITY {
        queue.pop_front();
    }
    queue.push_back(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dht_query_target_distinguishes_raw_entry_did_from_topic_name() {
        let raw = DhtQueryTarget::parse("0x32b622ddf0f94698ac584873f52d05daebc39ee3");
        assert_eq!(raw, DhtQueryTarget::RawEntryDid {
            entry_did: "0x32b622ddf0f94698ac584873f52d05daebc39ee3".to_string(),
        });

        let topic = DhtQueryTarget::parse("rings-chat");
        assert_eq!(topic, DhtQueryTarget::TopicKey {
            topic: "rings-chat".to_string(),
            derived_entry_did: "0x32b622ddf0f94698ac584873f52d05daebc39ee3".to_string(),
        });
    }

    #[test]
    fn session_key_ratio_and_lookup_buckets_are_well_behaved() {
        let key = SessionKeySnapshotDto {
            valid: true,
            created_at_ms: 1_000,
            expires_at_ms: 11_000,
            remaining_ms: 5_000,
            rotation_succeeded_total: 1,
            rotation_failed_total: 0,
            runtime_rotation_supported: true,
        };
        assert!((key.remaining_ratio() - 0.5).abs() < 1e-6);

        let lookups = DhtLookupSnapshotDto {
            total: 10,
            succeeded: 8,
            failed: 1,
            timed_out: 1,
            in_flight: 0,
            latency_sum_ms: 500,
            latency_count: 10,
            latency_buckets: vec![
                LatencyBucketDto {
                    le_ms: 10,
                    count: 3,
                },
                LatencyBucketDto {
                    le_ms: 50,
                    count: 7,
                },
                LatencyBucketDto {
                    le_ms: 1_000,
                    count: 10,
                },
            ],
        };
        assert!((lookups.avg_latency_ms() - 50.0).abs() < 1e-6);
        assert!((lookups.success_rate_pct() - 80.0).abs() < 1e-6);
        let non_cum = lookups.non_cumulative_buckets();
        assert_eq!(non_cum, vec![
            ("≤10ms".to_string(), 3),
            ("≤50ms".to_string(), 4),
            ("≤1s".to_string(), 3),
        ]);
    }

    #[test]
    fn telemetry_history_ignores_out_of_order_samples_and_resets_on_restart() {
        let mut history = TelemetryHistory::default();
        for idx in 1..=5_u64 {
            let mut snap = NodeSnapshot {
                did: Some("0x1111".to_string()),
                ..NodeSnapshot::default()
            };
            let mut obs = OperatorSnapshotDto {
                process_started_at_ms: 100,
                generated_at_ms: u128::from(idx * 1_000),
                ..OperatorSnapshotDto::default()
            };
            obs.messages.sent = idx * 10;
            snap.observability = Some(obs);
            history.record(&snap);
        }
        assert_eq!(history.sent_deltas.back().copied(), Some(10));

        // Out-of-order older snapshot (generated_at_ms = 2_000 < 5_000) must be ignored.
        let mut stale = NodeSnapshot {
            did: Some("0x1111".to_string()),
            ..NodeSnapshot::default()
        };
        stale.observability = Some(OperatorSnapshotDto {
            process_started_at_ms: 100,
            generated_at_ms: 2_000,
            messages: MessageTotalsDto {
                sent: 20,
                ..MessageTotalsDto::default()
            },
            ..OperatorSnapshotDto::default()
        });
        history.record(&stale);
        assert_eq!(history.sent_deltas.len(), 5);

        // Next in-order snapshot (generated_at_ms = 6_000, sent = 60) records delta = 10 (not 40!).
        let mut next = NodeSnapshot {
            did: Some("0x1111".to_string()),
            ..NodeSnapshot::default()
        };
        next.observability = Some(OperatorSnapshotDto {
            process_started_at_ms: 100,
            generated_at_ms: 6_000,
            messages: MessageTotalsDto {
                sent: 60,
                ..MessageTotalsDto::default()
            },
            ..OperatorSnapshotDto::default()
        });
        history.record(&next);
        assert_eq!(history.sent_deltas.back().copied(), Some(10));

        // Node process restart (`process_started_at_ms` changes from 100 to 7_000) resets baseline:
        let mut restarted = NodeSnapshot {
            did: Some("0x1111".to_string()),
            ..NodeSnapshot::default()
        };
        restarted.observability = Some(OperatorSnapshotDto {
            process_started_at_ms: 7_000,
            generated_at_ms: 7_500,
            messages: MessageTotalsDto {
                sent: 5,
                ..MessageTotalsDto::default()
            },
            ..OperatorSnapshotDto::default()
        });
        history.record(&restarted);
        assert_eq!(history.sent_deltas.back().copied(), Some(0));

        let mut after_restart = NodeSnapshot {
            did: Some("0x1111".to_string()),
            ..NodeSnapshot::default()
        };
        after_restart.observability = Some(OperatorSnapshotDto {
            process_started_at_ms: 7_000,
            generated_at_ms: 8_500,
            messages: MessageTotalsDto {
                sent: 12,
                ..MessageTotalsDto::default()
            },
            ..OperatorSnapshotDto::default()
        });
        history.record(&after_restart);
        assert_eq!(history.sent_deltas.back().copied(), Some(7));
    }

    #[test]
    fn peer_did_at_matches_peer_traffic_rows_order_with_deduped_measurements() {
        use rings_rpc::protos::rings_node::PeerMeasurementCountersInfo;

        let snap = NodeSnapshot {
            did: Some("0xlocal".to_string()),
            peers: vec![PeerInfo {
                did: "0xpeerA".to_string(),
                state: "Connected".to_string(),
            }],
            measurements: vec![
                PeerMeasurementInfo {
                    did: "0xpeerB".to_string(),
                    counters: PeerMeasurementCountersInfo::default(),
                    credit: None,
                    reliability: PeerReliabilityInfo::Healthy,
                },
                PeerMeasurementInfo {
                    did: "0xpeerA".to_string(),
                    counters: PeerMeasurementCountersInfo::default(),
                    credit: None,
                    reliability: PeerReliabilityInfo::Healthy,
                },
                PeerMeasurementInfo {
                    did: "0xpeerC".to_string(),
                    counters: PeerMeasurementCountersInfo::default(),
                    credit: None,
                    reliability: PeerReliabilityInfo::Unknown,
                },
            ],
            ..NodeSnapshot::default()
        };

        let rows = snap.peer_traffic_rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].peer_did, "0xpeerA");
        assert_eq!(rows[1].peer_did, "0xpeerB");
        assert_eq!(rows[2].peer_did, "0xpeerC");

        for (idx, row) in rows.iter().enumerate() {
            assert_eq!(snap.peer_did_at(idx), Some(row.peer_did.as_str()));
        }
        assert_eq!(snap.peer_did_at(3), None);
    }
}
