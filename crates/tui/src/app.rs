//! Pure application state machine, keyboard/mouse event handling, and command scheduling.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use rings_rpc::protos::rings_node::PeerMeasurementInfo;

use crate::action::ActionLogEntry;
use crate::action::ButtonId;
use crate::action::ClickTarget;
use crate::action::CommandModal;
use crate::action::CommandOutcome;
use crate::action::DispatchedCommand;
use crate::action::DispatchedOutcome;
use crate::action::HitMap;
use crate::action::ModalKind;
use crate::action::NodeCommand;
use crate::action::RequestMeta;
use crate::action::TabId;
use crate::config::ResolvedTuiConfig;
use crate::types::DhtKeyLookupReport;
use crate::types::NodeSnapshot;
use crate::types::TelemetryHistory;

/// Maximum number of command execution logs retained in memory.
pub const MAX_ACTION_LOGS: usize = 100;

/// Maximum number of pending commands buffered in the dispatch queue.
pub const MAX_PENDING_COMMANDS: usize = 32;

/// Maximum number of concurrent RPC tasks executing in flight at once.
pub const MAX_CONCURRENT_COMMANDS: usize = 8;

/// Connection state between the TUI and the target `rings-node` API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Initial poll has not completed yet.
    Connecting,
    /// Node API responded to the most recent poll.
    Online,
    /// Node API was unreachable or returned an error on the most recent poll.
    Offline(String),
}

/// Sub-view selection inside the `4:DHT & Storage` tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DhtSubtab {
    /// Chord finger table ranges.
    #[default]
    FingerTable,
    /// Persistent DHT data storage items.
    PersistentStorage,
    /// Ephemeral DHT cache storage items.
    CacheStorage,
}

impl DhtSubtab {
    /// Cycle to the next sub-view.
    pub const fn next(self) -> Self {
        match self {
            Self::FingerTable => Self::PersistentStorage,
            Self::PersistentStorage => Self::CacheStorage,
            Self::CacheStorage => Self::FingerTable,
        }
    }

    /// Cycle to the previous sub-view.
    pub const fn prev(self) -> Self {
        match self {
            Self::FingerTable => Self::CacheStorage,
            Self::PersistentStorage => Self::FingerTable,
            Self::CacheStorage => Self::PersistentStorage,
        }
    }

    /// Display title of the sub-view.
    pub const fn title(self) -> &'static str {
        match self {
            Self::FingerTable => "Finger Table",
            Self::PersistentStorage => "Persistent Storage",
            Self::CacheStorage => "Cache Storage",
        }
    }
}

/// Sub-view selection inside the `5:Directory` tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectorySubtab {
    /// Online-node presence descriptors (`lookupOnlineNodes`).
    #[default]
    OnlineNodes,
    /// Application-layer onion exit descriptors (`lookupOnionExits`).
    OnionExits,
}

impl DirectorySubtab {
    /// Toggle between `OnlineNodes` and `OnionExits`.
    pub const fn toggle(self) -> Self {
        match self {
            Self::OnlineNodes => Self::OnionExits,
            Self::OnionExits => Self::OnlineNodes,
        }
    }

    /// Display title of the sub-view.
    pub const fn title(self) -> &'static str {
        match self {
            Self::OnlineNodes => "Online Nodes Registry",
            Self::OnionExits => "Onion Exits Registry",
        }
    }
}

/// Classification of an in-flight background command for concurrency and recovery tracking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InFlightKind {
    /// Background or manual `RefreshSnapshot` poll (at most 1 in flight at a time).
    Refresh,
    /// State-mutating RPC command (serialized in FIFO order so at most 1 runs at a time).
    Mutating,
    /// Read-only inspection RPC command (`InspectPeer`, `DhtLookupKey`, `FetchTopic`, `LookupService`).
    ReadOnly,
}

/// Active keyboard focus pane within the `6:Commands` tab when the global toolbar is not focused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CommandsFocus {
    /// The 4x4 interactive command button grid.
    #[default]
    Workbench,
    /// The scrollable `Command Output` inspector panel.
    OutputPanel,
    /// The `Command Execution Log` history table.
    ExecutionLog,
}

/// Complete state of a running `rings-tui` session.
pub struct App {
    /// Resolved node endpoint, config path, and token state.
    pub config: ResolvedTuiConfig,
    /// Currently selected top-level tab.
    pub active_tab: TabId,
    /// Most recent snapshot collected from the node.
    pub snapshot: NodeSnapshot,
    /// Rolling telemetry history for sparklines.
    pub history: TelemetryHistory,
    /// Current node API connection status.
    pub connection_state: ConnectionState,
    /// Whether periodic background polling is enabled.
    pub auto_poll: bool,
    /// Whether directory queries include expired descriptors.
    pub include_expired_directory: bool,
    /// Active sub-view in `4:DHT & Storage`.
    pub dht_subtab: DhtSubtab,
    /// Active sub-view in `5:Directory`.
    pub directory_subtab: DirectorySubtab,
    /// Selected row index in the Peers table.
    pub selected_peer_idx: usize,
    /// Persistent viewport scroll offset for the Peers table.
    pub peer_table_offset: usize,
    /// Stable DID of the currently selected peer row, preserved across snapshot refreshes.
    pub selected_peer_did: Option<String>,
    /// Selected row index in the Recent Message Events table.
    pub selected_event_idx: usize,
    /// Persistent viewport scroll offset for the Recent Message Events table.
    pub event_table_offset: usize,
    /// Selected row index in the DHT / Storage table.
    pub selected_dht_idx: usize,
    /// Persistent viewport scroll offset for the DHT / Storage table.
    pub dht_table_offset: usize,
    /// Selected row index in the Directory table.
    pub selected_directory_idx: usize,
    /// Persistent viewport scroll offset for the Directory table.
    pub directory_table_offset: usize,
    /// Selected button index in the `6:Commands` workbench grid.
    pub selected_workbench_idx: usize,
    /// Selected row index in the `6:Commands` execution log table.
    pub selected_log_idx: usize,
    /// Stable sequence ID of the currently selected execution log entry.
    pub selected_log_id: Option<u64>,
    /// Whether new log entries automatically advance the selection and output inspector (`true`),
    /// or whether the user is pinned to a historical log entry (`false`).
    pub follow_latest_log: bool,
    /// Persistent viewport scroll offset for the `6:Commands` execution log table.
    pub log_table_offset: usize,
    /// Active keyboard focus pane in `6:Commands`.
    pub commands_focus: CommandsFocus,
    /// Vertical scroll offset (in visual lines) for the Command Output inspector panel in `6:Commands`.
    pub output_scroll: usize,
    /// Upper bound for `output_scroll` computed from the rendered output line count and panel height.
    pub output_max_scroll: usize,
    /// Optional keyboard focus on the persistent toolbar buttons.
    pub focused_toolbar_idx: Option<usize>,
    /// Active modal input dialog, when open.
    pub modal: Option<CommandModal>,
    /// Whether the help overlay is open.
    pub show_help: bool,
    /// Vertical scroll offset (in visual lines) for the Help overlay when terminal height is compact.
    pub help_scroll: usize,
    /// Upper bound for `help_scroll` computed during Help overlay rendering.
    pub help_max_scroll: usize,
    /// Number of asynchronous commands currently executing in the active client epoch.
    pub in_flight_commands: usize,
    /// Whether a `RefreshSnapshot` request is currently in flight in the active client epoch.
    pub has_refresh_in_flight: bool,
    /// Active in-flight requests keyed by `request_id` in the current `client_epoch`.
    in_flight_requests: BTreeMap<u64, InFlightKind>,
    /// Latest multi-line command output (e.g. SDP offer/answer, topic messages, service DIDs).
    pub last_command_output: Option<String>,
    /// Latest Chord DHT key lookup report.
    pub last_dht_lookup: Option<DhtKeyLookupReport>,
    /// Services confirmed registered by this node during this session.
    pub registered_services: Vec<String>,
    /// Last topic name used in PubSub dialogs for convenience.
    pub last_topic_name: Option<String>,
    /// Bounded history of executed button commands.
    pub action_logs: VecDeque<ActionLogEntry>,
    /// Next sequence ID for action logs.
    next_log_id: u64,
    /// Monotonic client generation counter incremented when the HTTP client is replaced.
    pub client_epoch: u64,
    /// Next monotonic request sequence ID assigned on dispatch.
    next_request_id: u64,
    /// Highest `request_id` of any applied `RefreshSnapshot` outcome in the current epoch.
    latest_applied_snapshot_id: u64,
    /// Per-peer `(request_id, PeerMeasurementInfo)` from `InspectPeer` in the current epoch,
    /// preventing older snapshots or older per-peer queries from overwriting newer measurements.
    peer_measurement_versions: BTreeMap<String, (u64, PeerMeasurementInfo)>,
    /// Highest `request_id` of any applied `DhtLookupKey` report in the current epoch.
    latest_dht_lookup_req_id: u64,
    /// Highest `request_id` of any applied `last_command_output` in the current epoch.
    latest_output_req_id: u64,
    /// Screen hit-test map populated during rendering for mouse clicks.
    pub hit_map: HitMap,
    /// Whether the user has requested to exit the TUI.
    pub should_quit: bool,
    /// Flag set when the user requests reloading the API token from disk.
    pub pending_token_reload: bool,
    /// Bounded queue of commands ready to be sent to the async node worker.
    pending_commands: VecDeque<NodeCommand>,
}

impl App {
    /// Construct a new [`App`] from a resolved TUI configuration and queue an initial refresh.
    pub fn new(config: ResolvedTuiConfig) -> Self {
        let mut pending_commands = VecDeque::with_capacity(MAX_PENDING_COMMANDS);
        pending_commands.push_back(NodeCommand::RefreshSnapshot {
            include_expired_directory: false,
        });
        let registered_services = if config.demo {
            vec!["ipfs-gateway".to_string(), "turn-stun".to_string()]
        } else {
            Vec::new()
        };
        Self {
            config,
            active_tab: TabId::Overview,
            snapshot: NodeSnapshot::default(),
            history: TelemetryHistory::default(),
            connection_state: ConnectionState::Connecting,
            auto_poll: true,
            include_expired_directory: false,
            dht_subtab: DhtSubtab::FingerTable,
            directory_subtab: DirectorySubtab::OnlineNodes,
            selected_peer_idx: 0,
            peer_table_offset: 0,
            selected_peer_did: None,
            selected_event_idx: 0,
            event_table_offset: 0,
            selected_dht_idx: 0,
            dht_table_offset: 0,
            selected_directory_idx: 0,
            directory_table_offset: 0,
            selected_workbench_idx: 0,
            selected_log_idx: 0,
            selected_log_id: None,
            follow_latest_log: true,
            log_table_offset: 0,
            commands_focus: CommandsFocus::Workbench,
            output_scroll: 0,
            output_max_scroll: 0,
            focused_toolbar_idx: None,
            modal: None,
            show_help: false,
            help_scroll: 0,
            help_max_scroll: 0,
            in_flight_commands: 0,
            has_refresh_in_flight: false,
            in_flight_requests: BTreeMap::new(),
            last_command_output: None,
            last_dht_lookup: None,
            registered_services,
            last_topic_name: None,
            action_logs: VecDeque::with_capacity(MAX_ACTION_LOGS),
            next_log_id: 1,
            client_epoch: 1,
            next_request_id: 1,
            latest_applied_snapshot_id: 0,
            peer_measurement_versions: BTreeMap::new(),
            latest_dht_lookup_req_id: 0,
            latest_output_req_id: 0,
            hit_map: HitMap::default(),
            should_quit: false,
            pending_token_reload: false,
            pending_commands,
        }
    }

    /// Recompute cached in-flight command and refresh counters from `in_flight_requests`.
    fn sync_in_flight_counters(&mut self) {
        self.in_flight_commands = self.in_flight_requests.len();
        self.has_refresh_in_flight = self
            .in_flight_requests
            .values()
            .any(|&k| k == InFlightKind::Refresh);
    }

    /// Return whether at least one mutating RPC request is currently in flight.
    fn has_mutating_in_flight(&self) -> bool {
        self.in_flight_requests
            .values()
            .any(|&k| k == InFlightKind::Mutating)
    }

    /// Clear all in-flight request tracking (e.g. during non-interactive preview rendering).
    pub fn clear_in_flight(&mut self) {
        self.in_flight_requests.clear();
        self.sync_in_flight_counters();
    }

    /// Enqueue or coalesce a `RefreshSnapshot` command so rapid manual refreshes do not flood the queue.
    pub fn enqueue_refresh(&mut self) {
        let desired = self.include_expired_directory;
        if let Some(NodeCommand::RefreshSnapshot {
            include_expired_directory,
        }) = self
            .pending_commands
            .iter_mut()
            .find(|cmd| matches!(cmd, NodeCommand::RefreshSnapshot { .. }))
        {
            *include_expired_directory = desired;
            return;
        }
        if self.pending_commands.len() < MAX_PENDING_COMMANDS {
            self.pending_commands
                .push_back(NodeCommand::RefreshSnapshot {
                    include_expired_directory: desired,
                });
        }
    }

    /// Enqueue `command` for execution, coalescing refreshes and rejecting when the queue is full.
    fn enqueue_command(&mut self, command: NodeCommand) -> std::result::Result<(), String> {
        if matches!(command, NodeCommand::RefreshSnapshot { .. }) {
            self.enqueue_refresh();
            return Ok(());
        }
        if self.pending_commands.len() >= MAX_PENDING_COMMANDS {
            let msg = format!(
                "Command queue is full ({MAX_PENDING_COMMANDS}); wait for in-flight RPCs to finish and retry"
            );
            self.push_log(command.title(), false, msg.clone(), None, 0);
            return Err(msg);
        }
        self.pending_commands.push_back(command);
        Ok(())
    }

    /// Advance to a new client generation after a successful token reload / client rebuild,
    /// invalidating any in-flight outcomes from the previous client epoch.
    pub fn bump_client_epoch(&mut self) {
        self.client_epoch = self.client_epoch.saturating_add(1);
        self.in_flight_requests.clear();
        self.sync_in_flight_counters();
        self.latest_applied_snapshot_id = 0;
        self.peer_measurement_versions.clear();
        self.latest_dht_lookup_req_id = 0;
        self.latest_output_req_id = 0;
        self.pending_commands.clear();
        self.enqueue_refresh();
    }

    /// Recover accounting for a specific [`RequestMeta`] when its background worker task panics or
    /// aborts unexpectedly without delivering a [`DispatchedOutcome`].
    pub fn on_worker_aborted_or_panicked(&mut self, meta: RequestMeta, reason: &str) {
        if meta.client_epoch != self.client_epoch {
            return;
        }
        self.in_flight_requests.remove(&meta.request_id);
        self.sync_in_flight_counters();
        self.push_log_with_request_id(
            meta.request_id,
            "workerTask",
            false,
            format!(
                "Background task #{} terminated unexpectedly: {reason}",
                meta.request_id
            ),
            None,
            0,
        );
    }

    /// Drain the next eligible [`NodeCommand`] wrapped with its [`RequestMeta`] envelope,
    /// respecting [`MAX_CONCURRENT_COMMANDS`], ensuring at most one `RefreshSnapshot` runs at a time,
    /// and serializing state-mutating commands in FIFO order.
    pub fn pop_pending_command(&mut self) -> Option<DispatchedCommand> {
        if self.in_flight_requests.len() >= MAX_CONCURRENT_COMMANDS {
            return None;
        }
        let refresh_busy = self.has_refresh_in_flight;
        let mutating_busy = self.has_mutating_in_flight();
        let pos = self.pending_commands.iter().position(|cmd| {
            if matches!(cmd, NodeCommand::RefreshSnapshot { .. }) {
                !refresh_busy
            } else if cmd.is_mutating() {
                !mutating_busy
            } else {
                true
            }
        })?;
        let command = self.pending_commands.remove(pos)?;
        let kind = if matches!(command, NodeCommand::RefreshSnapshot { .. }) {
            InFlightKind::Refresh
        } else if command.is_mutating() {
            InFlightKind::Mutating
        } else {
            InFlightKind::ReadOnly
        };
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        self.in_flight_requests.insert(request_id, kind);
        self.sync_in_flight_counters();
        Some(DispatchedCommand {
            meta: RequestMeta {
                client_epoch: self.client_epoch,
                request_id,
            },
            command,
        })
    }

    /// Queue a background snapshot refresh if auto-polling is enabled and no command is in flight.
    pub fn on_tick(&mut self) {
        if self.auto_poll && self.in_flight_requests.is_empty() && self.pending_commands.is_empty()
        {
            self.enqueue_refresh();
        }
    }

    /// Apply a [`DispatchedOutcome`] from the worker task if its [`RequestMeta`] matches the active
    /// client epoch and is not superseded by a newer request.
    pub fn apply_outcome(&mut self, dispatched: DispatchedOutcome) {
        if dispatched.meta.client_epoch != self.client_epoch {
            return;
        }
        self.in_flight_requests.remove(&dispatched.meta.request_id);
        self.sync_in_flight_counters();

        match dispatched.outcome {
            CommandOutcome::Snapshot(mut snapshot) => {
                if dispatched.meta.request_id < self.latest_applied_snapshot_id {
                    return;
                }
                if snapshot.include_expired_directory != self.include_expired_directory {
                    return;
                }
                // Preserve previously fetched directory rows when a directory subquery fails transiently
                // under the same filter setting.
                if snapshot.online_nodes_error.is_some()
                    && snapshot.online_nodes.is_empty()
                    && self.snapshot.include_expired_directory == snapshot.include_expired_directory
                    && !self.snapshot.online_nodes.is_empty()
                {
                    snapshot
                        .online_nodes
                        .clone_from(&self.snapshot.online_nodes);
                }
                if snapshot.onion_exits_error.is_some()
                    && snapshot.onion_exits.is_empty()
                    && self.snapshot.include_expired_directory == snapshot.include_expired_directory
                    && !self.snapshot.onion_exits.is_empty()
                {
                    snapshot.onion_exits.clone_from(&self.snapshot.onion_exits);
                }

                // Overlay any per-peer `InspectPeer` measurements whose `request_id` is newer than
                // this snapshot so an older snapshot never overwrites a newer single-peer inspection.
                let snap_req_id = dispatched.meta.request_id;
                for (meas_req_id, meas_info) in self.peer_measurement_versions.values() {
                    if *meas_req_id > snap_req_id {
                        if let Some(existing) = snapshot
                            .measurements
                            .iter_mut()
                            .find(|m| m.did.eq_ignore_ascii_case(&meas_info.did))
                        {
                            *existing = meas_info.clone();
                        } else {
                            snapshot.measurements.push(meas_info.clone());
                        }
                    }
                }

                self.latest_applied_snapshot_id = snap_req_id;
                self.connection_state = ConnectionState::Online;
                self.history.record(&snapshot);
                self.snapshot = *snapshot;
                self.clamp_selections();
            }
            CommandOutcome::SnapshotError {
                include_expired_directory,
                error,
            } => {
                if dispatched.meta.request_id < self.latest_applied_snapshot_id {
                    return;
                }
                if include_expired_directory != self.include_expired_directory {
                    return;
                }
                self.latest_applied_snapshot_id = dispatched.meta.request_id;
                self.connection_state = ConnectionState::Offline(error);
            }
            CommandOutcome::CommandSuccess {
                command,
                summary,
                output,
                measurement,
                dht_lookup,
                registered_service,
                duration_ms,
            } => {
                if let Some(svc) = registered_service {
                    if !self.registered_services.contains(&svc) {
                        self.registered_services.push(svc);
                    }
                }
                if let Some(meas) = measurement {
                    let peer_key = meas.did.to_ascii_lowercase();
                    let prev_peer_req_id = self
                        .peer_measurement_versions
                        .get(&peer_key)
                        .map(|(id, _)| *id)
                        .unwrap_or(0);
                    if dispatched.meta.request_id >= prev_peer_req_id {
                        self.peer_measurement_versions
                            .insert(peer_key, (dispatched.meta.request_id, (*meas).clone()));
                        if dispatched.meta.request_id >= self.latest_applied_snapshot_id {
                            if let Some(existing) = self
                                .snapshot
                                .measurements
                                .iter_mut()
                                .find(|m| m.did.eq_ignore_ascii_case(&meas.did))
                            {
                                *existing = *meas;
                            } else {
                                self.snapshot.measurements.push(*meas);
                            }
                            self.clamp_selections();
                        }
                    }
                }
                if let Some(lookup) = dht_lookup {
                    if dispatched.meta.request_id >= self.latest_dht_lookup_req_id {
                        self.latest_dht_lookup_req_id = dispatched.meta.request_id;
                        self.last_dht_lookup = Some(*lookup);
                    }
                }
                self.push_log_with_request_id(
                    dispatched.meta.request_id,
                    command,
                    true,
                    summary,
                    output,
                    duration_ms,
                );
                // Coalesce a snapshot refresh after mutating node state.
                self.enqueue_refresh();
            }
            CommandOutcome::CommandError {
                command,
                error,
                duration_ms,
            } => {
                self.push_log_with_request_id(
                    dispatched.meta.request_id,
                    command,
                    false,
                    error,
                    None,
                    duration_ms,
                );
            }
        }
    }

    /// Record an entry in the bounded action log.
    pub fn push_log(
        &mut self,
        command: &'static str,
        succeeded: bool,
        message: String,
        details: Option<String>,
        duration_ms: u64,
    ) {
        self.push_log_with_request_id(0, command, succeeded, message, details, duration_ms);
    }

    /// Record an entry tagged with its originating `request_id` (`0` for synchronous local toggles),
    /// preserving any historical log selection when the user is browsing history and keeping the
    /// selected log row and `last_command_output` synchronized.
    pub fn push_log_with_request_id(
        &mut self,
        request_id: u64,
        command: &'static str,
        succeeded: bool,
        message: String,
        details: Option<String>,
        duration_ms: u64,
    ) {
        let id = self.next_log_id;
        self.next_log_id = self.next_log_id.saturating_add(1);
        let entry = ActionLogEntry {
            id,
            request_id,
            command,
            succeeded,
            message,
            details,
            duration_ms,
        };
        let entry_output = entry.formatted_output();

        let is_stale_out_of_order = request_id > 0 && request_id < self.latest_output_req_id;
        if request_id > self.latest_output_req_id {
            self.latest_output_req_id = request_id;
        }

        let insert_pos = if is_stale_out_of_order {
            self.action_logs
                .iter()
                .rposition(|existing| existing.request_id > request_id)
                .map(|idx| idx + 1)
                .unwrap_or(self.action_logs.len())
        } else {
            0
        };

        if self.action_logs.len() >= MAX_ACTION_LOGS {
            self.action_logs.pop_back();
        }
        let clamped_pos = insert_pos.min(self.action_logs.len());
        self.action_logs.insert(clamped_pos, entry);

        let user_pinned_to_history = !self.follow_latest_log
            || (self.commands_focus != CommandsFocus::Workbench && self.selected_log_id.is_some());

        if user_pinned_to_history || is_stale_out_of_order {
            if clamped_pos <= self.log_table_offset && self.log_table_offset > 0 {
                self.log_table_offset = self
                    .log_table_offset
                    .saturating_add(1)
                    .min(self.action_logs.len().saturating_sub(1));
            }
            if let Some(pinned_id) = self.selected_log_id {
                if let Some(pos) = self.action_logs.iter().position(|e| e.id == pinned_id) {
                    self.selected_log_idx = pos;
                    if self.last_command_output.is_none() {
                        self.load_selected_log_into_output();
                    }
                    return;
                }
            }
            // Pinned entry was evicted from the 100-entry ring buffer; clamp and sync output.
            self.clamp_selections();
            self.load_selected_log_into_output();
        } else {
            self.selected_log_idx = 0;
            self.selected_log_id = Some(id);
            self.log_table_offset = 0;
            self.last_command_output = Some(entry_output);
            self.output_scroll = 0;
        }
    }

    /// Return the DID of the currently selected peer (or directory entry) in the exact same row
    /// order rendered by [`crate::types::NodeSnapshot::peer_traffic_rows`].
    pub fn selected_did(&self) -> Option<&str> {
        match self.active_tab {
            TabId::Directory => match self.directory_subtab {
                DirectorySubtab::OnlineNodes => self
                    .snapshot
                    .online_nodes
                    .get(self.selected_directory_idx)
                    .map(|n| n.did.as_str()),
                DirectorySubtab::OnionExits => self
                    .snapshot
                    .onion_exits
                    .get(self.selected_directory_idx)
                    .map(|e| e.did.as_str()),
            },
            _ => self.snapshot.peer_did_at(self.selected_peer_idx),
        }
    }

    /// Return the storage key or finger DID currently selected in `4:DHT & Storage`, if any.
    pub fn selected_dht_key(&self) -> Option<&str> {
        if self.active_tab != TabId::DhtStorage {
            return None;
        }
        let swarm = self.snapshot.swarm.as_ref()?;
        match self.dht_subtab {
            DhtSubtab::FingerTable => swarm
                .dht
                .as_ref()?
                .finger_table_ranges
                .get(self.selected_dht_idx)?
                .did
                .as_deref(),
            DhtSubtab::PersistentStorage => swarm
                .persistence_storage
                .as_ref()?
                .items
                .get(self.selected_dht_idx)
                .map(|i| i.key.as_str()),
            DhtSubtab::CacheStorage => swarm
                .cache_storage
                .as_ref()?
                .items
                .get(self.selected_dht_idx)
                .map(|i| i.key.as_str()),
        }
    }

    /// Trigger a button action by [`ButtonId`], either immediately or by opening its parameter modal.
    pub fn trigger_button(&mut self, button: ButtonId) {
        let default_did = self.selected_did().map(str::to_owned);
        let default_topic = self.last_topic_name.clone();
        match button {
            ButtonId::RefreshNow => {
                self.enqueue_refresh();
            }
            ButtonId::ToggleAutoPoll => {
                self.auto_poll = !self.auto_poll;
                let state_str = if self.auto_poll { "RESUMED" } else { "PAUSED" };
                self.push_log(
                    "autoPoll",
                    true,
                    format!("Background auto-polling {state_str}"),
                    None,
                    0,
                );
            }
            ButtonId::ConnectUrl => {
                self.modal = Some(CommandModal::open(
                    ModalKind::ConnectUrl,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::ConnectDid => {
                self.modal = Some(CommandModal::open(
                    ModalKind::ConnectDid,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::ConnectSeed => {
                self.modal = Some(CommandModal::open(
                    ModalKind::ConnectSeed,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::DisconnectPeer => {
                self.modal = Some(CommandModal::open(
                    ModalKind::DisconnectPeer,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::CreateOffer => {
                self.modal = Some(CommandModal::open(
                    ModalKind::CreateOffer,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::AnswerOffer => {
                self.modal = Some(CommandModal::open(
                    ModalKind::AnswerOffer,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::AcceptAnswer => {
                self.modal = Some(CommandModal::open(
                    ModalKind::AcceptAnswer,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::SendMessage => {
                self.modal = Some(CommandModal::open(
                    ModalKind::SendMessage,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::PublishTopic => {
                self.modal = Some(CommandModal::open(
                    ModalKind::PublishTopic,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::FetchTopic => {
                self.modal = Some(CommandModal::open(
                    ModalKind::FetchTopic,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::RegisterService => {
                self.modal = Some(CommandModal::open(
                    ModalKind::RegisterService,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::LookupService => {
                self.modal = Some(CommandModal::open(
                    ModalKind::LookupService,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::DhtLookupKey => {
                let prefill = self.selected_dht_key().map(str::to_owned).or(default_topic);
                self.modal = Some(CommandModal::open(
                    ModalKind::DhtLookupKey,
                    default_did.as_deref(),
                    prefill.as_deref(),
                ));
            }
            ButtonId::InspectPeer => {
                self.modal = Some(CommandModal::open(
                    ModalKind::InspectPeer,
                    default_did.as_deref(),
                    default_topic.as_deref(),
                ));
            }
            ButtonId::ToggleExpiredDirectory => {
                self.include_expired_directory = !self.include_expired_directory;
                self.enqueue_refresh();
                self.push_log(
                    "directoryFilter",
                    true,
                    format!("include_expired set to {}", self.include_expired_directory),
                    None,
                    0,
                );
            }
            ButtonId::ReloadToken => {
                self.pending_token_reload = true;
            }
            ButtonId::ShowHelp => {
                self.show_help = !self.show_help;
                self.help_scroll = 0;
            }
            ButtonId::Quit => {
                self.should_quit = true;
            }
        }
    }

    /// Submit the currently open modal dialog if its inputs are valid and the command queue has space.
    pub fn submit_modal(&mut self) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        match modal.build_command() {
            Ok(cmd) => match self.enqueue_command(cmd.clone()) {
                Ok(()) => match &cmd {
                    NodeCommand::PublishTopic { topic, .. }
                    | NodeCommand::FetchTopic { topic, .. } => {
                        self.last_topic_name = Some(topic.clone());
                    }
                    NodeCommand::DhtLookupKey { key } => {
                        self.last_topic_name = Some(key.clone());
                    }
                    _ => {}
                },
                Err(queue_err) => {
                    modal.error = Some(queue_err);
                    self.modal = Some(modal);
                }
            },
            Err(_) => {
                self.modal = Some(modal);
            }
        }
    }

    /// Handle a terminal keyboard event.
    pub fn handle_key_event(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        if self.show_help {
            match key.code {
                KeyCode::Esc
                | KeyCode::Char('?')
                | KeyCode::Char('q')
                | KeyCode::Char('Q')
                | KeyCode::Enter => {
                    self.show_help = false;
                    self.help_scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1).min(self.help_max_scroll);
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(5);
                }
                KeyCode::PageDown => {
                    self.help_scroll = self.help_scroll.saturating_add(5).min(self.help_max_scroll);
                }
                KeyCode::Home => {
                    self.help_scroll = 0;
                }
                KeyCode::End => {
                    self.help_scroll = self.help_max_scroll;
                }
                _ => {}
            }
            return;
        }

        if self.modal.is_some() {
            self.handle_modal_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
            KeyCode::Char('?') => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            KeyCode::Char('1') => {
                self.focused_toolbar_idx = None;
                self.active_tab = TabId::Overview;
            }
            KeyCode::Char('2') => {
                self.focused_toolbar_idx = None;
                self.active_tab = TabId::Monitor;
            }
            KeyCode::Char('3') => {
                self.focused_toolbar_idx = None;
                self.active_tab = TabId::Peers;
            }
            KeyCode::Char('4') => {
                self.focused_toolbar_idx = None;
                self.active_tab = TabId::DhtStorage;
            }
            KeyCode::Char('5') => {
                self.focused_toolbar_idx = None;
                self.active_tab = TabId::Directory;
            }
            KeyCode::Char('6') => {
                self.focused_toolbar_idx = None;
                self.active_tab = TabId::Commands;
            }
            KeyCode::Tab => {
                self.focused_toolbar_idx = None;
                self.active_tab = self.active_tab.next();
            }
            KeyCode::BackTab => {
                self.focused_toolbar_idx = None;
                self.active_tab = self.active_tab.prev();
            }
            // Button command hotkeys:
            KeyCode::Char('r') | KeyCode::Char('R') => self.trigger_button(ButtonId::RefreshNow),
            KeyCode::Char('p') | KeyCode::Char('P') => {
                self.trigger_button(ButtonId::ToggleAutoPoll)
            }
            KeyCode::Char('c') | KeyCode::Char('C') => self.trigger_button(ButtonId::ConnectUrl),
            KeyCode::Char('d') | KeyCode::Char('D') => self.trigger_button(ButtonId::ConnectDid),
            KeyCode::Char('s') | KeyCode::Char('S') => self.trigger_button(ButtonId::ConnectSeed),
            KeyCode::Char('x') | KeyCode::Char('X') => {
                self.trigger_button(ButtonId::DisconnectPeer)
            }
            KeyCode::Char('o') | KeyCode::Char('O') => self.trigger_button(ButtonId::CreateOffer),
            KeyCode::Char('a') | KeyCode::Char('A') => self.trigger_button(ButtonId::AnswerOffer),
            KeyCode::Char('y') | KeyCode::Char('Y') => self.trigger_button(ButtonId::AcceptAnswer),
            KeyCode::Char('m') | KeyCode::Char('M') => self.trigger_button(ButtonId::SendMessage),
            KeyCode::Char('t') | KeyCode::Char('T') => self.trigger_button(ButtonId::PublishTopic),
            KeyCode::Char('f') | KeyCode::Char('F') => self.trigger_button(ButtonId::FetchTopic),
            KeyCode::Char('g') | KeyCode::Char('G') => {
                self.trigger_button(ButtonId::RegisterService)
            }
            KeyCode::Char('v') | KeyCode::Char('V') => self.trigger_button(ButtonId::LookupService),
            KeyCode::Char('/') | KeyCode::Char('w') | KeyCode::Char('W') => {
                self.trigger_button(ButtonId::DhtLookupKey)
            }
            KeyCode::Char('i') | KeyCode::Char('I') => self.trigger_button(ButtonId::InspectPeer),
            KeyCode::Char('e') | KeyCode::Char('E') => {
                self.trigger_button(ButtonId::ToggleExpiredDirectory)
            }
            KeyCode::Char('u') | KeyCode::Char('U') => self.trigger_button(ButtonId::ReloadToken),
            KeyCode::Char(']') => {
                self.output_scroll = self
                    .output_scroll
                    .saturating_add(3)
                    .min(self.output_max_scroll);
            }
            KeyCode::Char('[') => {
                self.output_scroll = self.output_scroll.saturating_sub(3);
            }
            KeyCode::Char('}') => {
                self.step_log_selection_down(1);
            }
            KeyCode::Char('{') => {
                self.step_log_selection_up(1);
            }
            KeyCode::PageDown => {
                if self.active_tab == TabId::Commands {
                    if self.commands_focus == CommandsFocus::ExecutionLog {
                        self.step_log_selection_down(5);
                    } else {
                        self.output_scroll = self
                            .output_scroll
                            .saturating_add(5)
                            .min(self.output_max_scroll);
                    }
                } else {
                    for _ in 0..5 {
                        self.move_selection_down();
                    }
                }
            }
            KeyCode::PageUp => {
                if self.active_tab == TabId::Commands {
                    if self.commands_focus == CommandsFocus::ExecutionLog {
                        self.step_log_selection_up(5);
                    } else {
                        self.output_scroll = self.output_scroll.saturating_sub(5);
                    }
                } else {
                    for _ in 0..5 {
                        self.move_selection_up();
                    }
                }
            }
            KeyCode::Char('b') | KeyCode::Char('B') => {
                if self.focused_toolbar_idx.is_some() {
                    self.focused_toolbar_idx = None;
                } else {
                    self.focused_toolbar_idx = Some(0);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(idx) = self.focused_toolbar_idx {
                    if let Some(&btn) = ButtonId::TOOLBAR.get(idx) {
                        self.trigger_button(btn);
                    }
                } else if self.active_tab == TabId::DhtStorage {
                    self.dht_subtab = self.dht_subtab.next();
                    self.selected_dht_idx = 0;
                    self.dht_table_offset = 0;
                } else if self.active_tab == TabId::Directory {
                    self.directory_subtab = self.directory_subtab.toggle();
                    self.selected_directory_idx = 0;
                    self.directory_table_offset = 0;
                } else if self.active_tab == TabId::Commands {
                    match self.commands_focus {
                        CommandsFocus::Workbench => {
                            if let Some(&btn) = ButtonId::WORKBENCH.get(self.selected_workbench_idx)
                            {
                                self.trigger_button(btn);
                            }
                        }
                        CommandsFocus::ExecutionLog => {
                            self.load_selected_log_into_output();
                        }
                        CommandsFocus::OutputPanel => {}
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(idx) = self.focused_toolbar_idx {
                    if let Some(&btn) = ButtonId::TOOLBAR.get(idx) {
                        self.trigger_button(btn);
                    }
                } else if self.active_tab == TabId::Commands {
                    match self.commands_focus {
                        CommandsFocus::Workbench => {
                            if let Some(&btn) = ButtonId::WORKBENCH.get(self.selected_workbench_idx)
                            {
                                self.trigger_button(btn);
                            }
                        }
                        CommandsFocus::ExecutionLog => {
                            self.load_selected_log_into_output();
                        }
                        CommandsFocus::OutputPanel => {}
                    }
                } else if self.active_tab == TabId::Peers {
                    self.trigger_button(ButtonId::InspectPeer);
                } else if self.active_tab == TabId::DhtStorage {
                    self.trigger_button(ButtonId::DhtLookupKey);
                } else if self.active_tab == TabId::Directory {
                    self.trigger_button(ButtonId::ConnectDid);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection_down(),
            KeyCode::Left | KeyCode::Char('h') => self.move_selection_left(),
            KeyCode::Right | KeyCode::Char('l') => self.move_selection_right(),
            KeyCode::Esc => {
                self.focused_toolbar_idx = None;
                self.commands_focus = CommandsFocus::Workbench;
            }
            _ => {}
        }
    }

    /// Process a keyboard event while a modal form dialog is open.
    fn handle_modal_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('u') => {
                    if let Some(modal) = &mut self.modal {
                        modal.clear_active_field();
                    }
                    return;
                }
                KeyCode::Char('a') => {
                    if let Some(modal) = &mut self.modal {
                        modal.move_cursor_home();
                    }
                    return;
                }
                KeyCode::Char('e') => {
                    if let Some(modal) = &mut self.modal {
                        modal.move_cursor_end();
                    }
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => {
                self.modal = None;
            }
            KeyCode::Enter => {
                self.submit_modal();
            }
            KeyCode::Tab | KeyCode::Down => {
                if let Some(modal) = &mut self.modal {
                    modal.next_field();
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(modal) = &mut self.modal {
                    modal.prev_field();
                }
            }
            KeyCode::Left => {
                if let Some(modal) = &mut self.modal {
                    modal.move_cursor_left();
                }
            }
            KeyCode::Right => {
                if let Some(modal) = &mut self.modal {
                    modal.move_cursor_right();
                }
            }
            KeyCode::Home => {
                if let Some(modal) = &mut self.modal {
                    modal.move_cursor_home();
                }
            }
            KeyCode::End => {
                if let Some(modal) = &mut self.modal {
                    modal.move_cursor_end();
                }
            }
            KeyCode::Backspace => {
                if let Some(modal) = &mut self.modal {
                    modal.pop_char();
                }
            }
            KeyCode::Delete => {
                if let Some(modal) = &mut self.modal {
                    modal.delete_char();
                }
            }
            KeyCode::Char(ch) => {
                if let Some(modal) = &mut self.modal {
                    modal.push_char(ch);
                }
            }
            _ => {}
        }
    }

    /// Handle bracketed or multi-character pasted text into the active modal field in $O(n)$ time,
    /// preserving multi-line payload formatting (`\n`, `\t`) on payload fields while normalizing
    /// single-line identifier fields without prematurely submitting on embedded newlines.
    pub fn handle_paste(&mut self, pasted: &str) {
        if let Some(modal) = &mut self.modal {
            modal.paste_str(pasted);
        }
    }

    /// Handle a terminal mouse event using the bounding boxes registered in [`HitMap`].
    pub fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        if self.show_help {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.show_help = false;
                    self.help_scroll = 0;
                }
                MouseEventKind::ScrollUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(2);
                }
                MouseEventKind::ScrollDown => {
                    self.help_scroll = self.help_scroll.saturating_add(2).min(self.help_max_scroll);
                }
                _ => {}
            }
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(target) = self.hit_map.hit_test(mouse.column, mouse.row) else {
                    return;
                };
                match target {
                    ClickTarget::Tab(tab) => {
                        if self.modal.is_none() {
                            self.active_tab = tab;
                            self.focused_toolbar_idx = None;
                        }
                    }
                    ClickTarget::Button(btn) => {
                        if self.modal.is_none() {
                            self.focused_toolbar_idx = None;
                            if let Some(pos) = ButtonId::WORKBENCH.iter().position(|&b| b == btn) {
                                self.selected_workbench_idx = pos;
                                self.commands_focus = CommandsFocus::Workbench;
                            }
                            self.trigger_button(btn);
                        }
                    }
                    ClickTarget::TablePanel => {
                        if self.modal.is_none() {
                            self.focused_toolbar_idx = None;
                        }
                    }
                    ClickTarget::TableRow(row) => {
                        if self.modal.is_none() {
                            self.focused_toolbar_idx = None;
                            self.select_row(row);
                        }
                    }
                    ClickTarget::WorkbenchPanel => {
                        if self.modal.is_none() {
                            self.focused_toolbar_idx = None;
                            self.commands_focus = CommandsFocus::Workbench;
                        }
                    }
                    ClickTarget::LogPanel => {
                        if self.modal.is_none() {
                            self.focused_toolbar_idx = None;
                            self.commands_focus = CommandsFocus::ExecutionLog;
                            if self.selected_log_id.is_none() {
                                self.selected_log_id =
                                    self.action_logs.get(self.selected_log_idx).map(|e| e.id);
                            }
                        }
                    }
                    ClickTarget::LogRow(row) => {
                        if self.modal.is_none() {
                            self.focused_toolbar_idx = None;
                            self.commands_focus = CommandsFocus::ExecutionLog;
                            self.selected_log_idx = row;
                            self.clamp_selections();
                            self.selected_log_id =
                                self.action_logs.get(self.selected_log_idx).map(|e| e.id);
                            self.follow_latest_log = self.selected_log_idx == 0;
                            self.load_selected_log_into_output();
                        }
                    }
                    ClickTarget::OutputPanel => {
                        if self.modal.is_none() {
                            self.focused_toolbar_idx = None;
                            self.commands_focus = CommandsFocus::OutputPanel;
                            if self.selected_log_id.is_none() {
                                self.selected_log_id =
                                    self.action_logs.get(self.selected_log_idx).map(|e| e.id);
                            }
                        }
                    }
                    ClickTarget::SubViewToggle => {
                        if self.modal.is_none() {
                            self.focused_toolbar_idx = None;
                            if self.active_tab == TabId::DhtStorage {
                                self.dht_subtab = self.dht_subtab.next();
                                self.selected_dht_idx = 0;
                                self.dht_table_offset = 0;
                            } else if self.active_tab == TabId::Directory {
                                self.directory_subtab = self.directory_subtab.toggle();
                                self.selected_directory_idx = 0;
                                self.directory_table_offset = 0;
                            }
                        }
                    }
                    ClickTarget::ModalField(idx) => {
                        if let Some(modal) = &mut self.modal {
                            if idx < modal.fields.len() {
                                modal.active_field = idx;
                            }
                        }
                    }
                    ClickTarget::ModalSubmit => {
                        self.submit_modal();
                    }
                    ClickTarget::ModalCancel => {
                        self.modal = None;
                    }
                }
            }
            MouseEventKind::ScrollUp if self.modal.is_none() => {
                self.focused_toolbar_idx = None;
                let hit = self.hit_map.hit_test(mouse.column, mouse.row);
                if self.active_tab == TabId::Commands {
                    match hit {
                        Some(ClickTarget::LogRow(_)) | Some(ClickTarget::LogPanel) => {
                            self.commands_focus = CommandsFocus::ExecutionLog;
                            self.step_log_selection_up(1);
                        }
                        Some(ClickTarget::OutputPanel) => {
                            self.commands_focus = CommandsFocus::OutputPanel;
                            self.output_scroll = self.output_scroll.saturating_sub(2);
                        }
                        _ => {}
                    }
                } else if matches!(
                    hit,
                    Some(ClickTarget::TableRow(_)) | Some(ClickTarget::TablePanel)
                ) {
                    self.move_selection_up();
                }
            }
            MouseEventKind::ScrollDown if self.modal.is_none() => {
                self.focused_toolbar_idx = None;
                let hit = self.hit_map.hit_test(mouse.column, mouse.row);
                if self.active_tab == TabId::Commands {
                    match hit {
                        Some(ClickTarget::LogRow(_)) | Some(ClickTarget::LogPanel) => {
                            self.commands_focus = CommandsFocus::ExecutionLog;
                            self.step_log_selection_down(1);
                        }
                        Some(ClickTarget::OutputPanel) => {
                            self.commands_focus = CommandsFocus::OutputPanel;
                            self.output_scroll = self
                                .output_scroll
                                .saturating_add(2)
                                .min(self.output_max_scroll);
                        }
                        _ => {}
                    }
                } else if matches!(
                    hit,
                    Some(ClickTarget::TableRow(_)) | Some(ClickTarget::TablePanel)
                ) {
                    self.move_selection_down();
                }
            }
            _ => {}
        }
    }

    /// Load the currently selected execution log entry into the command output panel.
    fn load_selected_log_into_output(&mut self) {
        if let Some(entry) = self.action_logs.get(self.selected_log_idx) {
            self.last_command_output = Some(entry.formatted_output());
            self.output_scroll = 0;
        }
    }

    /// Move execution log selection upward by `step` rows and sync the output panel.
    fn step_log_selection_up(&mut self, step: usize) {
        self.selected_log_idx = self.selected_log_idx.saturating_sub(step);
        self.clamp_selections();
        self.selected_log_id = self.action_logs.get(self.selected_log_idx).map(|e| e.id);
        self.follow_latest_log = self.selected_log_idx == 0;
        self.load_selected_log_into_output();
    }

    /// Move execution log selection downward by `step` rows and sync the output panel.
    fn step_log_selection_down(&mut self, step: usize) {
        self.selected_log_idx = self.selected_log_idx.saturating_add(step);
        self.clamp_selections();
        self.selected_log_id = self.action_logs.get(self.selected_log_idx).map(|e| e.id);
        self.follow_latest_log = self.selected_log_idx == 0;
        self.load_selected_log_into_output();
    }

    /// Select table row `row` in the currently active tab and clamp indices.
    fn select_row(&mut self, row: usize) {
        match self.active_tab {
            TabId::Overview | TabId::Monitor => {
                self.selected_event_idx = row;
            }
            TabId::Peers => {
                self.selected_peer_idx = row;
                self.selected_peer_did = None;
            }
            TabId::DhtStorage => {
                self.selected_dht_idx = row;
            }
            TabId::Directory => {
                self.selected_directory_idx = row;
            }
            TabId::Commands => {
                self.commands_focus = CommandsFocus::ExecutionLog;
                self.selected_log_idx = row;
                self.clamp_selections();
                self.selected_log_id = self.action_logs.get(self.selected_log_idx).map(|e| e.id);
                self.follow_latest_log = self.selected_log_idx == 0;
                self.load_selected_log_into_output();
                return;
            }
        }
        self.clamp_selections();
    }

    /// Move the active tab's selection or scroll position upward by one step.
    fn move_selection_up(&mut self) {
        if self.focused_toolbar_idx.is_some() {
            self.focused_toolbar_idx = None;
            return;
        }
        match self.active_tab {
            TabId::Overview | TabId::Monitor => {
                self.selected_event_idx = self.selected_event_idx.saturating_sub(1);
            }
            TabId::Peers => {
                self.selected_peer_idx = self.selected_peer_idx.saturating_sub(1);
                self.selected_peer_did = None;
            }
            TabId::DhtStorage => {
                self.selected_dht_idx = self.selected_dht_idx.saturating_sub(1);
            }
            TabId::Directory => {
                self.selected_directory_idx = self.selected_directory_idx.saturating_sub(1);
            }
            TabId::Commands => match self.commands_focus {
                CommandsFocus::ExecutionLog => {
                    self.step_log_selection_up(1);
                    return;
                }
                CommandsFocus::OutputPanel => {
                    self.output_scroll = self.output_scroll.saturating_sub(1);
                    return;
                }
                CommandsFocus::Workbench => {
                    if self.selected_workbench_idx >= 4 {
                        self.selected_workbench_idx -= 4;
                    }
                }
            },
        }
        self.clamp_selections();
    }

    /// Move the active tab's selection or scroll position downward by one step.
    fn move_selection_down(&mut self) {
        if self.focused_toolbar_idx.is_some() {
            self.focused_toolbar_idx = None;
            return;
        }
        match self.active_tab {
            TabId::Overview | TabId::Monitor => {
                self.selected_event_idx = self.selected_event_idx.saturating_add(1);
            }
            TabId::Peers => {
                self.selected_peer_idx = self.selected_peer_idx.saturating_add(1);
                self.selected_peer_did = None;
            }
            TabId::DhtStorage => {
                self.selected_dht_idx = self.selected_dht_idx.saturating_add(1);
            }
            TabId::Directory => {
                self.selected_directory_idx = self.selected_directory_idx.saturating_add(1);
            }
            TabId::Commands => match self.commands_focus {
                CommandsFocus::ExecutionLog => {
                    self.step_log_selection_down(1);
                    return;
                }
                CommandsFocus::OutputPanel => {
                    self.output_scroll = self
                        .output_scroll
                        .saturating_add(1)
                        .min(self.output_max_scroll);
                    return;
                }
                CommandsFocus::Workbench => {
                    if self.selected_workbench_idx + 4 < ButtonId::WORKBENCH.len() {
                        self.selected_workbench_idx += 4;
                    }
                }
            },
        }
        self.clamp_selections();
    }

    /// Move focus or subtab selection to the left in the active view.
    fn move_selection_left(&mut self) {
        if let Some(idx) = self.focused_toolbar_idx {
            let len = ButtonId::TOOLBAR.len();
            self.focused_toolbar_idx = Some((idx + len - 1) % len);
            return;
        }
        match self.active_tab {
            TabId::DhtStorage => {
                self.dht_subtab = self.dht_subtab.prev();
                self.selected_dht_idx = 0;
                self.dht_table_offset = 0;
            }
            TabId::Directory => {
                self.directory_subtab = self.directory_subtab.toggle();
                self.selected_directory_idx = 0;
                self.directory_table_offset = 0;
            }
            TabId::Commands => match self.commands_focus {
                CommandsFocus::ExecutionLog => {
                    self.commands_focus = CommandsFocus::OutputPanel;
                }
                CommandsFocus::OutputPanel => {
                    self.commands_focus = CommandsFocus::Workbench;
                }
                CommandsFocus::Workbench => {
                    self.selected_workbench_idx = self.selected_workbench_idx.saturating_sub(1);
                }
            },
            _ => {}
        }
    }

    /// Move focus or subtab selection to the right in the active view.
    fn move_selection_right(&mut self) {
        if let Some(idx) = self.focused_toolbar_idx {
            let len = ButtonId::TOOLBAR.len();
            self.focused_toolbar_idx = Some((idx + 1) % len);
            return;
        }
        match self.active_tab {
            TabId::DhtStorage => {
                self.dht_subtab = self.dht_subtab.next();
                self.selected_dht_idx = 0;
                self.dht_table_offset = 0;
            }
            TabId::Directory => {
                self.directory_subtab = self.directory_subtab.toggle();
                self.selected_directory_idx = 0;
                self.directory_table_offset = 0;
            }
            TabId::Commands => match self.commands_focus {
                CommandsFocus::Workbench => {
                    if self.selected_workbench_idx + 1 < ButtonId::WORKBENCH.len() {
                        self.selected_workbench_idx += 1;
                    } else {
                        self.commands_focus = CommandsFocus::OutputPanel;
                    }
                }
                CommandsFocus::OutputPanel => {
                    self.commands_focus = CommandsFocus::ExecutionLog;
                }
                CommandsFocus::ExecutionLog => {}
            },
            _ => {}
        }
    }

    /// Clamp all table selection indices and scroll offsets to current snapshot bounds.
    fn clamp_selections(&mut self) {
        let peer_rows = self.snapshot.peer_traffic_rows();
        let peer_len = peer_rows.len();
        if let Some(prev_did) = &self.selected_peer_did {
            if let Some(pos) = peer_rows
                .iter()
                .position(|r| r.peer_did.eq_ignore_ascii_case(prev_did))
            {
                self.selected_peer_idx = pos;
            }
        }
        if peer_len == 0 {
            self.selected_peer_idx = 0;
            self.peer_table_offset = 0;
            self.selected_peer_did = None;
        } else {
            if self.selected_peer_idx >= peer_len {
                self.selected_peer_idx = peer_len - 1;
            }
            self.peer_table_offset = self.peer_table_offset.min(self.selected_peer_idx);
            self.selected_peer_did = peer_rows
                .get(self.selected_peer_idx)
                .map(|r| r.peer_did.clone());
        }

        let event_len = self
            .snapshot
            .observability
            .as_ref()
            .map(|o| o.recent_messages.len())
            .unwrap_or(0);
        if event_len == 0 {
            self.selected_event_idx = 0;
            self.event_table_offset = 0;
        } else {
            if self.selected_event_idx >= event_len {
                self.selected_event_idx = event_len - 1;
            }
            self.event_table_offset = self.event_table_offset.min(self.selected_event_idx);
        }

        let dht_len = match self.dht_subtab {
            DhtSubtab::FingerTable => self
                .snapshot
                .swarm
                .as_ref()
                .and_then(|s| s.dht.as_ref())
                .map(|d| d.finger_table_ranges.len())
                .unwrap_or(0),
            DhtSubtab::PersistentStorage => self
                .snapshot
                .swarm
                .as_ref()
                .and_then(|s| s.persistence_storage.as_ref())
                .map(|st| st.items.len())
                .unwrap_or(0),
            DhtSubtab::CacheStorage => self
                .snapshot
                .swarm
                .as_ref()
                .and_then(|s| s.cache_storage.as_ref())
                .map(|st| st.items.len())
                .unwrap_or(0),
        };
        if dht_len == 0 {
            self.selected_dht_idx = 0;
            self.dht_table_offset = 0;
        } else {
            if self.selected_dht_idx >= dht_len {
                self.selected_dht_idx = dht_len - 1;
            }
            self.dht_table_offset = self.dht_table_offset.min(self.selected_dht_idx);
        }

        let dir_len = match self.directory_subtab {
            DirectorySubtab::OnlineNodes => self.snapshot.online_nodes.len(),
            DirectorySubtab::OnionExits => self.snapshot.onion_exits.len(),
        };
        if dir_len == 0 {
            self.selected_directory_idx = 0;
            self.directory_table_offset = 0;
        } else {
            if self.selected_directory_idx >= dir_len {
                self.selected_directory_idx = dir_len - 1;
            }
            self.directory_table_offset =
                self.directory_table_offset.min(self.selected_directory_idx);
        }

        let log_len = self.action_logs.len();
        if log_len == 0 {
            self.selected_log_idx = 0;
            self.selected_log_id = None;
            self.log_table_offset = 0;
        } else {
            if self.selected_log_idx >= log_len {
                self.selected_log_idx = log_len - 1;
            }
            self.log_table_offset = self.log_table_offset.min(self.selected_log_idx);
            self.selected_log_id = self.action_logs.get(self.selected_log_idx).map(|e| e.id);
        }
    }
}
