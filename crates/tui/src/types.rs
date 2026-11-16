//! Data transfer objects, combined node snapshots, and telemetry time-series buffers.

use std::collections::VecDeque;
use std::str::FromStr;

use rings_core::dht::entry::Entry;
use rings_core::dht::Did;
use rings_rpc::protos::rings_node::NodeInfoResponse;
use rings_rpc::protos::rings_node::OnionExitDescriptorInfo;
use rings_rpc::protos::rings_node::OnlineNodeDescriptorInfo;
use rings_rpc::protos::rings_node::PeerInfo;
use rings_rpc::protos::rings_node::PeerMeasurementInfo;
use rings_rpc::protos::rings_node::SwarmInfo;
use serde::Deserialize;
use serde::Serialize;

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
    /// Optional native TUN gateway status (`/gateway/status`).
    pub gateway: Option<GatewayStatusDto>,
    /// Discovered online-node directory entries.
    pub online_nodes: Vec<OnlineNodeDescriptorInfo>,
    /// Discovered onion-exit directory entries.
    pub onion_exits: Vec<OnionExitDescriptorInfo>,
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
        if let Some(pos) = dht.successors.iter().position(|d| d == peer_did) {
            roles.push(format!("Succ#{}", pos + 1));
        }
        if dht.predecessor.as_deref() == Some(peer_did) {
            roles.push("Pred".to_string());
        }
        if roles.is_empty() {
            if let Some(range) = dht
                .finger_table_ranges
                .iter()
                .find(|r| r.did.as_deref() == Some(peer_did))
            {
                roles.push(format!("Finger[{}..{}]", range.start, range.end));
            }
        }
        if roles.is_empty() {
            "Direct Peer".to_string()
        } else {
            roles.join("+")
        }
    }

    /// Identify all nodes currently using this node's services (relay forwarding, onion circuit
    /// relay/exit, DHT VNode storage, mailbox custody, and registered services).
    pub fn service_consumers(
        &self,
        advertise_relay: bool,
        advertise_exit: bool,
        registered_services: &[String],
    ) -> Vec<ServiceConsumerRow> {
        let mut rows = Vec::new();
        let local_did = self.did.as_deref().unwrap_or_default();

        // 1. Peers with active inbound/outbound traffic or byte credits in measurements.
        for m in &self.measurements {
            if m.did == local_did {
                continue;
            }
            let is_connected = self.peers.iter().any(|p| p.did == m.did);
            let rx_bytes = m
                .credit
                .as_ref()
                .map(|c| c.bytes_received_from_peer)
                .unwrap_or(0);
            let tx_bytes = m.credit.as_ref().map(|c| c.bytes_sent_to_peer).unwrap_or(0);

            if m.counters.received == 0 && m.counters.sent == 0 && rx_bytes == 0 && tx_bytes == 0 {
                continue;
            }

            let mut services_used = Vec::new();
            if advertise_exit && rx_bytes > 0 {
                services_used.push("OnionExit".to_string());
            } else if advertise_relay && (m.counters.received > 0 || rx_bytes > 0) {
                services_used.push("OnionRelay".to_string());
            }
            if m.counters.received > 0 {
                services_used.push("DHT/MsgRelay".to_string());
            }
            if let Some(first_svc) = registered_services.first() {
                services_used.push(format!("Svc:{first_svc}"));
            }
            if services_used.is_empty() {
                services_used.push("Transport".to_string());
            }

            rows.push(ServiceConsumerRow {
                consumer_did: m.did.clone(),
                status: if is_connected {
                    "Active (Connected)".to_string()
                } else {
                    "Retained (Recent)".to_string()
                },
                services_used: services_used.join(", "),
                requests_in: m.counters.received,
                responses_out: m.counters.sent,
                bytes_rx: rx_bytes,
                bytes_tx: tx_bytes,
                credit_score: m.credit.as_ref().map(|c| c.score).unwrap_or(1.0),
            });
        }

        // 2. Storage/VNode consumers whose entries are hosted in our persistent or cache storage.
        if let Some(swarm) = &self.swarm {
            for (tier, st_opt) in [
                ("VNodeStore", swarm.persistence_storage.as_ref()),
                ("DHTCache", swarm.cache_storage.as_ref()),
            ] {
                if let Some(st) = st_opt {
                    for item in &st.items {
                        if let Some(val) = &item.value {
                            if val.did != local_did
                                && !rows.iter().any(|r| r.consumer_did == val.did)
                            {
                                rows.push(ServiceConsumerRow {
                                    consumer_did: val.did.clone(),
                                    status: format!("Hosted ({tier})"),
                                    services_used: format!("{tier}:{} ({})", val.kind, item.key),
                                    requests_in: val.data.len() as u64,
                                    responses_out: 0,
                                    bytes_rx: 0,
                                    bytes_tx: 0,
                                    credit_score: 1.0,
                                });
                            }
                        }
                    }
                }
            }
        }

        rows
    }
}

/// Summary row describing a remote peer or client consuming services provided by this node.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceConsumerRow {
    /// DID of the peer/client using our node's services.
    pub consumer_did: String,
    /// Connection or hosting status (`Active (Connected)`, `Retained`, `Hosted`).
    pub status: String,
    /// Services consumed by this peer (`OnionRelay`, `OnionExit`, `DHT/MsgRelay`, `VNodeStore`, etc.).
    pub services_used: String,
    /// Inbound messages/requests received from this peer.
    pub requests_in: u64,
    /// Outbound messages/responses sent to this peer.
    pub responses_out: u64,
    /// Verified payload bytes received from this peer.
    pub bytes_rx: u64,
    /// Payload bytes served/sent to this peer.
    pub bytes_tx: u64,
    /// Local credit score multiplier.
    pub credit_score: f64,
}

/// Structured result of a Chord DHT key lookup operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DhtKeyLookupReport {
    /// Raw key string or DID queried by the user.
    pub query_key: String,
    /// Resolved 160-bit Chord ring target DID (`0x...`).
    pub target_did: String,
    /// Next-hop or responsible peer DID from the local Chord ring, if known.
    pub route_peer: Option<String>,
    /// Where the key was found (`Live DHT Fetch`, `Persistent Storage`, `Cache Storage`).
    pub sources: Vec<String>,
    /// Entry kind (`Data`, `Append`, `Map`, or `None`).
    pub record_kind: String,
    /// Decoded elements stored under this key.
    pub elements: Vec<String>,
    /// Lookup latency in milliseconds.
    pub duration_ms: u64,
}

/// Resolve any key string (either a hex DID `0x...` or an arbitrary topic/service string) into
/// its canonical 160-bit Chord ring `Did` hex string (`0x...`).
pub fn resolve_dht_key_did(key: &str) -> String {
    let trimmed = key.trim();
    if let Ok(did) = Did::from_str(trimmed) {
        return did.to_string();
    }
    match Entry::gen_did(trimmed) {
        Ok(did) => did.to_string(),
        Err(_) => trimmed.to_string(),
    }
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
            last_totals: None,
        }
    }
}

impl TelemetryHistory {
    /// Record a new [`NodeSnapshot`] into the rolling time series.
    pub fn record(&mut self, snapshot: &NodeSnapshot) {
        let peer_count = snapshot
            .observability
            .as_ref()
            .map(|o| o.health.admitted_peer_count)
            .unwrap_or_else(|| u64::try_from(snapshot.peers.len()).unwrap_or(0));
        push_bounded(&mut self.peer_counts, peer_count);
        push_bounded(&mut self.poll_latencies_ms, snapshot.poll_latency_ms);

        if let Some(obs) = &snapshot.observability {
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
    fn telemetry_history_computes_deltas_and_respects_capacity() {
        let mut history = TelemetryHistory::default();
        for idx in 0..70_u64 {
            let mut snap = NodeSnapshot::default();
            let mut obs = OperatorSnapshotDto::default();
            obs.messages.sent = idx * 5;
            obs.messages.received = idx * 3;
            obs.health.admitted_peer_count = 4;
            snap.observability = Some(obs);
            history.record(&snap);
        }
        assert_eq!(history.sent_deltas.len(), TELEMETRY_HISTORY_CAPACITY);
        assert_eq!(history.sent_deltas.back().copied(), Some(5));
        assert_eq!(history.received_deltas.back().copied(), Some(3));
    }
}
