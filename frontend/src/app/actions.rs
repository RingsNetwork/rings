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

#[derive(Clone)]
struct StartAction {
    wallet_kind: UseStateHandle<WalletKind>,
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    node_starting: UseStateHandle<bool>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    site: dweb::Site,
    did: UseStateHandle<String>,
    status: UseStateHandle<String>,
    peers: UseStateHandle<Vec<PeerView>>,
    network_id: UseStateHandle<String>,
    ice_servers: UseStateHandle<String>,
    stabilize_interval: UseStateHandle<String>,
    storage_name: UseStateHandle<String>,
    webview_allow_short_paths: UseStateHandle<bool>,
    webview_onion_settings: webview::WebviewOnionSettings,
    seed_url: UseStateHandle<String>,
    custom_events: UseStateHandle<Vec<custom::CustomEvent>>,
    active_dialog: UseStateHandle<ActiveDialog>,
    webview_ready: UseStateHandle<bool>,
}

struct StartRequest {
    kind: WalletKind,
    network_id: String,
    ice_servers: String,
    stabilize_interval: String,
    storage_name: String,
    webview_allow_short_paths: bool,
    seed_url: String,
}

#[derive(Clone)]
struct DisconnectAction {
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    node_starting: UseStateHandle<bool>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    did: UseStateHandle<String>,
    status: UseStateHandle<String>,
    peers: UseStateHandle<Vec<PeerView>>,
    generated_offer: UseStateHandle<String>,
    remote_offer: UseStateHandle<String>,
    generated_answer: UseStateHandle<String>,
    remote_answer: UseStateHandle<String>,
    link_dialog_open: UseStateHandle<bool>,
    active_dialog: UseStateHandle<ActiveDialog>,
    webview_ready: UseStateHandle<bool>,
}

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
        webview_allow_short_paths: node.webview_allow_short_paths.clone(),
        webview_onion_settings: node.webview_onion_settings.clone(),
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
    }
    .callback();
    LaunchActions {
        on_wallet_kind,
        on_start,
        on_disconnect,
    }
}

impl StartAction {
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

    fn request(&self) -> StartRequest {
        StartRequest {
            kind: *self.wallet_kind,
            network_id: (*self.network_id).clone(),
            ice_servers: (*self.ice_servers).clone(),
            stabilize_interval: (*self.stabilize_interval).clone(),
            storage_name: (*self.storage_name).clone(),
            webview_allow_short_paths: *self.webview_allow_short_paths,
            seed_url: (*self.seed_url).trim().to_string(),
        }
    }

    async fn start(self, request: StartRequest, token: GenerationToken) {
        if let Some(bridge) = extension::extension_node_bridge() {
            self.start_extension_node(&bridge, request, token).await;
        } else {
            self.start_local_node(request, token).await;
        }
    }

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
            webview_allow_short_paths: request.webview_allow_short_paths,
            seed_url: request.seed_url,
        };
        match extension::extension_node_start(bridge, request.kind, settings).await {
            Ok(snapshot) => {
                *self.node_ref.borrow_mut() = None;
                super::clear_shell_dialog_route();
                self.active_dialog.set(ActiveDialog::None);
                if self.apply_extension_snapshot(snapshot, &token) {
                    self.poll_extension_node_start(bridge, token).await;
                }
            }
            Err(error) => self.fail_if_current(&token, error),
        }
    }

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

    async fn start_local_node(self, request: StartRequest, token: GenerationToken) {
        self.webview_onion_settings
            .set_allow_short_paths(request.webview_allow_short_paths);
        let settings = match extension::node_settings(
            request.network_id,
            request.ice_servers,
            request.stabilize_interval,
            request.storage_name,
            self.webview_onion_settings.clone(),
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

    async fn build_local_node(
        &self,
        account: &WalletAccount,
        settings: node::NodeSettings,
        token: &GenerationToken,
    ) -> Option<DemoNode> {
        self.status.set("authorizing session key".to_string());
        match extension::operation_timeout(
            "session authorization",
            extension::SESSION_AUTH_TIMEOUT,
            node::build_node(account, settings),
        )
        .await
        {
            Ok(node) if token.is_current() => Some(node),
            Ok(node) => {
                node.stop();
                None
            }
            Err(error) => {
                self.fail_if_current(token, error);
                None
            }
        }
    }

    async fn activate_local_node(
        self,
        built: DemoNode,
        account: WalletAccount,
        seed_url: String,
        token: GenerationToken,
    ) {
        let my_did = built.provider.address();
        if let Err(error) = self.register_local_protocols(&built, &my_did) {
            built.stop();
            self.fail_if_current(&token, error);
            return;
        }
        if !token.is_current() {
            built.stop();
            return;
        }
        self.did.set(my_did.clone());
        self.wallet_account.set(Some(account));
        *self.node_ref.borrow_mut() = Some(built.clone());
        let webview_ready = match webview::install_browser_gateway(built.webview.clone()) {
            Ok(true) => match webview::register_browser_gateway().await {
                Ok(()) => true,
                Err(error) => {
                    self.status.set(format!("webview gateway: {error}"));
                    false
                }
            },
            Ok(false) => false,
            Err(error) => {
                self.status.set(format!("webview gateway: {error}"));
                false
            }
        };
        if !token.is_current() {
            self.discard_stale_local_node(&built);
            return;
        }
        self.webview_ready.set(webview_ready);
        super::clear_shell_dialog_route();
        self.active_dialog.set(ActiveDialog::None);
        self.node_starting.set(false);
        self.connect_seed_if_configured(built, seed_url, token)
            .await;
    }

    fn discard_stale_local_node(&self, built: &DemoNode) {
        built.stop();
        let mut node_ref = self.node_ref.borrow_mut();
        if node_ref
            .as_ref()
            .is_some_and(|node| node.same_provider_instance(built))
        {
            *node_ref = None;
            webview::clear_browser_gateway();
        }
    }

    fn register_local_protocols(&self, built: &DemoNode, my_did: &str) -> Result<(), String> {
        self.site.borrow_mut().insert(
            "/".to_string(),
            format!("<h1>Rings node {my_did}</h1><p>Served by the Rings browser frontend.</p>"),
        );
        let on_dweb_response = Callback::from(|_: dweb::DwebResponse| {});
        dweb::register(&built.provider, self.site.clone(), on_dweb_response)?;
        let on_custom = self.custom_event_handler();
        for namespace in custom::DEMO_NAMESPACES {
            custom::register(&built.provider, namespace.to_string(), on_custom.clone())?;
        }
        Ok(())
    }

    fn custom_event_handler(&self) -> Callback<custom::CustomEvent> {
        let custom_events = self.custom_events.clone();
        Callback::from(move |event: custom::CustomEvent| {
            let mut next = (*custom_events).clone();
            next.insert(0, event);
            next.truncate(20);
            custom_events.set(next);
        })
    }

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

    async fn sync_seed_peer(self, built: DemoNode, seed_did: String, token: GenerationToken) {
        let Some(seed_peer) = PeerView::connected(seed_did) else {
            if token.is_current() {
                self.status
                    .set("node ready; seed returned an empty DID".to_string());
            }
            return;
        };
        if !token.is_current() {
            return;
        }
        let seed_token = token.clone();
        peer_sync::sync_peers_after_handshake(
            built,
            self.peers,
            self.status,
            "seed URL connected",
            Some(seed_peer),
            move || seed_token.is_current(),
        )
        .await;
    }

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

    fn set_seed_error(&self, token: &GenerationToken, error: String) {
        if token.is_current() {
            self.status
                .set(format!("node ready; seed connect failed: {error}"));
        }
    }

    fn fail_if_current(&self, token: &GenerationToken, error: String) {
        if token.is_current() {
            self.node_starting.set(false);
            self.status.set(error);
        }
    }
}

impl DisconnectAction {
    fn callback(self) -> Callback<MouseEvent> {
        Callback::from(move |_| self.clone().disconnect())
    }

    fn disconnect(self) {
        if let Some(bridge) = extension::extension_node_bridge() {
            self.disconnect_extension_node(bridge);
        } else {
            self.disconnect_local_node();
        }
    }

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

    fn disconnect_local_node(self) {
        let was_starting = *self.node_starting;
        let cleanup_token = self.generation.bump();
        let Some(node) = self.node_ref.borrow_mut().take() else {
            webview::clear_browser_gateway();
            self.node_starting.set(false);
            self.status.set(offline_disconnect_message(was_starting));
            return;
        };
        let provider = node.provider.clone();
        webview::clear_browser_gateway();
        self.clear_session();
        self.status.set("node disconnected".to_string());
        let status = self.status.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let message = cleanup_peer_links(&provider).await;
            node.stop();
            if cleanup_token.is_current() {
                status.set(message);
            }
        });
    }

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
        super::clear_shell_dialog_route();
        self.active_dialog.set(ActiveDialog::None);
    }
}

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

fn offline_disconnect_message(was_starting: bool) -> String {
    if was_starting {
        "node start cancelled"
    } else {
        "node already offline"
    }
    .to_string()
}
