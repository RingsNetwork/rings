//! Interactive buttons, modal command forms, and asynchronous command payloads.

use ratatui::layout::Rect;
use rings_rpc::protos::rings_node::PeerMeasurementInfo;

use crate::types::DhtKeyLookupReport;
use crate::types::NodeSnapshot;

/// Top-level navigation tabs in the TUI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabId {
    /// Node health, delegation TTL, gateway status, and summary cards.
    #[default]
    Overview,
    /// Real-time message sparklines, DHT lookup latency histogram, and recent activity feed.
    Monitor,
    /// Connected peers, transport states, reliability ratings, and byte credits.
    Peers,
    /// Chord ring topology, finger table ranges, and persistent/cache storage browser.
    DhtStorage,
    /// Online-node and onion-exit directory browser.
    Directory,
    /// Interactive command button workbench, PubSub/Service results, and execution log.
    Commands,
}

impl TabId {
    /// All tabs in display order.
    pub const ALL: [Self; 6] = [
        Self::Overview,
        Self::Monitor,
        Self::Peers,
        Self::DhtStorage,
        Self::Directory,
        Self::Commands,
    ];

    /// Short display title including its numeric hotkey.
    pub const fn title(self) -> &'static str {
        match self {
            Self::Overview => "1:Overview",
            Self::Monitor => "2:Monitor",
            Self::Peers => "3:Peers",
            Self::DhtStorage => "4:DHT & Storage",
            Self::Directory => "5:Directory",
            Self::Commands => "6:Commands",
        }
    }

    /// Cycle to the next tab.
    pub const fn next(self) -> Self {
        match self {
            Self::Overview => Self::Monitor,
            Self::Monitor => Self::Peers,
            Self::Peers => Self::DhtStorage,
            Self::DhtStorage => Self::Directory,
            Self::Directory => Self::Commands,
            Self::Commands => Self::Overview,
        }
    }

    /// Cycle to the previous tab.
    pub const fn prev(self) -> Self {
        match self {
            Self::Overview => Self::Commands,
            Self::Monitor => Self::Overview,
            Self::Peers => Self::Monitor,
            Self::DhtStorage => Self::Peers,
            Self::Directory => Self::DhtStorage,
            Self::Commands => Self::Directory,
        }
    }
}

/// Interactive button commands available on the toolbar and the Commands workbench.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonId {
    /// Immediately trigger a full status & telemetry refresh.
    RefreshNow,
    /// Pause or resume background automatic polling.
    ToggleAutoPoll,
    /// Connect to a peer via its HTTP JSON-RPC URL (`connectPeerViaHttp`).
    ConnectUrl,
    /// Connect to a peer by DID over the DHT (`connectWithDid`).
    ConnectDid,
    /// Connect to peers from a seed file or `<did>@<url>` (`connectWithSeed`).
    ConnectSeed,
    /// Disconnect a connected peer (`disconnect`).
    DisconnectPeer,
    /// Create a manual WebRTC handshake offer (`createOffer`).
    CreateOffer,
    /// Answer a manual WebRTC handshake offer (`answerOffer`).
    AnswerOffer,
    /// Accept a manual WebRTC handshake answer (`acceptAnswer`).
    AcceptAnswer,
    /// Send a namespaced backend message (`sendBackendMessage`).
    SendMessage,
    /// Publish a message to a DHT PubSub topic (`publishMessageToTopic`).
    PublishTopic,
    /// Fetch messages from a DHT PubSub topic (`fetchTopicMessages`).
    FetchTopic,
    /// Register a service name on the local node (`registerService`).
    RegisterService,
    /// Look up provider DIDs for a service name (`lookupService`).
    LookupService,
    /// Query a key or DID in the Chord DHT and inspect its routing & stored elements.
    DhtLookupKey,
    /// Query transport & credit measurements for a single peer (`peerMeasurement`).
    InspectPeer,
    /// Toggle inclusion of expired descriptors in the Directory view.
    ToggleExpiredDirectory,
    /// Reload the API Bearer token file from disk.
    ReloadToken,
    /// Open or close the keyboard & mouse help overlay.
    ShowHelp,
    /// Exit the TUI application.
    Quit,
}

impl ButtonId {
    /// Buttons displayed in the persistent quick-action bar across all tabs.
    pub const TOOLBAR: [Self; 11] = [
        Self::RefreshNow,
        Self::ToggleAutoPoll,
        Self::ConnectUrl,
        Self::ConnectDid,
        Self::DisconnectPeer,
        Self::DhtLookupKey,
        Self::SendMessage,
        Self::PublishTopic,
        Self::LookupService,
        Self::ShowHelp,
        Self::Quit,
    ];

    /// All command buttons displayed in the Commands workbench grid.
    pub const WORKBENCH: [Self; 16] = [
        Self::ConnectUrl,
        Self::ConnectDid,
        Self::ConnectSeed,
        Self::DisconnectPeer,
        Self::DhtLookupKey,
        Self::CreateOffer,
        Self::AnswerOffer,
        Self::AcceptAnswer,
        Self::SendMessage,
        Self::PublishTopic,
        Self::FetchTopic,
        Self::RegisterService,
        Self::LookupService,
        Self::InspectPeer,
        Self::ToggleExpiredDirectory,
        Self::ReloadToken,
    ];

    /// Compact button label for the top quick-action toolbar (fits 80+ col terminals).
    pub const fn toolbar_label(self) -> &'static str {
        match self {
            Self::RefreshNow => "[R]Refresh",
            Self::ToggleAutoPoll => "[P]Poll",
            Self::ConnectUrl => "[C]+URL",
            Self::ConnectDid => "[D]+DID",
            Self::ConnectSeed => "[S]+Seed",
            Self::DisconnectPeer => "[X]Drop",
            Self::CreateOffer => "[O]Offer",
            Self::AnswerOffer => "[A]Answer",
            Self::AcceptAnswer => "[Y]Accept",
            Self::SendMessage => "[M]Msg",
            Self::PublishTopic => "[T]Topic",
            Self::FetchTopic => "[F]Fetch",
            Self::RegisterService => "[G]+Svc",
            Self::LookupService => "[V]Svc?",
            Self::DhtLookupKey => "[/ ]DHTKey",
            Self::InspectPeer => "[I]Meas",
            Self::ToggleExpiredDirectory => "[E]Exp",
            Self::ReloadToken => "[U]Token",
            Self::ShowHelp => "[?]Help",
            Self::Quit => "[Q]Quit",
        }
    }

    /// Full button label with hotkey hint used in the Commands workbench grid.
    pub const fn label(self) -> &'static str {
        match self {
            Self::RefreshNow => "[R] Refresh",
            Self::ToggleAutoPoll => "[P] Auto-Poll",
            Self::ConnectUrl => "[C] Connect URL",
            Self::ConnectDid => "[D] Connect DID",
            Self::ConnectSeed => "[S] Connect Seed",
            Self::DisconnectPeer => "[X] Disconnect",
            Self::CreateOffer => "[O] Create Offer",
            Self::AnswerOffer => "[A] Answer Offer",
            Self::AcceptAnswer => "[Y] Accept Answer",
            Self::SendMessage => "[M] Send Msg",
            Self::PublishTopic => "[T] Pub Topic",
            Self::FetchTopic => "[F] Fetch Topic",
            Self::RegisterService => "[G] Reg Service",
            Self::LookupService => "[V] Lookup Svc",
            Self::DhtLookupKey => "[/] DHT Lookup",
            Self::InspectPeer => "[I] Peer Measure",
            Self::ToggleExpiredDirectory => "[E] Expired Dir",
            Self::ReloadToken => "[U] Reload Token",
            Self::ShowHelp => "[?] Help",
            Self::Quit => "[Q] Quit",
        }
    }

    /// Detailed description shown in the Commands workbench and status bar.
    pub const fn description(self) -> &'static str {
        match self {
            Self::RefreshNow => {
                "Poll node status, observability metrics, gateway, and directories now"
            }
            Self::ToggleAutoPoll => "Pause or resume periodic background status polling",
            Self::ConnectUrl => "Dial a peer over HTTP signaling (connectPeerViaHttp)",
            Self::ConnectDid => "Dial a peer by DID routed over the Chord DHT (connectWithDid)",
            Self::ConnectSeed => {
                "Dial bootstrap peers from seed file or <did>@<url> (connectWithSeed)"
            }
            Self::DisconnectPeer => "Close the active transport to a peer DID (disconnect)",
            Self::CreateOffer => "Step 1 of manual WebRTC handshake: create SDP offer for a DID",
            Self::AnswerOffer => "Step 2 of manual WebRTC handshake: answer a peer's encoded offer",
            Self::AcceptAnswer => "Step 3 of manual WebRTC handshake: accept peer's encoded answer",
            Self::SendMessage => "Send a namespaced backend protocol message to a peer DID",
            Self::PublishTopic => "Publish a message to a DHT-backed PubSub topic",
            Self::FetchTopic => "Fetch retained messages from a DHT-backed PubSub topic",
            Self::RegisterService => "Announce the local node as a provider for a named service",
            Self::LookupService => "Resolve provider DIDs registered for a named service",
            Self::DhtLookupKey => {
                "Lookup any key or DID in the Chord DHT and inspect its stored elements"
            }
            Self::InspectPeer => "Query local transport counters and byte credits for a peer DID",
            Self::ToggleExpiredDirectory => {
                "Include or hide expired descriptors in Online/Onion registries"
            }
            Self::ReloadToken => "Reload the API Bearer token from disk and reconnect client",
            Self::ShowHelp => "Display keyboard shortcuts and mouse interaction guide",
            Self::Quit => "Exit rings-tui",
        }
    }
}

/// Editable text input field inside a modal command dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormField {
    /// Human-readable field label.
    pub label: &'static str,
    /// Hint shown when the field is empty.
    pub placeholder: &'static str,
    /// Current user-edited text value.
    pub value: String,
    /// Whether this field is optional.
    pub optional: bool,
}

impl FormField {
    /// Construct a new form field with an initial value.
    pub fn new(
        label: &'static str,
        placeholder: &'static str,
        initial: impl Into<String>,
        optional: bool,
    ) -> Self {
        Self {
            label,
            placeholder,
            value: initial.into(),
            optional,
        }
    }
}

/// Kind of parameterized command modal dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalKind {
    /// `connectPeerViaHttp` dialog.
    ConnectUrl,
    /// `connectWithDid` dialog.
    ConnectDid,
    /// `connectWithSeed` dialog.
    ConnectSeed,
    /// `disconnect` dialog.
    DisconnectPeer,
    /// `createOffer` dialog.
    CreateOffer,
    /// `answerOffer` dialog.
    AnswerOffer,
    /// `acceptAnswer` dialog.
    AcceptAnswer,
    /// `sendBackendMessage` dialog.
    SendMessage,
    /// `publishMessageToTopic` dialog.
    PublishTopic,
    /// `fetchTopicMessages` dialog.
    FetchTopic,
    /// `registerService` dialog.
    RegisterService,
    /// `lookupService` dialog.
    LookupService,
    /// Chord DHT key lookup dialog.
    DhtLookupKey,
    /// `peerMeasurement` dialog.
    InspectPeer,
}

/// Active modal dialog state for entering command arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandModal {
    /// Command type represented by this modal.
    pub kind: ModalKind,
    /// Dialog title.
    pub title: &'static str,
    /// Input fields in tab order.
    pub fields: Vec<FormField>,
    /// Index of the currently focused input field.
    pub active_field: usize,
    /// Inline validation error message, if submission failed validation.
    pub error: Option<String>,
}

impl CommandModal {
    /// Build a modal dialog for `kind`, pre-filling target DID or topic when available.
    pub fn open(kind: ModalKind, default_did: Option<&str>, default_topic: Option<&str>) -> Self {
        let did_prefill = default_did.unwrap_or("");
        let topic_prefill = default_topic.unwrap_or("rings-chat");
        let (title, fields) = match kind {
            ModalKind::ConnectUrl => ("Connect Peer via HTTP URL", vec![
                FormField::new("Peer HTTP URL", "http://127.0.0.1:50001", "", false),
                FormField::new(
                    "Remote API Token (optional)",
                    "Leave empty for public external handshake",
                    "",
                    true,
                ),
            ]),
            ModalKind::ConnectDid => ("Connect Peer via DID (DHT)", vec![FormField::new(
                "Target Peer DID",
                "0x... or did:ring:...",
                did_prefill,
                false,
            )]),
            ModalKind::ConnectSeed => ("Connect Peers from Seed", vec![FormField::new(
                "Seed File Path or <did>@<http-url>",
                "~/.rings/seed.json or 0x1234...@http://127.0.0.1:50001",
                "",
                false,
            )]),
            ModalKind::DisconnectPeer => ("Disconnect Peer", vec![FormField::new(
                "Peer DID to Disconnect",
                "0x...",
                did_prefill,
                false,
            )]),
            ModalKind::CreateOffer => ("Manual Handshake (1/3): Create Offer", vec![
                FormField::new("Target Peer DID", "0x...", did_prefill, false),
            ]),
            ModalKind::AnswerOffer => ("Manual Handshake (2/3): Answer Offer", vec![
                FormField::new(
                    "Encoded Offer Payload (base58)",
                    "Paste offer string from peer...",
                    "",
                    false,
                ),
            ]),
            ModalKind::AcceptAnswer => ("Manual Handshake (3/3): Accept Answer", vec![
                FormField::new(
                    "Encoded Answer Payload (base58)",
                    "Paste answer string from peer...",
                    "",
                    false,
                ),
            ]),
            ModalKind::SendMessage => ("Send Namespaced Backend Message", vec![
                FormField::new("Destination Peer DID", "0x...", did_prefill, false),
                FormField::new("Protocol Namespace", "custom_chat", "default", false),
                FormField::new("Message Payload (UTF-8)", "Hello Rings!", "", false),
            ]),
            ModalKind::PublishTopic => ("Publish Message to DHT Topic", vec![
                FormField::new("Topic Name", "rings-chat", topic_prefill, false),
                FormField::new("Message Content", "Hello topic subscribers!", "", false),
            ]),
            ModalKind::FetchTopic => ("Fetch Messages from DHT Topic", vec![
                FormField::new("Topic Name", "rings-chat", topic_prefill, false),
                FormField::new("Skip Offset", "0", "0", false),
            ]),
            ModalKind::RegisterService => ("Register Local Service in DHT", vec![FormField::new(
                "Service Name",
                "ipfs-provider",
                "",
                false,
            )]),
            ModalKind::LookupService => ("Lookup Service Providers in DHT", vec![FormField::new(
                "Service Name",
                "ipfs-provider",
                "",
                false,
            )]),
            ModalKind::DhtLookupKey => (
                "Chord DHT Key Lookup (Topic / Service / VNode Key / 0x DID)",
                vec![FormField::new(
                    "DHT Key or Target DID",
                    "rings-chat, online_nodes, ipfs-gateway, or 0x...",
                    topic_prefill,
                    false,
                )],
            ),
            ModalKind::InspectPeer => {
                ("Inspect Peer Transport Measurements", vec![FormField::new(
                    "Peer DID",
                    "0x...",
                    did_prefill,
                    false,
                )])
            }
        };

        Self {
            kind,
            title,
            fields,
            active_field: 0,
            error: None,
        }
    }

    /// Move focus to the next field.
    pub fn next_field(&mut self) {
        if !self.fields.is_empty() {
            self.active_field = (self.active_field + 1) % self.fields.len();
        }
    }

    /// Move focus to the previous field.
    pub fn prev_field(&mut self) {
        if !self.fields.is_empty() {
            self.active_field = (self.active_field + self.fields.len() - 1) % self.fields.len();
        }
    }

    /// Append a character to the currently focused field.
    pub fn push_char(&mut self, ch: char) {
        if let Some(field) = self.fields.get_mut(self.active_field) {
            field.value.push(ch);
            self.error = None;
        }
    }

    /// Remove the last character from the currently focused field.
    pub fn pop_char(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_field) {
            field.value.pop();
            self.error = None;
        }
    }

    /// Clear the currently focused field.
    pub fn clear_active_field(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_field) {
            field.value.clear();
            self.error = None;
        }
    }

    /// Validate fields and build the corresponding [`NodeCommand`].
    pub fn build_command(&mut self) -> std::result::Result<NodeCommand, String> {
        for field in &self.fields {
            if !field.optional && field.value.trim().is_empty() {
                let msg = format!("Field '{}' is required", field.label);
                self.error = Some(msg.clone());
                return Err(msg);
            }
        }

        let v0 = self
            .fields
            .first()
            .map(|f| f.value.trim().to_string())
            .unwrap_or_default();
        let v1 = self
            .fields
            .get(1)
            .map(|f| f.value.trim().to_string())
            .unwrap_or_default();
        let v2 = self
            .fields
            .get(2)
            .map(|f| f.value.clone())
            .unwrap_or_default();

        match self.kind {
            ModalKind::ConnectUrl => Ok(NodeCommand::ConnectPeerViaHttp {
                url: v0,
                api_token: (!v1.is_empty()).then_some(v1),
            }),
            ModalKind::ConnectDid => Ok(NodeCommand::ConnectWithDid { did: v0 }),
            ModalKind::ConnectSeed => Ok(NodeCommand::ConnectWithSeed { source: v0 }),
            ModalKind::DisconnectPeer => Ok(NodeCommand::Disconnect { did: v0 }),
            ModalKind::CreateOffer => Ok(NodeCommand::CreateOffer { did: v0 }),
            ModalKind::AnswerOffer => Ok(NodeCommand::AnswerOffer { offer: v0 }),
            ModalKind::AcceptAnswer => Ok(NodeCommand::AcceptAnswer { answer: v0 }),
            ModalKind::SendMessage => Ok(NodeCommand::SendBackendMessage {
                destination_did: v0,
                namespace: v1,
                payload: v2,
            }),
            ModalKind::PublishTopic => Ok(NodeCommand::PublishTopic {
                topic: v0,
                message: self
                    .fields
                    .get(1)
                    .map(|f| f.value.clone())
                    .unwrap_or_default(),
            }),
            ModalKind::FetchTopic => {
                let skip = v1.parse::<i64>().map_err(|_| {
                    let msg = "Skip Offset must be a valid integer".to_string();
                    self.error = Some(msg.clone());
                    msg
                })?;
                Ok(NodeCommand::FetchTopic { topic: v0, skip })
            }
            ModalKind::RegisterService => Ok(NodeCommand::RegisterService { name: v0 }),
            ModalKind::LookupService => Ok(NodeCommand::LookupService { name: v0 }),
            ModalKind::DhtLookupKey => Ok(NodeCommand::DhtLookupKey { key: v0 }),
            ModalKind::InspectPeer => Ok(NodeCommand::InspectPeer { did: v0 }),
        }
    }
}

/// Asynchronous command dispatched to the background node client worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeCommand {
    /// Poll the full node snapshot.
    RefreshSnapshot {
        /// Whether to include expired descriptors in directory queries.
        include_expired_directory: bool,
    },
    /// Connect to a peer over HTTP.
    ConnectPeerViaHttp {
        /// Target HTTP URL.
        url: String,
        /// Optional Bearer token for the remote peer.
        api_token: Option<String>,
    },
    /// Connect to a peer by DID via the DHT.
    ConnectWithDid {
        /// Target peer DID.
        did: String,
    },
    /// Connect to seed peers.
    ConnectWithSeed {
        /// Seed file path, JSON/YAML, or `<did>@<url>`.
        source: String,
    },
    /// Disconnect a peer by DID.
    Disconnect {
        /// Target peer DID.
        did: String,
    },
    /// Create a WebRTC manual offer.
    CreateOffer {
        /// Target peer DID.
        did: String,
    },
    /// Answer a WebRTC manual offer.
    AnswerOffer {
        /// Encoded offer string.
        offer: String,
    },
    /// Accept a WebRTC manual answer.
    AcceptAnswer {
        /// Encoded answer string.
        answer: String,
    },
    /// Send a namespaced backend message.
    SendBackendMessage {
        /// Destination peer DID.
        destination_did: String,
        /// Extension protocol namespace.
        namespace: String,
        /// UTF-8 message payload.
        payload: String,
    },
    /// Publish a message to a DHT topic.
    PublishTopic {
        /// Topic name.
        topic: String,
        /// Message content.
        message: String,
    },
    /// Fetch messages from a DHT topic.
    FetchTopic {
        /// Topic name.
        topic: String,
        /// Starting message index.
        skip: i64,
    },
    /// Register a service name.
    RegisterService {
        /// Service name.
        name: String,
    },
    /// Lookup providers for a service name.
    LookupService {
        /// Service name.
        name: String,
    },
    /// Lookup a key or DID in the Chord DHT and inspect its stored elements.
    DhtLookupKey {
        /// Key string or DID.
        key: String,
    },
    /// Query measurements for a single peer.
    InspectPeer {
        /// Peer DID.
        did: String,
    },
}

impl NodeCommand {
    /// Short human-readable title for the command log.
    pub const fn title(&self) -> &'static str {
        match self {
            Self::RefreshSnapshot { .. } => "RefreshSnapshot",
            Self::ConnectPeerViaHttp { .. } => "connectPeerViaHttp",
            Self::ConnectWithDid { .. } => "connectWithDid",
            Self::ConnectWithSeed { .. } => "connectWithSeed",
            Self::Disconnect { .. } => "disconnect",
            Self::CreateOffer { .. } => "createOffer",
            Self::AnswerOffer { .. } => "answerOffer",
            Self::AcceptAnswer { .. } => "acceptAnswer",
            Self::SendBackendMessage { .. } => "sendBackendMessage",
            Self::PublishTopic { .. } => "publishMessageToTopic",
            Self::FetchTopic { .. } => "fetchTopicMessages",
            Self::RegisterService { .. } => "registerService",
            Self::LookupService { .. } => "lookupService",
            Self::DhtLookupKey { .. } => "dhtLookupKey",
            Self::InspectPeer { .. } => "peerMeasurement",
        }
    }

    /// Return `true` when this command mutates node transport, handshake, topic, or service state
    /// and therefore must be serialized in FIFO order relative to other mutating commands.
    pub const fn is_mutating(&self) -> bool {
        matches!(
            self,
            Self::ConnectPeerViaHttp { .. }
                | Self::ConnectWithDid { .. }
                | Self::ConnectWithSeed { .. }
                | Self::Disconnect { .. }
                | Self::CreateOffer { .. }
                | Self::AnswerOffer { .. }
                | Self::AcceptAnswer { .. }
                | Self::SendBackendMessage { .. }
                | Self::PublishTopic { .. }
                | Self::RegisterService { .. }
        )
    }
}

/// Metadata tagging every asynchronous command and its outcome with the client epoch and monotonic
/// request sequence ID so stale or out-of-order responses can be deterministically rejected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestMeta {
    /// Monotonic client generation counter incremented whenever the HTTP/RPC client is replaced.
    pub client_epoch: u64,
    /// Monotonic request sequence identifier within the TUI session.
    pub request_id: u64,
}

/// Asynchronous command paired with its [`RequestMeta`] envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchedCommand {
    /// Request correlation metadata.
    pub meta: RequestMeta,
    /// Command payload to execute.
    pub command: NodeCommand,
}

/// Asynchronous command outcome paired with the [`RequestMeta`] of the command that produced it.
#[derive(Clone, Debug)]
pub struct DispatchedOutcome {
    /// Request correlation metadata copied from the originating [`DispatchedCommand`].
    pub meta: RequestMeta,
    /// Outcome payload.
    pub outcome: CommandOutcome,
}

/// Result of executing a [`NodeCommand`] against a [`crate::client::NodeClient`].
#[derive(Clone, Debug)]
pub enum CommandOutcome {
    /// Snapshot refresh completed.
    Snapshot(Box<NodeSnapshot>),
    /// Snapshot refresh failed (`include_expired_directory`, error message).
    SnapshotError {
        /// Filter flag that was requested by the refresh command.
        include_expired_directory: bool,
        /// Error explanation.
        error: String,
    },
    /// User-triggered command succeeded with a summary and optional detailed payload.
    CommandSuccess {
        /// Command name.
        command: &'static str,
        /// Short status summary.
        summary: String,
        /// Optional multi-line or copyable output (e.g., offer/answer payload, topic messages, service DIDs).
        output: Option<String>,
        /// Optional updated single-peer measurement.
        measurement: Option<Box<PeerMeasurementInfo>>,
        /// Optional structured DHT key lookup report.
        dht_lookup: Option<Box<DhtKeyLookupReport>>,
        /// Optional service name confirmed registered on the node (`registerService`).
        registered_service: Option<String>,
        /// Execution time in milliseconds.
        duration_ms: u64,
    },
    /// User-triggered command failed.
    CommandError {
        /// Command name.
        command: &'static str,
        /// Error explanation.
        error: String,
        /// Execution time in milliseconds.
        duration_ms: u64,
    },
}

/// Entry recorded in the interactive command execution log.
#[derive(Clone, Debug)]
pub struct ActionLogEntry {
    /// Monotonic sequence number in the TUI session.
    pub id: u64,
    /// Command name.
    pub command: &'static str,
    /// Whether the command succeeded.
    pub succeeded: bool,
    /// Summary message or error text.
    pub message: String,
    /// Optional detailed output payload (such as SDP offer/answer or lookup results).
    pub details: Option<String>,
    /// Execution latency in milliseconds.
    pub duration_ms: u64,
}

/// Clickable screen region registered during rendering for mouse interaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickTarget {
    /// Top navigation tab.
    Tab(TabId),
    /// Toolbar or workbench command button.
    Button(ButtonId),
    /// Row index in the primary table of the active tab.
    TableRow(usize),
    /// Sub-tab toggle in the Directory or DHT view.
    SubViewToggle,
    /// Input field inside an open modal dialog.
    ModalField(usize),
    /// Submit button inside an open modal dialog.
    ModalSubmit,
    /// Cancel button inside an open modal dialog.
    ModalCancel,
}

/// Map of bounding rectangles to [`ClickTarget`]s populated on each frame render.
#[derive(Clone, Debug, Default)]
pub struct HitMap {
    regions: Vec<(Rect, ClickTarget)>,
}

impl HitMap {
    /// Clear all recorded regions before rendering a new frame.
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    /// Register a clickable rectangular region.
    pub fn register(&mut self, rect: Rect, target: ClickTarget) {
        self.regions.push((rect, target));
    }

    /// Look up the topmost [`ClickTarget`] containing terminal coordinates `(col, row)`.
    pub fn hit_test(&self, col: u16, row: u16) -> Option<ClickTarget> {
        self.regions
            .iter()
            .rev()
            .find(|(rect, _)| {
                col >= rect.x
                    && col < rect.x.saturating_add(rect.width)
                    && row >= rect.y
                    && row < rect.y.saturating_add(rect.height)
            })
            .map(|(_, target)| *target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_validation_and_command_building_work() {
        let mut modal = CommandModal::open(ModalKind::ConnectUrl, None, None);
        assert!(modal.build_command().is_err());
        assert!(modal.error.is_some());

        for ch in "http://127.0.0.1:50001".chars() {
            modal.push_char(ch);
        }
        let cmd = modal.build_command().expect("valid connect url command");
        assert_eq!(cmd, NodeCommand::ConnectPeerViaHttp {
            url: "http://127.0.0.1:50001".to_string(),
            api_token: None,
        });
    }

    #[test]
    fn hit_map_resolves_registered_rectangles() {
        let mut hits = HitMap::default();
        hits.register(
            Rect {
                x: 2,
                y: 1,
                width: 10,
                height: 3,
            },
            ClickTarget::Button(ButtonId::RefreshNow),
        );
        assert_eq!(
            hits.hit_test(5, 2),
            Some(ClickTarget::Button(ButtonId::RefreshNow))
        );
        assert_eq!(hits.hit_test(0, 0), None);
    }
}
