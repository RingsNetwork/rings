//! Versioned, bounded operator observability for a running node.
//!
//! [`Observability`] is the synchronous adapter installed at the core swarm boundary. It keeps
//! process-lifetime counters, a fixed-size recent-event ring, and bounded DHT lookup correlation
//! state. The native HTTP layer combines that state with live session, mailbox, peer-rating, and
//! overlay snapshots before returning JSON or Prometheus text. No payload, key material, message
//! identifier, mailbox identifier, or lookup correlation key crosses the export boundary.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::MutexGuard;

use rings_core::message::MessageCategory;
use rings_core::swarm::observer::LookupCorrelation;
use rings_core::swarm::observer::LookupKind;
use rings_core::swarm::observer::LookupOutcome;
use rings_core::swarm::observer::MessageActivity;
use rings_core::swarm::observer::MessageObservation;
use rings_core::swarm::observer::ObservationOutcome;
use rings_core::swarm::observer::SwarmObserver;
use serde::Serialize;

/// Version of the operator JSON and Prometheus schema.
pub const OPERATOR_SCHEMA_VERSION: u16 = 1;
/// Stable versioned path of the structured operator snapshot.
pub const OPERATOR_JSON_PATH: &str = "/operator/v1/observability";
/// Stable versioned path of the Prometheus-compatible scrape response.
pub const OPERATOR_METRICS_PATH: &str = "/operator/v1/metrics";
/// Maximum number of recent message records retained in process memory.
pub const RECENT_MESSAGE_CAPACITY: usize = 256;
/// Maximum number of peer-rating records returned by one structured snapshot.
pub const PEER_RATING_CAPACITY: usize = 128;
/// Maximum number of lookup correlations retained until completion or timeout.
const LOOKUP_CAPACITY: usize = 1024;
/// Lookup age at which a scrape classifies an unanswered operation as timed out.
const LOOKUP_TIMEOUT_MS: u128 = 30_000;
/// Finite Prometheus histogram boundaries for local DHT lookup latency.
const LOOKUP_LATENCY_BUCKETS_MS: [u64; 12] = [
    5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000,
];

/// Process-local observability recorder shared by the processor and core swarm.
pub struct Observability {
    /// Unix epoch millisecond at which this recorder was constructed.
    process_started_at_ms: u128,
    /// Complete bounded state protected for one short synchronous transition.
    state: Mutex<RuntimeState>,
}

/// Mutable state behind [`Observability`].
struct RuntimeState {
    /// Monotonic sequence assigned to bounded recent message records.
    next_event_sequence: u64,
    /// Counters indexed by activity, traffic category, then outcome.
    message_counts: [[[u64; 2]; 4]; 4],
    /// Oldest-first bounded message record ring.
    recent_messages: VecDeque<RecentMessageEvent>,
    /// Correlated lookup starts, never exported with their keys.
    lookups: BTreeMap<(LookupKind, LookupCorrelation), LookupStart>,
    /// Cumulative process-lifetime lookup totals and latency buckets.
    lookup_totals: LookupTotals,
}

/// Internal start witness for one in-flight lookup.
#[derive(Clone, Copy)]
struct LookupStart {
    /// Epoch millisecond of the local start observation.
    started_at_ms: u128,
}

/// Mutable cumulative DHT lookup counters.
#[derive(Clone, Default)]
struct LookupTotals {
    /// Lookup rounds begun during this process lifetime.
    started: u64,
    /// Lookup rounds that accepted an answer.
    succeeded: u64,
    /// Lookup rounds that failed before an answer.
    failed: u64,
    /// Lookup rounds classified as timed out by bounded retention.
    timed_out: u64,
    /// Sum of completed lookup latencies in milliseconds.
    latency_sum_ms: u128,
    /// Number of terminal lookups represented in the latency histogram.
    latency_count: u64,
    /// Cumulative histogram buckets in the order of [`LOOKUP_LATENCY_BUCKETS_MS`].
    latency_buckets: [u64; LOOKUP_LATENCY_BUCKETS_MS.len()],
}

/// One privacy-safe recent message record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecentMessageEvent {
    /// Process-local order, useful for polling without a message identifier.
    pub sequence: u64,
    /// Unix epoch millisecond at which the operation completed locally.
    pub observed_at_ms: u128,
    /// `sent`, `received`, `forwarded`, or `stored`.
    pub action: &'static str,
    /// Finite scheduling category such as `application` or `dht_control`.
    pub category: &'static str,
    /// Compile-time protocol variant name; never an application topic or payload value.
    pub message_class: &'static str,
    /// `succeeded` or `failed`.
    pub outcome: &'static str,
}

/// Process-lifetime message counters with node-local semantics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MessageTotals {
    /// Successful locally originated operations accepted by the next hop.
    pub sent: u64,
    /// Successful logical inbound operations accepted by this node.
    pub received: u64,
    /// Successful relays accepted by their next hop.
    pub forwarded: u64,
    /// Successful offline-message holds written to a relay inbox.
    pub stored: u64,
    /// Failed operations across every action above.
    pub failed: u64,
}

/// Public DHT lookup counters and latency distribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DhtLookupSnapshot {
    /// Lookup rounds begun during this process lifetime.
    pub total: u64,
    /// Lookup rounds that accepted a local or authenticated remote answer.
    pub succeeded: u64,
    /// Lookup rounds that terminated with an explicit local failure.
    pub failed: u64,
    /// Lookup rounds retained past the thirty-second observation bound.
    pub timed_out: u64,
    /// Lookup rounds still awaiting an answer.
    pub in_flight: u64,
    /// Sum of terminal lookup latency in milliseconds.
    pub latency_sum_ms: u128,
    /// Number of terminal lookups in the latency distribution.
    pub latency_count: u64,
    /// Cumulative latency buckets suitable for Prometheus export.
    pub latency_buckets: Vec<LatencyBucket>,
}

/// One cumulative DHT latency histogram bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LatencyBucket {
    /// Inclusive upper bound in milliseconds.
    pub le_ms: u64,
    /// Number of completed lookups at or below the bound.
    pub count: u64,
}

/// Active session-key lifecycle without key material or key-derived identifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SessionKeySnapshot {
    /// Whether the active delegation is valid at snapshot time.
    pub valid: bool,
    /// Delegation creation time in Unix epoch milliseconds.
    pub created_at_ms: u128,
    /// Delegation expiry in Unix epoch milliseconds.
    pub expires_at_ms: u128,
    /// Saturating time until expiry in milliseconds.
    pub remaining_ms: u128,
    /// Successful runtime rotations; currently zero because rotation requires restart.
    pub rotation_succeeded_total: u64,
    /// Failed runtime rotations; currently zero because rotation requires restart.
    pub rotation_failed_total: u64,
    /// Whether this process can replace its signing delegation without restart.
    pub runtime_rotation_supported: bool,
}

/// Aggregate live relay-mailbox state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MailboxSnapshot {
    /// Live relay-inbox carriers retained by this node.
    pub registered: u64,
    /// Live held messages across all retained inboxes.
    pub held_messages: u64,
    /// Successful holds during this process lifetime.
    pub stored_total: u64,
}

/// Bounded structured local assessment of one peer.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PeerRatingSnapshot {
    /// Public peer DID, present only in this bounded authenticated JSON response.
    pub peer: String,
    /// Advisory `healthy`, `unknown`, or `degraded` local reliability class.
    pub reliability: &'static str,
    /// Local resource-priority multiplier, not a network-wide reputation score.
    pub credit_score: f64,
    /// Successful recent sends in the active reliability epoch.
    pub sent: u64,
    /// Failed recent sends in the active reliability epoch.
    pub failed_to_send: u64,
    /// Successful recent receives in the active reliability epoch.
    pub received: u64,
    /// Failed recent receives in the active reliability epoch.
    pub failed_to_receive: u64,
}

/// Explicit separation of HTTP-process liveness and overlay readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct HealthSnapshot {
    /// True because producing the response proves the authenticated API task is serving.
    pub process_api_healthy: bool,
    /// Number of active routable transport peers in the current snapshot.
    pub admitted_peer_count: u64,
    /// Whether the node currently has at least one admitted transport peer.
    pub has_admitted_peer: bool,
    /// Whether the local DHT currently has a successor.
    pub has_successor: bool,
    /// Whether the local DHT currently has a predecessor.
    pub has_predecessor: bool,
    /// Conservative participation signal: an admitted peer and successor are both present.
    pub overlay_ready: bool,
}

/// Complete JSON response returned by the v1 operator endpoint.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OperatorSnapshot {
    /// Schema version for compatibility-sensitive consumers.
    pub schema_version: u16,
    /// Unix epoch millisecond at which this snapshot was assembled.
    pub generated_at_ms: u128,
    /// Unix epoch millisecond at which process-local counters began.
    pub process_started_at_ms: u128,
    /// Explicit scope of message and lookup counters.
    pub counter_scope: &'static str,
    /// Process-lifetime node-local message totals.
    pub messages: MessageTotals,
    /// Oldest-first bounded recent message activity.
    pub recent_messages: Vec<RecentMessageEvent>,
    /// Session delegation lifecycle.
    pub session_key: SessionKeySnapshot,
    /// Aggregate local relay-mailbox state.
    pub mailboxes: MailboxSnapshot,
    /// Process-lifetime DHT lookup outcomes and latency.
    pub dht_lookups: DhtLookupSnapshot,
    /// Bounded local peer assessments.
    pub peer_ratings: Vec<PeerRatingSnapshot>,
    /// Process/API and overlay health as separate propositions.
    pub health: HealthSnapshot,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            next_event_sequence: 0,
            message_counts: [[[0; 2]; 4]; 4],
            recent_messages: VecDeque::with_capacity(RECENT_MESSAGE_CAPACITY),
            lookups: BTreeMap::new(),
            lookup_totals: LookupTotals::default(),
        }
    }
}

impl Observability {
    /// Construct an empty process-local recorder at the current wall-clock instant.
    pub fn new() -> Self {
        Self {
            process_started_at_ms: now_ms(),
            state: Mutex::new(RuntimeState::default()),
        }
    }

    /// Return the beginning of the process-local counter scope.
    pub const fn process_started_at_ms(&self) -> u128 {
        self.process_started_at_ms
    }

    /// Produce message and lookup state, expiring stale correlations as timeouts first.
    pub fn runtime_snapshot(&self, observed_at_ms: u128) -> RuntimeSnapshot {
        let mut state = lock_or_recover(&self.state);
        expire_lookups(&mut state, observed_at_ms);
        RuntimeSnapshot {
            messages: message_totals(&state.message_counts),
            recent_messages: state.recent_messages.iter().cloned().collect(),
            dht_lookups: lookup_snapshot(&state),
        }
    }
}

impl Default for Observability {
    fn default() -> Self {
        Self::new()
    }
}

/// Recorder-owned part of an operator snapshot.
pub struct RuntimeSnapshot {
    /// Process-lifetime message totals.
    pub messages: MessageTotals,
    /// Bounded recent message records.
    pub recent_messages: Vec<RecentMessageEvent>,
    /// Bounded lookup state and cumulative outcomes.
    pub dht_lookups: DhtLookupSnapshot,
}

impl SwarmObserver for Observability {
    fn observe_message(&self, observation: MessageObservation) {
        let observed_at_ms = now_ms();
        let mut state = lock_or_recover(&self.state);
        let activity = activity_index(observation.activity);
        let category = category_index(observation.category);
        let outcome = outcome_index(observation.outcome);
        if let Some(count) = state
            .message_counts
            .get_mut(activity)
            .and_then(|categories| categories.get_mut(category))
            .and_then(|outcomes| outcomes.get_mut(outcome))
        {
            *count = count.saturating_add(1);
        }
        let sequence = state.next_event_sequence;
        state.next_event_sequence = state.next_event_sequence.saturating_add(1);
        if state.recent_messages.len() == RECENT_MESSAGE_CAPACITY {
            state.recent_messages.pop_front();
        }
        state.recent_messages.push_back(RecentMessageEvent {
            sequence,
            observed_at_ms,
            action: activity_name(observation.activity),
            category: category_name(observation.category),
            message_class: observation.message_class,
            outcome: outcome_name(observation.outcome),
        });
    }

    fn lookup_started(&self, kind: LookupKind, correlation: LookupCorrelation) {
        let observed_at_ms = now_ms();
        let mut state = lock_or_recover(&self.state);
        expire_lookups(&mut state, observed_at_ms);
        if let Some(previous) = state.lookups.remove(&(kind, correlation)) {
            finish_lookup(&mut state.lookup_totals, previous, observed_at_ms, None);
        }
        while state.lookups.len() >= LOOKUP_CAPACITY {
            let Some(oldest) = state
                .lookups
                .iter()
                .min_by_key(|(_, start)| start.started_at_ms)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(start) = state.lookups.remove(&oldest) {
                finish_lookup(&mut state.lookup_totals, start, observed_at_ms, None);
            }
        }
        state.lookup_totals.started = state.lookup_totals.started.saturating_add(1);
        state.lookups.insert((kind, correlation), LookupStart {
            started_at_ms: observed_at_ms,
        });
    }

    fn lookup_finished(
        &self,
        kind: LookupKind,
        correlation: LookupCorrelation,
        outcome: LookupOutcome,
    ) {
        let observed_at_ms = now_ms();
        let mut state = lock_or_recover(&self.state);
        expire_lookups(&mut state, observed_at_ms);
        if let Some(start) = state.lookups.remove(&(kind, correlation)) {
            finish_lookup(
                &mut state.lookup_totals,
                start,
                observed_at_ms,
                Some(outcome),
            );
        }
    }
}

/// Render one complete snapshot as Prometheus text without identifier-valued labels.
pub fn render_prometheus(snapshot: &OperatorSnapshot) -> String {
    let mut output = String::new();
    render_health_metrics(&mut output, &snapshot.health);
    render_message_metrics(&mut output, &snapshot.messages);
    render_session_metrics(&mut output, &snapshot.session_key);
    render_mailbox_metrics(&mut output, &snapshot.mailboxes);
    render_lookup_metrics(&mut output, &snapshot.dht_lookups);
    render_peer_rating_metrics(&mut output, &snapshot.peer_ratings);
    output
}

/// Append process/API and overlay health gauges.
fn render_health_metrics(output: &mut String, health: &HealthSnapshot) {
    metric_header(
        output,
        "rings_process_api_healthy",
        "gauge",
        "Authenticated operator API health.",
    );
    metric(
        output,
        "rings_process_api_healthy",
        bool_value(health.process_api_healthy),
    );
    metric_header(
        output,
        "rings_overlay_ready",
        "gauge",
        "Conservative local overlay readiness.",
    );
    metric(
        output,
        "rings_overlay_ready",
        bool_value(health.overlay_ready),
    );
    metric_header(
        output,
        "rings_overlay_admitted_peers",
        "gauge",
        "Current admitted transport peers.",
    );
    metric(
        output,
        "rings_overlay_admitted_peers",
        health.admitted_peer_count,
    );
}

/// Append process-lifetime message-operation counters.
fn render_message_metrics(output: &mut String, messages: &MessageTotals) {
    metric_header(
        output,
        "rings_message_operations_total",
        "counter",
        "Node-local process-lifetime logical message operations.",
    );
    labeled_metric(
        output,
        "rings_message_operations_total",
        "action=\"sent\",outcome=\"succeeded\"",
        messages.sent,
    );
    labeled_metric(
        output,
        "rings_message_operations_total",
        "action=\"received\",outcome=\"succeeded\"",
        messages.received,
    );
    labeled_metric(
        output,
        "rings_message_operations_total",
        "action=\"forwarded\",outcome=\"succeeded\"",
        messages.forwarded,
    );
    labeled_metric(
        output,
        "rings_message_operations_total",
        "action=\"stored\",outcome=\"succeeded\"",
        messages.stored,
    );
    labeled_metric(
        output,
        "rings_message_operations_total",
        "action=\"all\",outcome=\"failed\"",
        messages.failed,
    );
}

/// Append active session-delegation lifecycle metrics.
fn render_session_metrics(output: &mut String, session: &SessionKeySnapshot) {
    metric_header(
        output,
        "rings_session_key_valid",
        "gauge",
        "Whether the active session delegation is valid.",
    );
    metric(output, "rings_session_key_valid", bool_value(session.valid));
    metric_header(
        output,
        "rings_session_key_runtime_rotation_supported",
        "gauge",
        "Whether the running process can replace its session delegation without restart.",
    );
    metric(
        output,
        "rings_session_key_runtime_rotation_supported",
        bool_value(session.runtime_rotation_supported),
    );
    metric_header(
        output,
        "rings_session_key_expires_at_seconds",
        "gauge",
        "Active session delegation expiry as Unix epoch seconds.",
    );
    metric(
        output,
        "rings_session_key_expires_at_seconds",
        session.expires_at_ms / 1_000,
    );
    metric_header(
        output,
        "rings_session_key_seconds_remaining",
        "gauge",
        "Saturating seconds until active session delegation expiry.",
    );
    metric(
        output,
        "rings_session_key_seconds_remaining",
        session.remaining_ms / 1_000,
    );
    metric_header(
        output,
        "rings_session_key_rotation_succeeded_total",
        "counter",
        "Successful runtime session delegation rotations.",
    );
    metric(
        output,
        "rings_session_key_rotation_succeeded_total",
        session.rotation_succeeded_total,
    );
    metric_header(
        output,
        "rings_session_key_rotation_failed_total",
        "counter",
        "Failed runtime session delegation rotations.",
    );
    metric(
        output,
        "rings_session_key_rotation_failed_total",
        session.rotation_failed_total,
    );
}

/// Append live mailbox gauges and the process-lifetime store counter.
fn render_mailbox_metrics(output: &mut String, mailboxes: &MailboxSnapshot) {
    metric_header(
        output,
        "rings_mailboxes_registered",
        "gauge",
        "Live relay-inbox carriers retained by this node.",
    );
    metric(output, "rings_mailboxes_registered", mailboxes.registered);
    metric_header(
        output,
        "rings_mailbox_held_messages",
        "gauge",
        "Live held messages across retained relay inboxes.",
    );
    metric(
        output,
        "rings_mailbox_held_messages",
        mailboxes.held_messages,
    );
    metric_header(
        output,
        "rings_mailbox_stored_total",
        "counter",
        "Successful offline-message holds during this process lifetime.",
    );
    metric(output, "rings_mailbox_stored_total", mailboxes.stored_total);
}

/// Append DHT lookup counters, in-flight state, and latency histogram.
fn render_lookup_metrics(output: &mut String, lookups: &DhtLookupSnapshot) {
    metric_header(
        output,
        "rings_dht_lookups_total",
        "counter",
        "DHT lookups begun during this process lifetime.",
    );
    metric(output, "rings_dht_lookups_total", lookups.total);
    metric_header(
        output,
        "rings_dht_lookup_succeeded_total",
        "counter",
        "DHT lookups that accepted an answer.",
    );
    metric(
        output,
        "rings_dht_lookup_succeeded_total",
        lookups.succeeded,
    );
    metric_header(
        output,
        "rings_dht_lookup_failed_total",
        "counter",
        "DHT lookups that ended with an explicit failure.",
    );
    metric(output, "rings_dht_lookup_failed_total", lookups.failed);
    metric_header(
        output,
        "rings_dht_lookup_timed_out_total",
        "counter",
        "DHT lookups that exceeded the bounded observation window.",
    );
    metric(
        output,
        "rings_dht_lookup_timed_out_total",
        lookups.timed_out,
    );
    metric_header(
        output,
        "rings_dht_lookups_in_flight",
        "gauge",
        "DHT lookups currently awaiting an answer.",
    );
    metric(output, "rings_dht_lookups_in_flight", lookups.in_flight);
    metric_header(
        output,
        "rings_dht_lookup_latency_milliseconds",
        "histogram",
        "Local DHT lookup completion latency in milliseconds.",
    );
    for bucket in &lookups.latency_buckets {
        labeled_metric(
            output,
            "rings_dht_lookup_latency_milliseconds_bucket",
            &format!("le=\"{}\"", bucket.le_ms),
            bucket.count,
        );
    }
    labeled_metric(
        output,
        "rings_dht_lookup_latency_milliseconds_bucket",
        "le=\"+Inf\"",
        lookups.latency_count,
    );
    metric(
        output,
        "rings_dht_lookup_latency_milliseconds_sum",
        lookups.latency_sum_ms,
    );
    metric(
        output,
        "rings_dht_lookup_latency_milliseconds_count",
        lookups.latency_count,
    );
}

/// Append finite-class peer-rating counts without peer identifiers as labels.
fn render_peer_rating_metrics(output: &mut String, peer_ratings: &[PeerRatingSnapshot]) {
    metric_header(
        output,
        "rings_local_peer_ratings",
        "gauge",
        "Peers in each bounded local reliability class.",
    );
    for class in ["healthy", "unknown", "degraded"] {
        let count = peer_ratings
            .iter()
            .filter(|rating| rating.reliability == class)
            .count();
        labeled_metric(
            output,
            "rings_local_peer_ratings",
            &format!("reliability=\"{class}\""),
            u64::try_from(count).unwrap_or(u64::MAX),
        );
    }
}

/// Return the current Unix epoch time in milliseconds.
fn now_ms() -> u128 {
    rings_core::utils::get_epoch_ms()
}

/// Acquire a recorder lock, preserving collected state after an earlier panic.
fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Map a finite message activity to its counter-array index.
const fn activity_index(activity: MessageActivity) -> usize {
    match activity {
        MessageActivity::Sent => 0,
        MessageActivity::Received => 1,
        MessageActivity::Forwarded => 2,
        MessageActivity::Stored => 3,
    }
}

/// Map a finite message category to its counter-array index.
const fn category_index(category: MessageCategory) -> usize {
    match category {
        MessageCategory::DhtControl => 0,
        MessageCategory::Storage => 1,
        MessageCategory::E2e => 2,
        MessageCategory::Application => 3,
    }
}

/// Map a finite operation outcome to its counter-array index.
const fn outcome_index(outcome: ObservationOutcome) -> usize {
    match outcome {
        ObservationOutcome::Succeeded => 0,
        ObservationOutcome::Failed => 1,
    }
}

/// Return the stable lowercase export name for a message activity.
const fn activity_name(activity: MessageActivity) -> &'static str {
    match activity {
        MessageActivity::Sent => "sent",
        MessageActivity::Received => "received",
        MessageActivity::Forwarded => "forwarded",
        MessageActivity::Stored => "stored",
    }
}

/// Return the stable lowercase export name for a scheduling category.
const fn category_name(category: MessageCategory) -> &'static str {
    match category {
        MessageCategory::DhtControl => "dht_control",
        MessageCategory::Storage => "storage",
        MessageCategory::E2e => "e2e",
        MessageCategory::Application => "application",
    }
}

/// Return the stable lowercase export name for an operation outcome.
const fn outcome_name(outcome: ObservationOutcome) -> &'static str {
    match outcome {
        ObservationOutcome::Succeeded => "succeeded",
        ObservationOutcome::Failed => "failed",
    }
}

/// Collapse category-specific counters into the public message totals.
fn message_totals(counts: &[[[u64; 2]; 4]; 4]) -> MessageTotals {
    let successful = |activity: MessageActivity| {
        counts
            .get(activity_index(activity))
            .into_iter()
            .flatten()
            .fold(0_u64, |total, category| {
                total.saturating_add(category.first().copied().unwrap_or_default())
            })
    };
    let failed = counts
        .iter()
        .flat_map(|categories| categories.iter())
        .fold(0_u64, |total, outcomes| {
            total.saturating_add(outcomes.get(1).copied().unwrap_or_default())
        });
    MessageTotals {
        sent: successful(MessageActivity::Sent),
        received: successful(MessageActivity::Received),
        forwarded: successful(MessageActivity::Forwarded),
        stored: successful(MessageActivity::Stored),
        failed,
    }
}

/// Complete all retained lookups older than the observation timeout.
fn expire_lookups(state: &mut RuntimeState, observed_at_ms: u128) {
    let expired = state
        .lookups
        .iter()
        .filter(|(_, start)| {
            observed_at_ms.saturating_sub(start.started_at_ms) >= LOOKUP_TIMEOUT_MS
        })
        .map(|(key, _)| *key)
        .collect::<Vec<_>>();
    for key in expired {
        if let Some(start) = state.lookups.remove(&key) {
            finish_lookup(&mut state.lookup_totals, start, observed_at_ms, None);
        }
    }
}

/// Record one terminal lookup outcome and its cumulative latency buckets.
fn finish_lookup(
    totals: &mut LookupTotals,
    start: LookupStart,
    observed_at_ms: u128,
    outcome: Option<LookupOutcome>,
) {
    let latency_ms = observed_at_ms.saturating_sub(start.started_at_ms);
    match outcome {
        Some(LookupOutcome::Succeeded) => totals.succeeded = totals.succeeded.saturating_add(1),
        Some(LookupOutcome::Failed) => totals.failed = totals.failed.saturating_add(1),
        None => totals.timed_out = totals.timed_out.saturating_add(1),
    }
    totals.latency_sum_ms = totals.latency_sum_ms.saturating_add(latency_ms);
    totals.latency_count = totals.latency_count.saturating_add(1);
    for (count, bound) in totals
        .latency_buckets
        .iter_mut()
        .zip(LOOKUP_LATENCY_BUCKETS_MS)
    {
        if latency_ms <= u128::from(bound) {
            *count = count.saturating_add(1);
        }
    }
}

/// Copy internal bounded lookup state into its identifier-free public form.
fn lookup_snapshot(state: &RuntimeState) -> DhtLookupSnapshot {
    DhtLookupSnapshot {
        total: state.lookup_totals.started,
        succeeded: state.lookup_totals.succeeded,
        failed: state.lookup_totals.failed,
        timed_out: state.lookup_totals.timed_out,
        in_flight: u64::try_from(state.lookups.len()).unwrap_or(u64::MAX),
        latency_sum_ms: state.lookup_totals.latency_sum_ms,
        latency_count: state.lookup_totals.latency_count,
        latency_buckets: LOOKUP_LATENCY_BUCKETS_MS
            .iter()
            .copied()
            .zip(state.lookup_totals.latency_buckets)
            .map(|(le_ms, count)| LatencyBucket { le_ms, count })
            .collect(),
    }
}

/// Append Prometheus help and type declarations for one metric family.
fn metric_header(output: &mut String, name: &str, kind: &str, help: &str) {
    output.push_str("# HELP ");
    output.push_str(name);
    output.push(' ');
    output.push_str(help);
    output.push('\n');
    output.push_str("# TYPE ");
    output.push_str(name);
    output.push(' ');
    output.push_str(kind);
    output.push('\n');
}

/// Append one unlabeled Prometheus sample.
fn metric(output: &mut String, name: &str, value: impl std::fmt::Display) {
    output.push_str(name);
    output.push(' ');
    output.push_str(&value.to_string());
    output.push('\n');
}

/// Append one Prometheus sample with a caller-provided finite label set.
fn labeled_metric(output: &mut String, name: &str, labels: &str, value: impl std::fmt::Display) {
    output.push_str(name);
    output.push('{');
    output.push_str(labels);
    output.push_str("} ");
    output.push_str(&value.to_string());
    output.push('\n');
}

/// Encode a Boolean as Prometheus' conventional zero-or-one gauge value.
const fn bool_value(value: bool) -> u8 {
    if value {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(activity: MessageActivity, outcome: ObservationOutcome) -> MessageObservation {
        MessageObservation {
            activity,
            category: MessageCategory::Application,
            message_class: "CustomMessage",
            outcome,
        }
    }

    #[test]
    fn recent_activity_is_bounded_and_payload_free() {
        let recorder = Observability::new();
        for _ in 0..(RECENT_MESSAGE_CAPACITY + 7) {
            recorder.observe_message(observation(
                MessageActivity::Received,
                ObservationOutcome::Succeeded,
            ));
        }
        let snapshot = recorder.runtime_snapshot(now_ms());
        assert_eq!(snapshot.recent_messages.len(), RECENT_MESSAGE_CAPACITY);
        assert_eq!(
            snapshot.messages.received,
            (RECENT_MESSAGE_CAPACITY + 7) as u64
        );
        let json = serde_json::to_string(&snapshot.recent_messages).expect("serialize events");
        assert!(!json.contains("payload"));
        assert!(!json.contains("transaction"));
    }

    #[test]
    fn timed_out_lookup_leaves_no_in_flight_identifier() {
        let recorder = Observability::new();
        let correlation = LookupCorrelation::Transaction(uuid::Uuid::new_v4());
        recorder.lookup_started(LookupKind::Successor, correlation);
        let future = now_ms().saturating_add(LOOKUP_TIMEOUT_MS);
        let snapshot = recorder.runtime_snapshot(future);
        assert_eq!(snapshot.dht_lookups.total, 1);
        assert_eq!(snapshot.dht_lookups.timed_out, 1);
        assert_eq!(snapshot.dht_lookups.in_flight, 0);
    }

    #[test]
    fn prometheus_output_declares_types_without_peer_identifiers() {
        let snapshot = OperatorSnapshot {
            schema_version: OPERATOR_SCHEMA_VERSION,
            generated_at_ms: 2,
            process_started_at_ms: 1,
            counter_scope: "node_local_process_lifetime",
            messages: MessageTotals::default(),
            recent_messages: Vec::new(),
            session_key: SessionKeySnapshot {
                valid: true,
                created_at_ms: 1,
                expires_at_ms: 10_000,
                remaining_ms: 9_000,
                rotation_succeeded_total: 0,
                rotation_failed_total: 0,
                runtime_rotation_supported: false,
            },
            mailboxes: MailboxSnapshot::default(),
            dht_lookups: DhtLookupSnapshot {
                total: 0,
                succeeded: 0,
                failed: 0,
                timed_out: 0,
                in_flight: 0,
                latency_sum_ms: 0,
                latency_count: 0,
                latency_buckets: Vec::new(),
            },
            peer_ratings: vec![PeerRatingSnapshot {
                peer: "did:ring:sensitive-peer".to_string(),
                reliability: "healthy",
                credit_score: 1.0,
                sent: 1,
                failed_to_send: 0,
                received: 1,
                failed_to_receive: 0,
            }],
            health: HealthSnapshot {
                process_api_healthy: true,
                admitted_peer_count: 1,
                has_admitted_peer: true,
                has_successor: true,
                has_predecessor: false,
                overlay_ready: true,
            },
        };

        let output = render_prometheus(&snapshot);
        assert!(output.contains("# TYPE rings_dht_lookup_latency_milliseconds histogram"));
        assert!(output.contains("rings_session_key_runtime_rotation_supported 0"));
        assert!(output.contains("rings_local_peer_ratings{reliability=\"healthy\"} 1"));
        assert!(!output.contains("sensitive-peer"));
    }
}
