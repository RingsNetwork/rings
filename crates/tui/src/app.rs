//! Core application state machine, keyboard/mouse event handling, and command execution.

use std::collections::VecDeque;
use std::time::Instant;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;

use crate::action::ActionLogEntry;
use crate::action::ButtonId;
use crate::action::ClickTarget;
use crate::action::CommandModal;
use crate::action::CommandOutcome;
use crate::action::HitMap;
use crate::action::ModalKind;
use crate::action::NodeCommand;
use crate::action::TabId;
use crate::client::NodeClient;
use crate::config::ResolvedTuiConfig;
use crate::types::DhtKeyLookupReport;
use crate::types::NodeSnapshot;
use crate::types::TelemetryHistory;

/// Maximum number of command execution logs retained in memory.
pub const MAX_ACTION_LOGS: usize = 100;

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
    /// Selected row index in the Recent Message Events table.
    pub selected_event_idx: usize,
    /// Selected row index in the DHT / Storage table.
    pub selected_dht_idx: usize,
    /// Selected row index in the Directory table.
    pub selected_directory_idx: usize,
    /// Selected button index in the `6:Commands` workbench grid.
    pub selected_workbench_idx: usize,
    /// Selected row index in the `6:Commands` execution log table.
    pub selected_log_idx: usize,
    /// Optional keyboard focus on the persistent toolbar buttons.
    pub focused_toolbar_idx: Option<usize>,
    /// Active modal input dialog, when open.
    pub modal: Option<CommandModal>,
    /// Whether the help overlay is open.
    pub show_help: bool,
    /// Number of asynchronous commands currently executing.
    pub in_flight_commands: usize,
    /// Latest multi-line command output (e.g. SDP offer/answer, topic messages, service DIDs).
    pub last_command_output: Option<String>,
    /// Latest Chord DHT key lookup report.
    pub last_dht_lookup: Option<DhtKeyLookupReport>,
    /// Services registered by this node during this session (or configured).
    pub registered_services: Vec<String>,
    /// Last topic name used in PubSub dialogs for convenience.
    pub last_topic_name: Option<String>,
    /// Bounded history of executed button commands.
    pub action_logs: VecDeque<ActionLogEntry>,
    /// Next sequence ID for action logs.
    next_log_id: u64,
    /// Screen hit-test map populated during rendering for mouse clicks.
    pub hit_map: HitMap,
    /// Whether the user has requested to exit the TUI.
    pub should_quit: bool,
    /// Flag Set when the user requests reloading the API token from disk.
    pub pending_token_reload: bool,
    /// Queue of commands ready to be sent to the async node worker.
    pending_commands: VecDeque<NodeCommand>,
}

impl App {
    /// Construct a new [`App`] from a resolved TUI configuration and queue an initial refresh.
    pub fn new(config: ResolvedTuiConfig) -> Self {
        let mut pending_commands = VecDeque::new();
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
            selected_event_idx: 0,
            selected_dht_idx: 0,
            selected_directory_idx: 0,
            selected_workbench_idx: 0,
            selected_log_idx: 0,
            focused_toolbar_idx: None,
            modal: None,
            show_help: false,
            in_flight_commands: 0,
            last_command_output: None,
            last_dht_lookup: None,
            registered_services,
            last_topic_name: None,
            action_logs: VecDeque::with_capacity(MAX_ACTION_LOGS),
            next_log_id: 1,
            hit_map: HitMap::default(),
            should_quit: false,
            pending_token_reload: false,
            pending_commands,
        }
    }

    /// Drain the next queued [`NodeCommand`] to be dispatched to the worker task.
    pub fn pop_pending_command(&mut self) -> Option<NodeCommand> {
        let cmd = self.pending_commands.pop_front()?;
        self.in_flight_commands = self.in_flight_commands.saturating_add(1);
        Some(cmd)
    }

    /// Queue a background snapshot refresh if auto-polling is enabled and no command is in flight.
    pub fn on_tick(&mut self) {
        if self.auto_poll && self.in_flight_commands == 0 && self.pending_commands.is_empty() {
            self.pending_commands
                .push_back(NodeCommand::RefreshSnapshot {
                    include_expired_directory: self.include_expired_directory,
                });
        }
    }

    /// Apply a completed [`CommandOutcome`] from the worker task to the application state.
    pub fn apply_outcome(&mut self, outcome: CommandOutcome) {
        self.in_flight_commands = self.in_flight_commands.saturating_sub(1);
        match outcome {
            CommandOutcome::Snapshot(snapshot) => {
                self.connection_state = ConnectionState::Online;
                self.history.record(&snapshot);
                self.snapshot = *snapshot;
                self.clamp_selections();
            }
            CommandOutcome::SnapshotError(err) => {
                self.connection_state = ConnectionState::Offline(err);
            }
            CommandOutcome::CommandSuccess {
                command,
                summary,
                output,
                measurement,
                dht_lookup,
                duration_ms,
            } => {
                if let Some(meas) = measurement {
                    if let Some(existing) = self
                        .snapshot
                        .measurements
                        .iter_mut()
                        .find(|m| m.did == meas.did)
                    {
                        *existing = *meas;
                    } else {
                        self.snapshot.measurements.push(*meas);
                    }
                }
                if let Some(lookup) = dht_lookup {
                    self.last_dht_lookup = Some(*lookup);
                }
                if let Some(out) = &output {
                    self.last_command_output = Some(out.clone());
                }
                self.push_log(command, true, summary, output, duration_ms);
                // Automatically queue a fresh snapshot after mutating node state.
                self.pending_commands
                    .push_back(NodeCommand::RefreshSnapshot {
                        include_expired_directory: self.include_expired_directory,
                    });
            }
            CommandOutcome::CommandError {
                command,
                error,
                duration_ms,
            } => {
                self.push_log(command, false, error, None, duration_ms);
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
        let id = self.next_log_id;
        self.next_log_id = self.next_log_id.saturating_add(1);
        if self.action_logs.len() >= MAX_ACTION_LOGS {
            self.action_logs.pop_back();
        }
        self.action_logs.push_front(ActionLogEntry {
            id,
            command,
            succeeded,
            message,
            details,
            duration_ms,
        });
        self.selected_log_idx = 0;
    }

    /// Return the DID of the currently selected peer (or directory entry) to pre-fill command modals.
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
            _ => self
                .snapshot
                .peers
                .get(self.selected_peer_idx)
                .map(|p| p.did.as_str())
                .or_else(|| {
                    self.snapshot
                        .measurements
                        .get(self.selected_peer_idx)
                        .map(|m| m.did.as_str())
                }),
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
                self.pending_commands
                    .push_back(NodeCommand::RefreshSnapshot {
                        include_expired_directory: self.include_expired_directory,
                    });
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
                self.pending_commands
                    .push_back(NodeCommand::RefreshSnapshot {
                        include_expired_directory: self.include_expired_directory,
                    });
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
            }
            ButtonId::Quit => {
                self.should_quit = true;
            }
        }
    }

    /// Submit the currently open modal dialog if its inputs are valid.
    pub fn submit_modal(&mut self) {
        let Some(modal) = &mut self.modal else {
            return;
        };
        if let Ok(cmd) = modal.build_command() {
            match &cmd {
                NodeCommand::PublishTopic { topic, .. } | NodeCommand::FetchTopic { topic, .. } => {
                    self.last_topic_name = Some(topic.clone());
                }
                NodeCommand::DhtLookupKey { key } => {
                    self.last_topic_name = Some(key.clone());
                }
                NodeCommand::RegisterService { name }
                    if !self.registered_services.contains(name) =>
                {
                    self.registered_services.push(name.clone());
                }
                _ => {}
            }
            self.modal = None;
            self.pending_commands.push_back(cmd);
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
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => {
                    self.show_help = false;
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
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('1') => self.active_tab = TabId::Overview,
            KeyCode::Char('2') => self.active_tab = TabId::Monitor,
            KeyCode::Char('3') => self.active_tab = TabId::Peers,
            KeyCode::Char('4') => self.active_tab = TabId::DhtStorage,
            KeyCode::Char('5') => self.active_tab = TabId::Directory,
            KeyCode::Char('6') => self.active_tab = TabId::Commands,
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
                } else if self.active_tab == TabId::Directory {
                    self.directory_subtab = self.directory_subtab.toggle();
                    self.selected_directory_idx = 0;
                } else if self.active_tab == TabId::Commands {
                    if let Some(&btn) = ButtonId::WORKBENCH.get(self.selected_workbench_idx) {
                        self.trigger_button(btn);
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(idx) = self.focused_toolbar_idx {
                    if let Some(&btn) = ButtonId::TOOLBAR.get(idx) {
                        self.trigger_button(btn);
                    }
                } else if self.active_tab == TabId::Commands {
                    if let Some(&btn) = ButtonId::WORKBENCH.get(self.selected_workbench_idx) {
                        self.trigger_button(btn);
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
            }
            _ => {}
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            if let Some(modal) = &mut self.modal {
                modal.clear_active_field();
            }
            return;
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
            KeyCode::Backspace => {
                if let Some(modal) = &mut self.modal {
                    modal.pop_char();
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

    /// Handle a terminal mouse event using the bounding boxes registered in [`HitMap`].
    pub fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.show_help {
                    self.show_help = false;
                    return;
                }
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
                            if let Some(pos) = ButtonId::WORKBENCH.iter().position(|&b| b == btn) {
                                self.selected_workbench_idx = pos;
                            }
                            self.trigger_button(btn);
                        }
                    }
                    ClickTarget::TableRow(row) => {
                        if self.modal.is_none() {
                            self.select_row(row);
                        }
                    }
                    ClickTarget::SubViewToggle => {
                        if self.modal.is_none() {
                            if self.active_tab == TabId::DhtStorage {
                                self.dht_subtab = self.dht_subtab.next();
                                self.selected_dht_idx = 0;
                            } else if self.active_tab == TabId::Directory {
                                self.directory_subtab = self.directory_subtab.toggle();
                                self.selected_directory_idx = 0;
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
                self.move_selection_up();
            }
            MouseEventKind::ScrollDown if self.modal.is_none() => {
                self.move_selection_down();
            }
            _ => {}
        }
    }

    fn select_row(&mut self, row: usize) {
        match self.active_tab {
            TabId::Overview | TabId::Monitor => {
                self.selected_event_idx = row;
            }
            TabId::Peers => {
                self.selected_peer_idx = row;
            }
            TabId::DhtStorage => {
                self.selected_dht_idx = row;
            }
            TabId::Directory => {
                self.selected_directory_idx = row;
            }
            TabId::Commands => {
                self.selected_log_idx = row;
            }
        }
        self.clamp_selections();
    }

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
            }
            TabId::DhtStorage => {
                self.selected_dht_idx = self.selected_dht_idx.saturating_sub(1);
            }
            TabId::Directory => {
                self.selected_directory_idx = self.selected_directory_idx.saturating_sub(1);
            }
            TabId::Commands => {
                self.selected_workbench_idx = self.selected_workbench_idx.saturating_sub(4);
            }
        }
        self.clamp_selections();
    }

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
            }
            TabId::DhtStorage => {
                self.selected_dht_idx = self.selected_dht_idx.saturating_add(1);
            }
            TabId::Directory => {
                self.selected_directory_idx = self.selected_directory_idx.saturating_add(1);
            }
            TabId::Commands => {
                if self.selected_workbench_idx + 4 < ButtonId::WORKBENCH.len() {
                    self.selected_workbench_idx += 4;
                }
            }
        }
        self.clamp_selections();
    }

    fn move_selection_left(&mut self) {
        if let Some(idx) = self.focused_toolbar_idx {
            let len = ButtonId::TOOLBAR.len();
            self.focused_toolbar_idx = Some((idx + len - 1) % len);
            return;
        }
        match self.active_tab {
            TabId::DhtStorage => {
                self.dht_subtab = self.dht_subtab.next();
                self.selected_dht_idx = 0;
            }
            TabId::Directory => {
                self.directory_subtab = self.directory_subtab.toggle();
                self.selected_directory_idx = 0;
            }
            TabId::Commands => {
                self.selected_workbench_idx = self.selected_workbench_idx.saturating_sub(1);
            }
            _ => {}
        }
    }

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
            }
            TabId::Directory => {
                self.directory_subtab = self.directory_subtab.toggle();
                self.selected_directory_idx = 0;
            }
            TabId::Commands if self.selected_workbench_idx + 1 < ButtonId::WORKBENCH.len() => {
                self.selected_workbench_idx += 1;
            }
            _ => {}
        }
    }

    fn clamp_selections(&mut self) {
        let peer_len = self
            .snapshot
            .peers
            .len()
            .max(self.snapshot.measurements.len());
        if peer_len == 0 {
            self.selected_peer_idx = 0;
        } else if self.selected_peer_idx >= peer_len {
            self.selected_peer_idx = peer_len - 1;
        }

        let event_len = self
            .snapshot
            .observability
            .as_ref()
            .map(|o| o.recent_messages.len())
            .unwrap_or(0);
        if event_len == 0 {
            self.selected_event_idx = 0;
        } else if self.selected_event_idx >= event_len {
            self.selected_event_idx = event_len - 1;
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
        } else if self.selected_dht_idx >= dht_len {
            self.selected_dht_idx = dht_len - 1;
        }

        let dir_len = match self.directory_subtab {
            DirectorySubtab::OnlineNodes => self.snapshot.online_nodes.len(),
            DirectorySubtab::OnionExits => self.snapshot.onion_exits.len(),
        };
        if dir_len == 0 {
            self.selected_directory_idx = 0;
        } else if self.selected_directory_idx >= dir_len {
            self.selected_directory_idx = dir_len - 1;
        }

        let log_len = self.action_logs.len();
        if log_len == 0 {
            self.selected_log_idx = 0;
        } else if self.selected_log_idx >= log_len {
            self.selected_log_idx = log_len - 1;
        }
    }
}

/// Execute a single [`NodeCommand`] against `client` and package its [`CommandOutcome`].
pub async fn execute_node_command(client: &dyn NodeClient, cmd: NodeCommand) -> CommandOutcome {
    let started = Instant::now();
    let command_name = cmd.title();

    match cmd {
        NodeCommand::RefreshSnapshot {
            include_expired_directory,
        } => match client.fetch_snapshot(include_expired_directory).await {
            Ok(snap) => CommandOutcome::Snapshot(Box::new(snap)),
            Err(err) => CommandOutcome::SnapshotError(err.to_string()),
        },
        NodeCommand::ConnectPeerViaHttp { url, api_token } => {
            match client.connect_peer_via_http(&url, api_token).await {
                Ok(did) => CommandOutcome::CommandSuccess {
                    command: command_name,
                    summary: format!("Connected to {url} -> Peer DID: {did}"),
                    output: Some(format!("Remote Peer DID: {did}\nEndpoint: {url}")),
                    measurement: None,
                    dht_lookup: None,
                    duration_ms: elapsed_ms(started),
                },
                Err(err) => CommandOutcome::CommandError {
                    command: command_name,
                    error: err.to_string(),
                    duration_ms: elapsed_ms(started),
                },
            }
        }
        NodeCommand::ConnectWithDid { did } => match client.connect_with_did(&did).await {
            Ok(()) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("Dialed peer DID {did} via DHT"),
                output: None,
                measurement: None,
                dht_lookup: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::ConnectWithSeed { source } => match client.connect_with_seed(&source).await {
            Ok(count) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("Connected seed ({count} bootstrap peer(s)) from {source}"),
                output: None,
                measurement: None,
                dht_lookup: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::Disconnect { did } => match client.disconnect(&did).await {
            Ok(()) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("Disconnected peer {did}"),
                output: None,
                measurement: None,
                dht_lookup: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::CreateOffer { did } => match client.create_offer(&did).await {
            Ok(offer) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("Created WebRTC offer for {did} ({} chars)", offer.len()),
                output: Some(format!(
                    "=== WebRTC Offer for {did} ===\n{offer}\n\nNext step: pass this offer to peer's Answer Offer ([A])."
                )),
                measurement: None,
                dht_lookup: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::AnswerOffer { offer } => match client.answer_offer(&offer).await {
            Ok(answer) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("Generated handshake reply ({} chars)", answer.len()),
                output: Some(format!(
                    "=== WebRTC Answer / Reply Payload ===\n{answer}\n\nNext step: return this payload to offerer's Accept Answer ([Y])."
                )),
                measurement: None,
                dht_lookup: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::AcceptAnswer { answer } => match client.accept_answer(&answer).await {
            Ok(()) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: "Accepted WebRTC answer; manual handshake completed!".to_string(),
                output: Some("Manual WebRTC handshake completed.".to_string()),
                measurement: None,
                dht_lookup: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::SendBackendMessage {
            destination_did,
            namespace,
            payload,
        } => match client
            .send_backend_message(&destination_did, &namespace, &payload)
            .await
        {
            Ok(()) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!(
                    "Sent {} bytes to {destination_did} [ns: {namespace}]",
                    payload.len()
                ),
                output: Some(format!(
                    "To: {destination_did}\nNamespace: {namespace}\nPayload: {payload}"
                )),
                measurement: None,
                dht_lookup: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::PublishTopic { topic, message } => {
            match client.publish_topic_message(&topic, &message).await {
                Ok(()) => CommandOutcome::CommandSuccess {
                    command: command_name,
                    summary: format!("Published message to topic '{topic}'"),
                    output: Some(format!("Topic: {topic}\nPublished: {message}")),
                    measurement: None,
                    dht_lookup: None,
                    duration_ms: elapsed_ms(started),
                },
                Err(err) => CommandOutcome::CommandError {
                    command: command_name,
                    error: err.to_string(),
                    duration_ms: elapsed_ms(started),
                },
            }
        }
        NodeCommand::FetchTopic { topic, skip } => {
            match client.fetch_topic_messages(&topic, skip).await {
                Ok(messages) => {
                    let body = if messages.is_empty() {
                        format!("Topic '{topic}' (skip={skip}): <no messages>")
                    } else {
                        let lines = messages
                            .iter()
                            .enumerate()
                            .map(|(idx, m)| format!("[{}] {}", skip.saturating_add(idx as i64), m))
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("=== Topic '{topic}' ({} messages) ===\n{lines}", messages.len())
                    };
                    CommandOutcome::CommandSuccess {
                        command: command_name,
                        summary: format!(
                            "Fetched {} message(s) from topic '{topic}' (skip={skip})",
                            messages.len()
                        ),
                        output: Some(body),
                        measurement: None,
                        dht_lookup: None,
                        duration_ms: elapsed_ms(started),
                    }
                }
                Err(err) => CommandOutcome::CommandError {
                    command: command_name,
                    error: err.to_string(),
                    duration_ms: elapsed_ms(started),
                },
            }
        }
        NodeCommand::RegisterService { name } => match client.register_service(&name).await {
            Ok(()) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("Registered local service '{name}' in DHT"),
                output: Some(format!("Service registered: {name}")),
                measurement: None,
                dht_lookup: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::LookupService { name } => match client.lookup_service(&name).await {
            Ok(dids) => {
                let body = if dids.is_empty() {
                    format!("Service '{name}': no provider DIDs found")
                } else {
                    format!(
                        "=== Providers for Service '{name}' ({}) ===\n{}",
                        dids.len(),
                        dids.join("\n")
                    )
                };
                CommandOutcome::CommandSuccess {
                    command: command_name,
                    summary: format!("Service '{name}' resolved to {} provider(s)", dids.len()),
                    output: Some(body),
                    measurement: None,
                    dht_lookup: None,
                    duration_ms: elapsed_ms(started),
                }
            }
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::DhtLookupKey { key } => match client.dht_lookup_key(&key).await {
            Ok(report) => {
                let sources_str = if report.sources.is_empty() {
                    "Not Found".to_string()
                } else {
                    report.sources.join(" + ")
                };
                let elems_body = if report.elements.is_empty() {
                    "<no elements stored>".to_string()
                } else {
                    report
                        .elements
                        .iter()
                        .enumerate()
                        .map(|(idx, el)| format!("[{idx}] {el}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let body = format!(
                    "=== DHT Lookup Report ===\nQuery Key: {}\nTarget Chord DID: {}\nNext-Hop / Route Peer: {}\nFound In: {}\nKind: {} ({} elements)\n\n{}",
                    report.query_key,
                    report.target_did,
                    report.route_peer.as_deref().unwrap_or("Local / Self"),
                    sources_str,
                    report.record_kind,
                    report.elements.len(),
                    elems_body
                );
                CommandOutcome::CommandSuccess {
                    command: command_name,
                    summary: format!(
                        "DHT Key '{}' -> {} ({} elements, [{}])",
                        report.query_key,
                        report.target_did,
                        report.elements.len(),
                        sources_str
                    ),
                    output: Some(body),
                    measurement: None,
                    dht_lookup: Some(Box::new(report)),
                    duration_ms: elapsed_ms(started),
                }
            }
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::InspectPeer { did } => match client.peer_measurement(&did).await {
            Ok(Some(meas)) => {
                let credit_line = match &meas.credit {
                    Some(c) => format!(
                        "Score: {:.2} | Bytes TX: {} | Bytes RX: {} | Last Seen: {}s",
                        c.score, c.bytes_sent_to_peer, c.bytes_received_from_peer, c.last_seen_seconds
                    ),
                    None => "No persistent byte credit record".to_string(),
                };
                let details = format!(
                    "=== Peer Measurement: {} ===\nReliability: {:?}\nConnected: {} | Disconnected: {}\nSent: {} (failed: {}) | Received: {} (failed: {})\n{}",
                    meas.did,
                    meas.reliability,
                    meas.counters.connected,
                    meas.counters.disconnected,
                    meas.counters.sent,
                    meas.counters.failed_to_send,
                    meas.counters.received,
                    meas.counters.failed_to_receive,
                    credit_line
                );
                CommandOutcome::CommandSuccess {
                    command: command_name,
                    summary: format!(
                        "Peer {did}: {:?}, sent={}, recv={}",
                        meas.reliability, meas.counters.sent, meas.counters.received
                    ),
                    output: Some(details),
                    measurement: Some(Box::new(meas)),
                    dht_lookup: None,
                    duration_ms: elapsed_ms(started),
                }
            }
            Ok(None) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("No measurement record found for peer {did}"),
                output: Some(format!("No local measurement record for {did}")),
                measurement: None,
                dht_lookup: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
