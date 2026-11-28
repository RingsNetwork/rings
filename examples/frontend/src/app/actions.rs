//! Node start and disconnect action handlers for the frontend shell.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use futures::FutureExt;
use gloo_timers::future::sleep;
use wasm_bindgen::JsValue;
use web_sys::Event;
use web_sys::MouseEvent;
use yew::prelude::*;

use super::CustomState;
use super::LinkState;
use super::NodeState;
use super::ShellState;
use crate::controls::ActiveDialog;
use crate::controls::LaunchActions;
use crate::custom;
use crate::dweb;
use crate::extension;
use crate::generation::GenerationClock;
use crate::generation::GenerationToken;
use crate::node;
use crate::node::DemoNode;
use crate::node::PeerView;
use crate::peer_sync;
use crate::wallet;
use crate::wallet::WalletAccount;
use crate::wallet::WalletKind;
use crate::webview;

/// State handles and references needed to start a local or extension node.
#[derive(Clone)]
struct StartAction {
    /// Selected wallet standard for signing in.
    wallet_kind: UseStateHandle<WalletKind>,
    /// Connected wallet account metadata, if authenticated.
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    /// Whether a node startup sequence is currently in progress.
    node_starting: UseStateHandle<bool>,
    /// Shared handle to the running in-page demo node, if any.
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    /// Generation clock used to discard stale async callbacks.
    generation: GenerationClock,
    /// In-memory dweb site content served by the local node.
    site: dweb::Site,
    /// Local node DID once started.
    did: UseStateHandle<String>,
    /// Human-readable status message displayed in the UI.
    status: UseStateHandle<String>,
    /// Current list of connected peers.
    peers: UseStateHandle<Vec<PeerView>>,
    /// Configured Rings network identifier.
    network_id: UseStateHandle<String>,
    /// Configured ICE server URLs.
    ice_servers: UseStateHandle<String>,
    /// Configured DHT stabilization interval in seconds.
    stabilize_interval: UseStateHandle<String>,
    /// Configured browser storage namespace.
    storage_name: UseStateHandle<String>,
    /// Optional HTTP seed endpoint URL to connect on startup.
    seed_url: UseStateHandle<String>,
    /// Recent custom protocol events received by the node.
    custom_events: UseStateHandle<Vec<custom::CustomEvent>>,
    /// Currently open shell dialog.
    active_dialog: UseStateHandle<ActiveDialog>,
    /// Whether the browser WebView gateway is registered and ready.
    webview_ready: UseStateHandle<bool>,
}

/// Snapshot of user-configured parameters captured when starting a node.
struct StartRequest {
    /// Selected wallet standard.
    kind: WalletKind,
    /// Rings network identifier string.
    network_id: String,
    /// ICE server list string.
    ice_servers: String,
    /// Stabilization interval string in seconds.
    stabilize_interval: String,
    /// Browser storage database name.
    storage_name: String,
    /// Trimmed seed HTTP endpoint URL.
    seed_url: String,
}

/// State handles and references needed to stop a node and reset session state.
#[derive(Clone)]
struct DisconnectAction {
    /// Connected wallet account metadata handle.
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    /// Node startup-in-progress flag handle.
    node_starting: UseStateHandle<bool>,
    /// Shared handle to the running in-page demo node.
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    /// Generation clock bumped to invalidate in-flight tasks.
    generation: GenerationClock,
    /// Local node DID handle.
    did: UseStateHandle<String>,
    /// Human-readable status message handle.
    status: UseStateHandle<String>,
    /// Connected peer list handle.
    peers: UseStateHandle<Vec<PeerView>>,
    /// Locally generated SDP offer handle.
    generated_offer: UseStateHandle<String>,
    /// Remote SDP offer input handle.
    remote_offer: UseStateHandle<String>,
    /// Locally generated SDP answer handle.
    generated_answer: UseStateHandle<String>,
    /// Remote SDP answer input handle.
    remote_answer: UseStateHandle<String>,
    /// Peer link dialog visibility handle.
    link_dialog_open: UseStateHandle<bool>,
    /// Currently open shell dialog handle.
    active_dialog: UseStateHandle<ActiveDialog>,
    /// WebView gateway readiness handle.
    webview_ready: UseStateHandle<bool>,
    /// Registered custom protocol namespaces handle.
    custom_registered: UseStateHandle<Vec<String>>,
    /// Received custom protocol event log handle.
    custom_events: UseStateHandle<Vec<custom::CustomEvent>>,
}

/// Builds the wallet-selection, start, and disconnect callbacks for the control sidebar.
pub(super) fn launch_actions(
    node: &NodeState,
    link: &LinkState,
    custom_state: &CustomState,
    shell: &ShellState,
    on_wallet_kind: Callback<Event>,
) -> LaunchActions {
    let on_start = StartAction {
        wallet_kind: node.wallet_kind.clone(),
        wallet_account: node.wallet_account.clone(),
        node_starting: node.node_starting.clone(),
        node_ref: node.node_ref.clone(),
        generation: node.generation.clone(),
        site: node.site.clone(),
        did: node.did.clone(),
        status: node.status.clone(),
        peers: node.peers.clone(),
        network_id: node.network_id.clone(),
        ice_servers: node.ice_servers.clone(),
        stabilize_interval: node.stabilize_interval.clone(),
        storage_name: node.storage_name.clone(),
        seed_url: node.seed_url.clone(),
        webview_ready: node.webview_ready.clone(),
        custom_events: custom_state.events.clone(),
        active_dialog: shell.active_dialog.clone(),
    }
    .callback();
    let on_disconnect = DisconnectAction {
        wallet_account: node.wallet_account.clone(),
        node_starting: node.node_starting.clone(),
        node_ref: node.node_ref.clone(),
        generation: node.generation.clone(),
        did: node.did.clone(),
        status: node.status.clone(),
        peers: node.peers.clone(),
        generated_offer: link.generated_offer.clone(),
        remote_offer: link.remote_offer.clone(),
        generated_answer: link.generated_answer.clone(),
        remote_answer: link.remote_answer.clone(),
        link_dialog_open: link.link_dialog_open.clone(),
        active_dialog: shell.active_dialog.clone(),
        webview_ready: node.webview_ready.clone(),
        custom_registered: custom_state.registered.clone(),
        custom_events: custom_state.events.clone(),
    }
    .callback();
    LaunchActions {
        on_wallet_kind,
        on_start,
        on_disconnect,
    }
}

impl StartAction {
    /// Creates a click callback that initiates node startup.
    fn callback(self) -> Callback<MouseEvent> {
        Callback::from(move |_| {
            let action = self.clone();
            let request = action.request();
            let start_token = action.generation.bump();
            action.node_starting.set(true);
            action.webview_ready.set(false);
            action
                .status
                .set(format!("connecting {}", request.kind.label()));
            wasm_bindgen_futures::spawn_local(async move {
                action.start(request, start_token).await;
            });
        })
    }

    /// Captures the current form settings into a [`StartRequest`].
    fn request(&self) -> StartRequest {
        StartRequest {
            kind: *self.wallet_kind,
            network_id: (*self.network_id).clone(),
            ice_servers: (*self.ice_servers).clone(),
            stabilize_interval: (*self.stabilize_interval).clone(),
            storage_name: (*self.storage_name).clone(),
            seed_url: (*self.seed_url).trim().to_string(),
        }
    }

    /// Dispatches startup to either the browser extension bridge or a local in-page node.
    async fn start(self, request: StartRequest, token: GenerationToken) {
        if let Some(bridge) = extension::extension_node_bridge() {
            self.start_extension_node(&bridge, request, token).await;
        } else {
            self.start_local_node(request, token).await;
        }
    }

    /// Starts the background node hosted by the browser extension and polls until ready.
    async fn start_extension_node(
        self,
        bridge: &JsValue,
        request: StartRequest,
        token: GenerationToken,
    ) {
        let settings = extension::ExtensionStartSettings {
            network_id: request.network_id,
            ice_servers: request.ice_servers,
            stabilize_interval: request.stabilize_interval,
            storage_name: request.storage_name,
            seed_url: request.seed_url,
        };
        match extension::extension_node_start(bridge, request.kind, settings).await {
            Ok(snapshot) => {
                *self.node_ref.borrow_mut() = None;
                super::routes::clear_shell_dialog_route();
                self.active_dialog.set(ActiveDialog::None);
                if self.apply_extension_snapshot(snapshot, &token) {
                    self.poll_extension_node_start(bridge, token).await;
                }
            }
            Err(error) => self.fail_if_current(&token, error),
        }
    }

    /// Polls the extension bridge until background node startup completes or fails.
    async fn poll_extension_node_start(self, bridge: &JsValue, token: GenerationToken) {
        let result = extension::poll_extension_node_start(
            bridge,
            self.did.clone(),
            self.peers.clone(),
            self.wallet_account.clone(),
            self.node_starting.clone(),
            self.status.clone(),
            token.clone(),
        )
        .await;
        if let Err(error) = result {
            self.fail_if_current(&token, error);
        }
    }

    /// Validates settings, authorizes the wallet, builds a local node, and activates it.
    async fn start_local_node(self, request: StartRequest, token: GenerationToken) {
        let settings = match extension::node_settings(
            request.network_id,
            request.ice_servers,
            request.stabilize_interval,
            request.storage_name,
        ) {
            Ok(settings) => settings,
            Err(error) => {
                self.fail_if_current(&token, error.to_string());
                return;
            }
        };
        let account = match self.authorize_account(request.kind, &token).await {
            Some(account) => account,
            None => return,
        };
        let built = match self.build_local_node(&account, settings, &token).await {
            Some(node) => node,
            None => return,
        };
        self.activate_local_node(built, account, request.seed_url, token)
            .await;
    }

    /// Connects the requested wallet with a timeout, discarding stale results.
    async fn authorize_account(
        &self,
        kind: WalletKind,
        token: &GenerationToken,
    ) -> Option<WalletAccount> {
        match extension::operation_timeout(
            "account authorization",
            extension::WALLET_CONNECT_TIMEOUT,
            wallet::connect(kind),
        )
        .await
        {
            Ok(account) if token.is_current() => Some(account),
            Ok(_) => None,
            Err(error) => {
                self.fail_if_current(token, error);
                None
            }
        }
    }

    /// Authorizes a delegatee session and builds the local [`DemoNode`].
    async fn build_local_node(
        &self,
        account: &WalletAccount,
        settings: node::NodeSettings,
        token: &GenerationToken,
    ) -> Option<DemoNode> {
        self.status.set("authorizing delegatee key".to_string());
        match extension::operation_timeout(
            "session authorization",
            extension::SESSION_AUTH_TIMEOUT,
            node::build_node(account, settings, node::WebviewHost::CurrentWindow),
        )
        .await
        {
            Ok(node) if token.is_current() => Some(node),
            Ok(node) => {
                let _ = node.close().await;
                None
            }
            Err(error) => {
                self.fail_if_current(token, error);
                None
            }
        }
    }

    /// Registers protocols, installs the WebView gateway, publishes node state, and connects the seed.
    async fn activate_local_node(
        self,
        built: DemoNode,
        account: WalletAccount,
        seed_url: String,
        token: GenerationToken,
    ) {
        let my_did = built.provider.address();
        if let Err(error) = self.register_local_protocols(&built, &my_did) {
            let _ = built.close().await;
            self.fail_if_current(&token, error);
            return;
        }
        if !token.is_current() {
            let _ = built.close().await;
            return;
        }
        self.did.set(my_did.clone());
        self.wallet_account.set(Some(account));
        *self.node_ref.borrow_mut() = Some(built.clone());
        let webview_result = match webview::install_browser_gateway(built.webview.clone()) {
            Ok(true) => webview::register_browser_gateway().await.map(|()| true),
            Ok(false) => Ok(false),
            Err(error) => Err(error),
        };
        if !token.is_current() {
            self.discard_stale_local_node(built).await;
            return;
        }
        let webview_ready = match webview_result {
            Ok(ready) => ready,
            Err(error) => {
                self.status.set(format!("webview gateway: {error}"));
                false
            }
        };
        self.webview_ready.set(webview_ready);
        super::routes::clear_shell_dialog_route();
        self.active_dialog.set(ActiveDialog::None);
        self.node_starting.set(false);
        self.connect_seed_if_configured(built, seed_url, token)
            .await;
    }

    /// Clears and closes a locally built node that became stale before activation finished.
    async fn discard_stale_local_node(&self, built: DemoNode) {
        let stale = {
            let mut node_ref = self.node_ref.borrow_mut();
            if node_ref
                .as_ref()
                .is_some_and(|node| node.same_provider_instance(&built))
            {
                let taken = node_ref.take();
                webview::clear_browser_gateway();
                taken
            } else {
                None
            }
        };
        drop(stale);
        let _ = built.close().await;
    }

    /// Registers the dweb and demo custom protocol handlers on a newly built local node.
    fn register_local_protocols(&self, built: &DemoNode, my_did: &str) -> Result<(), String> {
        self.site.borrow_mut().insert(
            "/".to_string(),
            format!("<h1>Rings node {my_did}</h1><p>Served by the Rings browser frontend.</p>"),
        );
        let on_dweb_response = Callback::from(|_: dweb::DwebResponse| {});
        let dweb_handler = dweb::register(&built.provider, self.site.clone(), on_dweb_response)?;
        built.retain_protocol_handler(dweb_handler);
        let on_custom = self.custom_event_handler();
        for namespace in custom::DEMO_NAMESPACES {
            let handler =
                custom::register(&built.provider, namespace.to_string(), on_custom.clone())?;
            built.retain_protocol_handler(handler);
        }
        Ok(())
    }

    /// Builds a callback that prepends incoming custom protocol events to the bounded log.
    fn custom_event_handler(&self) -> Callback<custom::CustomEvent> {
        let custom_events = self.custom_events.clone();
        Callback::from(move |event: custom::CustomEvent| {
            let mut next = (*custom_events).clone();
            next.insert(0, event);
            next.truncate(20);
            custom_events.set(next);
        })
    }

    /// Connects to the configured seed HTTP endpoint if non-empty.
    async fn connect_seed_if_configured(
        self,
        built: DemoNode,
        seed_url: String,
        token: GenerationToken,
    ) {
        if seed_url.is_empty() {
            self.status.set("node ready".to_string());
            return;
        }
        self.status
            .set(format!("node ready; connecting seed {seed_url}"));
        match node::connect_http(&built.provider, seed_url).await {
            Ok(seed_did) => self.sync_seed_peer(built, seed_did, token).await,
            Err(error) => self.set_seed_error(&token, error),
        }
    }

    /// Synchronizes the peer list after completing an HTTP seed handshake.
    async fn sync_seed_peer(self, built: DemoNode, seed_did: String, token: GenerationToken) {
        if seed_did.trim().is_empty() {
            if token.is_current() {
                self.status
                    .set("node ready; seed returned an empty DID".to_string());
            }
            return;
        }
        if !token.is_current() {
            return;
        }
        let seed_token = token.clone();
        peer_sync::sync_peers_after_handshake(
            built,
            self.peers,
            self.status,
            "seed URL connected",
            move || seed_token.is_current(),
        )
        .await;
    }

    /// Applies an extension status snapshot to the UI state handles.
    fn apply_extension_snapshot(
        &self,
        snapshot: extension::ExtensionNodeSnapshot,
        token: &GenerationToken,
    ) -> bool {
        extension::apply_extension_snapshot(
            snapshot,
            &self.did,
            &self.peers,
            &self.wallet_account,
            &self.node_starting,
            &self.status,
            token,
        )
    }

    /// Records a seed connection error if the generation token is still current.
    fn set_seed_error(&self, token: &GenerationToken, error: String) {
        if token.is_current() {
            self.status
                .set(format!("node ready; seed connect failed: {error}"));
        }
    }

    /// Marks startup as failed and updates the status message if the token is still current.
    fn fail_if_current(&self, token: &GenerationToken, error: String) {
        if token.is_current() {
            self.node_starting.set(false);
            self.status.set(error);
        }
    }
}

impl DisconnectAction {
    /// Creates a click callback that disconnects the active node.
    fn callback(self) -> Callback<MouseEvent> {
        Callback::from(move |_| self.clone().disconnect())
    }

    /// Dispatches disconnection to either the extension bridge or the local node.
    fn disconnect(self) {
        if let Some(bridge) = extension::extension_node_bridge() {
            self.disconnect_extension_node(bridge);
        } else {
            self.disconnect_local_node();
        }
    }

    /// Requests the browser extension background node to stop and clears session state on success.
    fn disconnect_extension_node(self, bridge: JsValue) {
        let stop_token = self.generation.bump();
        self.node_starting.set(true);
        self.status.set("stopping background node".to_string());
        wasm_bindgen_futures::spawn_local(async move {
            match extension::extension_node_stop(&bridge).await {
                Ok(message) if stop_token.is_current() => {
                    self.clear_session();
                    self.status.set(message);
                }
                Ok(_) => {}
                Err(error) if stop_token.is_current() => {
                    self.node_starting.set(false);
                    self.status.set(format!("background stop failed: {error}"));
                }
                Err(_) => {}
            }
        });
    }

    /// Tears down the local in-page node, clears session state, and closes peer links asynchronously.
    fn disconnect_local_node(self) {
        let was_starting = *self.node_starting;
        let cleanup_token = self.generation.bump();
        let Some(node) = self.node_ref.borrow_mut().take() else {
            webview::clear_browser_gateway();
            self.node_starting.set(false);
            self.status.set(offline_disconnect_message(was_starting));
            return;
        };
        webview::clear_browser_gateway();
        self.clear_session();
        self.status.set("node disconnected".to_string());
        let status = self.status.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let message = cleanup_peer_links(&node.provider).await;
            let message = match node.close().await {
                Ok(()) => message,
                Err(error) => format!("{message}; storage release failed: {error}"),
            };
            if cleanup_token.is_current() {
                status.set(message);
            }
        });
    }

    /// Resets all node, SDP, custom protocol, and dialog UI state to offline defaults.
    fn clear_session(&self) {
        self.did.set(String::new());
        self.wallet_account.set(None);
        self.node_starting.set(false);
        self.webview_ready.set(false);
        self.peers.set(Vec::new());
        self.generated_offer.set(String::new());
        self.remote_offer.set(String::new());
        self.generated_answer.set(String::new());
        self.remote_answer.set(String::new());
        self.link_dialog_open.set(false);
        self.custom_registered.set(
            custom::DEMO_NAMESPACES
                .iter()
                .map(|ns| (*ns).to_string())
                .collect(),
        );
        self.custom_events.set(Vec::new());
        super::routes::clear_shell_dialog_route();
        self.active_dialog.set(ActiveDialog::None);
    }
}

/// Disconnects all active peer transports on `provider` with a two-second timeout.
async fn cleanup_peer_links(provider: &std::sync::Arc<rings_node::provider::Provider>) -> String {
    let cleanup = node::disconnect_all(provider).fuse();
    let timeout = sleep(Duration::from_secs(2)).fuse();
    futures::pin_mut!(cleanup, timeout);
    futures::select! {
        result = cleanup => match result {
            Ok(0) => "node disconnected".to_string(),
            Ok(count) => format!("node disconnected; closed {count} peer links"),
            Err(error) => format!("node disconnected; peer cleanup failed: {error}"),
        },
        _ = timeout => "node disconnected; peer cleanup timed out".to_string(),
    }
}

/// Returns the status message shown when disconnecting while no node instance is active.
fn offline_disconnect_message(was_starting: bool) -> String {
    if was_starting {
        "node start cancelled"
    } else {
        "node already offline"
    }
    .to_string()
}
