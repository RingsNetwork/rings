//! Yew application for the Rings browser frontend.

use std::cell::RefCell;
use std::rc::Rc;

use gloo_timers::callback::Interval;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::Event;
use yew::prelude::*;

use crate::connect;
use crate::connect::ConnectState;
use crate::connect::LinkTab;
use crate::connect::SdpMode;
use crate::controls;
use crate::controls::ActiveDialog;
use crate::controls::ControlView;
use crate::controls::LaunchActions;
use crate::controls::Panel;
use crate::controls::SessionView;
use crate::controls::ShellPage;
use crate::custom;
use crate::dweb;
use crate::extension;
use crate::footer;
use crate::forms::select_value;
use crate::generation::GenerationClock;
use crate::guide;
use crate::landing;
use crate::node;
use crate::node::DemoNode;
use crate::node::PeerView;
use crate::styles;
use crate::wallet::WalletAccount;
use crate::wallet::WalletKind;
use crate::webview;
use crate::webview_ui;
use crate::workbench;

mod actions;
mod routes;

/// Default ICE server list setting (empty to use the node's built-in defaults).
pub(crate) const DEFAULT_ICE_SERVERS: &str = "";
/// Legacy default ICE server URL migrated automatically to [`DEFAULT_ICE_SERVERS`].
pub(crate) const LEGACY_DEFAULT_ICE_SERVERS: &str = "stun://stun.l.google.com:19302";
/// Default DHT stabilization interval setting, in seconds.
pub(crate) const DEFAULT_STABILIZE_INTERVAL_SECONDS: &str = "15";
/// Legacy stabilization interval setting migrated automatically to [`DEFAULT_STABILIZE_INTERVAL_SECONDS`].
const LEGACY_DEFAULT_STABILIZE_INTERVAL_SECONDS: &str = "3";

/// Snapshot of persisted frontend settings used as a Yew effect dependency.
#[derive(Clone, PartialEq)]
struct SettingsSnapshot {
    /// Serialized wallet kind identifier.
    wallet_kind: String,
    /// Rings network identifier.
    network_id: String,
    /// ICE server configuration string.
    ice_servers: String,
    /// DHT stabilization interval in seconds.
    stabilize_interval: String,
    /// Browser storage database name.
    storage_name: String,
    /// Default seed HTTP endpoint URL.
    seed_url: String,
    /// Last entered peer HTTP endpoint URL.
    http_endpoint: String,
}

/// Top-level UI navigation and shell layout state.
struct ShellState {
    /// Currently displayed top-level page.
    active_page: UseStateHandle<ShellPage>,
    /// Selected layer index in the landing page architecture diagram.
    active_architecture_layer: UseStateHandle<usize>,
    /// Active workbench panel tab.
    active_panel: UseStateHandle<Panel>,
    /// Currently open console dialog, if any.
    active_dialog: UseStateHandle<ActiveDialog>,
    /// Whether the console control sidebar is collapsed.
    control_sidebar_collapsed: UseStateHandle<bool>,
}

/// Reactive state and handles for the local or extension Rings node.
struct NodeState {
    /// Selected wallet standard.
    wallet_kind: UseStateHandle<WalletKind>,
    /// Authenticated wallet account metadata, if connected.
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    /// Whether the node is currently starting or stopping.
    node_starting: UseStateHandle<bool>,
    /// Shared reference to the active in-page [`DemoNode`].
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    /// Generation clock used to invalidate stale async operations.
    generation: GenerationClock,
    /// In-memory dweb site content served by the local node.
    site: dweb::Site,
    /// Local node DID string, empty when offline.
    did: UseStateHandle<String>,
    /// Human-readable status line shown in the console.
    status: UseStateHandle<String>,
    /// Configured Rings network identifier.
    network_id: UseStateHandle<String>,
    /// Configured ICE server list.
    ice_servers: UseStateHandle<String>,
    /// Configured stabilization interval in seconds.
    stabilize_interval: UseStateHandle<String>,
    /// Configured browser storage database name.
    storage_name: UseStateHandle<String>,
    /// Current snapshot of connected peers.
    peers: UseStateHandle<Vec<PeerView>>,
    /// Configured seed HTTP endpoint URL.
    seed_url: UseStateHandle<String>,
    /// Whether the local browser WebView gateway is registered.
    webview_ready: UseStateHandle<bool>,
}

/// Reactive state for the peer connection and manual SDP exchange dialog.
struct LinkState {
    /// HTTP seed endpoint input value.
    http_endpoint: UseStateHandle<String>,
    /// Remote peer DID input value for manual SDP offers.
    sdp_remote_did: UseStateHandle<String>,
    /// Locally generated SDP offer payload.
    generated_offer: UseStateHandle<String>,
    /// Remote SDP offer payload pasted by the responder.
    remote_offer: UseStateHandle<String>,
    /// Locally generated SDP answer payload.
    generated_answer: UseStateHandle<String>,
    /// Remote SDP answer payload pasted by the initiator.
    remote_answer: UseStateHandle<String>,
    /// Selected manual SDP role (initiator or responder).
    sdp_mode: UseStateHandle<SdpMode>,
    /// Whether the peer link dialog is open.
    link_dialog_open: UseStateHandle<bool>,
    /// Active tab in the peer link dialog.
    link_tab: UseStateHandle<LinkTab>,
}

/// Reactive state for the custom protocol workbench panel.
struct CustomState {
    /// Target custom protocol namespace input.
    namespace: UseStateHandle<String>,
    /// List of currently registered custom protocol namespaces.
    registered: UseStateHandle<Vec<String>>,
    /// Target peer DID input for outgoing messages.
    peer: UseStateHandle<String>,
    /// Outgoing message payload input.
    payload: UseStateHandle<String>,
    /// Bounded log of received custom protocol events.
    events: UseStateHandle<Vec<custom::CustomEvent>>,
}

/// Reactive state for the onion HTTP proxy workbench panel.
struct OnionState {
    /// Target request URL input.
    url: UseStateHandle<String>,
    /// HTTP method input.
    method: UseStateHandle<String>,
    /// Request headers text input.
    headers: UseStateHandle<String>,
    /// Request body text input.
    body: UseStateHandle<String>,
    /// Summary of the onion route used for the last request.
    route_result: UseStateHandle<String>,
    /// Status text of the last onion proxy response.
    response_status: UseStateHandle<String>,
    /// Formatted response headers from the last onion proxy request.
    response_headers: UseStateHandle<String>,
    /// Response body from the last onion proxy request.
    response_body: UseStateHandle<String>,
}

/// Availability status and disabled reason for the WebView launcher control.
#[derive(Debug, Eq, PartialEq)]
struct WebviewAvailability {
    /// Whether the WebView popup can currently be opened.
    ready: bool,
    /// Explanation shown when WebView is unavailable; empty when ready.
    unavailable_reason: String,
}

/// Borrowed application state and actions passed to the top-level render helpers.
struct AppRenderContext<'a> {
    /// Shell navigation and layout state.
    shell: &'a ShellState,
    /// Node identity, settings, and peer state.
    node: &'a NodeState,
    /// Peer link dialog state.
    link: &'a LinkState,
    /// Custom protocol workbench state.
    custom_state: &'a CustomState,
    /// Onion proxy workbench state.
    onion: &'a OnionState,
    /// Node launch and disconnect callbacks.
    launch_actions: LaunchActions,
    /// Whether the app is running inside the browser extension popup/bridge.
    extension_mode: bool,
}

/// Initializes the shell navigation, panel, and dialog state handles.
#[hook]
fn use_shell_state() -> ShellState {
    ShellState {
        active_page: use_state(routes::initial_shell_page),
        active_architecture_layer: use_state(|| 0_usize),
        active_panel: use_state(|| Panel::Onion),
        active_dialog: use_state(routes::initial_shell_dialog),
        control_sidebar_collapsed: use_state(|| false),
    }
}

/// Initializes the node lifecycle, persisted settings, and peer state handles.
#[hook]
fn use_node_state() -> NodeState {
    let generation_ref = use_mut_ref(GenerationClock::default);
    let generation = generation_ref.borrow().clone();
    NodeState {
        wallet_kind: use_state(initial_wallet_kind),
        wallet_account: use_state(|| None::<WalletAccount>),
        node_starting: use_state(|| false),
        node_ref: use_mut_ref(|| None::<DemoNode>),
        generation,
        site: use_mut_ref(dweb::default_site),
        did: use_state(String::new),
        status: use_state(|| "select an account standard and start the browser node".to_string()),
        network_id: use_state(|| {
            load_setting_or_default(
                extension::SETTING_NETWORK_ID,
                extension::LEGACY_SETTING_NETWORK_ID,
                "1",
            )
        }),
        ice_servers: use_state(load_ice_servers_setting_or_default),
        stabilize_interval: use_state(load_stabilize_interval_setting_or_default),
        storage_name: use_state(|| {
            load_setting_or_default(
                extension::SETTING_STORAGE_NAME,
                extension::LEGACY_SETTING_STORAGE_NAME,
                "rings-frontend",
            )
        }),
        peers: use_state(Vec::<PeerView>::new),
        seed_url: use_state(|| {
            load_setting_or_default(
                extension::SETTING_SEED_URL,
                extension::LEGACY_SETTING_SEED_URL,
                node::PUBLIC_SEED_ENDPOINT,
            )
        }),
        webview_ready: use_state(|| false),
    }
}

/// Initializes the peer link dialog and SDP exchange state handles.
#[hook]
fn use_link_state() -> LinkState {
    LinkState {
        http_endpoint: use_state(|| {
            load_setting_or_default(
                extension::SETTING_HTTP_ENDPOINT,
                extension::LEGACY_SETTING_HTTP_ENDPOINT,
                node::PUBLIC_SEED_ENDPOINT,
            )
        }),
        sdp_remote_did: use_state(String::new),
        generated_offer: use_state(String::new),
        remote_offer: use_state(String::new),
        generated_answer: use_state(String::new),
        remote_answer: use_state(String::new),
        sdp_mode: use_state(|| SdpMode::Initiator),
        link_dialog_open: use_state(|| false),
        link_tab: use_state(|| LinkTab::ManualSdp),
    }
}

/// Initializes the custom protocol workbench state handles.
#[hook]
fn use_custom_state() -> CustomState {
    CustomState {
        namespace: use_state(|| "custom".to_string()),
        registered: use_state(|| {
            custom::DEMO_NAMESPACES
                .iter()
                .map(|namespace| (*namespace).to_string())
                .collect::<Vec<_>>()
        }),
        peer: use_state(String::new),
        payload: use_state(|| "hello from Rings".to_string()),
        events: use_state(Vec::<custom::CustomEvent>::new),
    }
}

/// Initializes the onion proxy workbench state handles.
#[hook]
fn use_onion_state() -> OnionState {
    OnionState {
        url: use_state(|| "https://example.com/".to_string()),
        method: use_state(|| "GET".to_string()),
        headers: use_state(String::new),
        body: use_state(String::new),
        route_result: use_state(String::new),
        response_status: use_state(|| "idle".to_string()),
        response_headers: use_state(String::new),
        response_body: use_state(String::new),
    }
}

/// Loads the persisted wallet kind or falls back to [`WalletKind::WebCrypto`].
fn initial_wallet_kind() -> WalletKind {
    extension::load_setting_with_legacy(
        extension::SETTING_WALLET_KIND,
        extension::LEGACY_SETTING_WALLET_KIND,
    )
    .map(|value| WalletKind::from_value(&value))
    .unwrap_or(WalletKind::WebCrypto)
}

/// Loads a setting from current or legacy storage keys, returning `default` if absent.
fn load_setting_or_default(key: &str, legacy_key: &str, default: &'static str) -> String {
    extension::load_setting_with_legacy(key, legacy_key).unwrap_or_else(|| default.to_string())
}

/// Loads the ICE servers setting, migrating the legacy STUN default to an empty string.
fn load_ice_servers_setting_or_default() -> String {
    match extension::load_setting_with_legacy(
        extension::SETTING_ICE_SERVERS,
        extension::LEGACY_SETTING_ICE_SERVERS,
    ) {
        Some(value) if value.trim() == LEGACY_DEFAULT_ICE_SERVERS => {
            extension::save_setting(extension::SETTING_ICE_SERVERS, DEFAULT_ICE_SERVERS);
            DEFAULT_ICE_SERVERS.to_string()
        }
        Some(value) => value,
        None => DEFAULT_ICE_SERVERS.to_string(),
    }
}

/// Loads the stabilization interval setting, migrating the legacy 3-second default.
fn load_stabilize_interval_setting_or_default() -> String {
    match extension::load_setting_with_legacy(
        extension::SETTING_STABILIZE_INTERVAL,
        extension::LEGACY_SETTING_STABILIZE_INTERVAL,
    ) {
        Some(value) if value.trim() == LEGACY_DEFAULT_STABILIZE_INTERVAL_SECONDS => {
            extension::save_setting(
                extension::SETTING_STABILIZE_INTERVAL,
                DEFAULT_STABILIZE_INTERVAL_SECONDS,
            );
            DEFAULT_STABILIZE_INTERVAL_SECONDS.to_string()
        }
        Some(value) => value,
        None => DEFAULT_STABILIZE_INTERVAL_SECONDS.to_string(),
    }
}

/// Rings browser frontend app.
#[function_component(App)]
pub fn app() -> Html {
    let shell = use_shell_state();
    let node = use_node_state();
    let link = use_link_state();
    let custom_state = use_custom_state();
    let onion = use_onion_state();
    let on_wallet_kind = wallet_kind_callback(&node.wallet_kind);
    use_settings_persistence(&node, &link);
    use_extension_status(&node);
    let launch_actions =
        actions::launch_actions(&node, &link, &custom_state, &shell, on_wallet_kind);
    use_peer_refresh(&node);
    let extension_mode = extension::extension_node_bridge().is_some();
    use_extension_panel_reset(shell.active_panel.clone(), extension_mode);
    use_shell_history(shell.active_page.clone(), shell.active_dialog.clone());
    render_app(AppRenderContext {
        shell: &shell,
        node: &node,
        link: &link,
        custom_state: &custom_state,
        onion: &onion,
        launch_actions,
        extension_mode,
    })
}

/// Builds a change callback that updates `wallet_kind` from a `<select>` event.
fn wallet_kind_callback(wallet_kind: &UseStateHandle<WalletKind>) -> Callback<Event> {
    let wallet_kind = wallet_kind.clone();
    Callback::from(move |event: Event| {
        if let Some(value) = select_value(&event) {
            wallet_kind.set(WalletKind::from_value(&value));
        }
    })
}

/// Persists node and link configuration values whenever any setting changes.
#[hook]
fn use_settings_persistence(node: &NodeState, link: &LinkState) {
    let settings_snapshot = SettingsSnapshot {
        wallet_kind: (*node.wallet_kind).value().to_string(),
        network_id: (*node.network_id).clone(),
        ice_servers: (*node.ice_servers).clone(),
        stabilize_interval: (*node.stabilize_interval).clone(),
        storage_name: (*node.storage_name).clone(),
        seed_url: (*node.seed_url).clone(),
        http_endpoint: (*link.http_endpoint).clone(),
    };
    use_effect_with(settings_snapshot, move |settings| {
        extension::save_setting(extension::SETTING_WALLET_KIND, &settings.wallet_kind);
        extension::save_setting(extension::SETTING_NETWORK_ID, &settings.network_id);
        extension::save_setting(extension::SETTING_ICE_SERVERS, &settings.ice_servers);
        extension::save_setting(
            extension::SETTING_STABILIZE_INTERVAL,
            &settings.stabilize_interval,
        );
        extension::save_setting(extension::SETTING_STORAGE_NAME, &settings.storage_name);
        extension::save_setting(extension::SETTING_SEED_URL, &settings.seed_url);
        extension::save_setting(extension::SETTING_HTTP_ENDPOINT, &settings.http_endpoint);
    });
}

/// Fetches the initial background node status when running inside the browser extension.
#[hook]
fn use_extension_status(node: &NodeState) {
    let did = node.did.clone();
    let peers = node.peers.clone();
    let wallet_account = node.wallet_account.clone();
    let node_starting = node.node_starting.clone();
    let status = node.status.clone();
    let token = node.generation.token();
    use_effect_with((), move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            let Some(bridge) = extension::extension_node_bridge() else {
                return;
            };
            match extension::extension_node_status(&bridge).await {
                Ok(snapshot) => {
                    extension::apply_extension_snapshot(
                        snapshot,
                        &did,
                        &peers,
                        &wallet_account,
                        &node_starting,
                        &status,
                        &token,
                    );
                }
                Err(error) => status.set(format!("background status failed: {error}")),
            }
        });
    });
}

/// Polls the peer list every four seconds while the node is online.
#[hook]
fn use_peer_refresh(node: &NodeState) {
    let node_ref = node.node_ref.clone();
    let generation = node.generation.clone();
    let peers = node.peers.clone();
    let did = node.did.clone();
    let wallet_account = node.wallet_account.clone();
    let node_starting = node.node_starting.clone();
    let status = node.status.clone();
    let node_online = !(*node.did).is_empty();
    use_effect_with(node_online, move |online| {
        let interval = if *online {
            Some(Interval::new(4_000, move || {
                refresh_peer_snapshot(
                    node_ref.clone(),
                    generation.clone(),
                    peers.clone(),
                    did.clone(),
                    wallet_account.clone(),
                    node_starting.clone(),
                    status.clone(),
                );
            }))
        } else {
            None
        };
        move || drop(interval)
    });
}

/// Refreshes the connected peer snapshot from either the extension bridge or the local node.
fn refresh_peer_snapshot(
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    peers: UseStateHandle<Vec<PeerView>>,
    did: UseStateHandle<String>,
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    node_starting: UseStateHandle<bool>,
    status: UseStateHandle<String>,
) {
    if let Some(bridge) = extension::extension_node_bridge() {
        refresh_extension_snapshot(
            bridge,
            generation,
            peers,
            did,
            wallet_account,
            node_starting,
            status,
        );
        return;
    }
    let Some(node) = node_ref.borrow().clone() else {
        return;
    };
    let refresh_token = generation.token();
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(next) = node::list_peers(&node.provider).await {
            if refresh_token.is_current() {
                if let Some(updated) =
                    crate::peer_sync::refresh_waiting_peer_status(&status, next.len())
                {
                    status.set(updated);
                }
                peers.set(next);
            }
        }
    });
}

/// Fetches and applies the latest node status snapshot from the browser extension bridge.
fn refresh_extension_snapshot(
    bridge: JsValue,
    generation: GenerationClock,
    peers: UseStateHandle<Vec<PeerView>>,
    did: UseStateHandle<String>,
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    node_starting: UseStateHandle<bool>,
    status: UseStateHandle<String>,
) {
    let refresh_token = generation.token();
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(snapshot) = extension::extension_node_status(&bridge).await {
            extension::apply_extension_snapshot(
                snapshot,
                &did,
                &peers,
                &wallet_account,
                &node_starting,
                &status,
                &refresh_token,
            );
        }
    });
}

/// Resets the workbench panel to [`Panel::Onion`] when entering extension mode.
#[hook]
fn use_extension_panel_reset(active_panel: UseStateHandle<Panel>, extension_mode: bool) {
    use_effect_with(extension_mode, move |extension_mode| {
        if *extension_mode {
            active_panel.set(Panel::Onion);
        }
        || {}
    });
}

/// Subscribes to `popstate` and `hashchange` events to keep shell state aligned with the URL.
#[hook]
fn use_shell_history(
    active_page: UseStateHandle<ShellPage>,
    active_dialog: UseStateHandle<ActiveDialog>,
) {
    use_effect_with((), move |_| {
        let listener = web_sys::window().map(|window| {
            let page = active_page.clone();
            let dialog = active_dialog.clone();
            let listener = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_| {
                let route = routes::current_shell_route();
                page.set(route.page);
                dialog.set(route.dialog);
            }));
            let callback = listener.as_ref().unchecked_ref();
            let _ = window.add_event_listener_with_callback("popstate", callback);
            let _ = window.add_event_listener_with_callback("hashchange", callback);
            (window, listener)
        });

        move || {
            if let Some((window, listener)) = listener {
                let callback = listener.as_ref().unchecked_ref();
                let _ = window.remove_event_listener_with_callback("popstate", callback);
                let _ = window.remove_event_listener_with_callback("hashchange", callback);
            }
        }
    });
}

/// Renders the active top-level view (WebView, document shell, or console shell).
fn render_app(ctx: AppRenderContext<'_>) -> Html {
    let effective_page = effective_shell_page(ctx.shell, ctx.extension_mode);
    if effective_page == ShellPage::Webview {
        return html! { <webview_ui::WebviewShell /> };
    }
    let navigate_page = navigate_page_callback(ctx.shell);
    let header = controls::app_header(effective_page, navigate_page.clone(), !ctx.extension_mode);
    match document_body(effective_page, ctx.shell, &navigate_page) {
        Some(body) => render_document_shell(effective_page, header, body, navigate_page),
        None => render_console_shell(ctx, header),
    }
}

/// The body of a document page, or `None` for the application screens. The pages partition
/// into documents (landing, guide), which scroll and carry the site footer, and application
/// screens (console, WebView), which fill the viewport.
fn document_body(
    page: ShellPage,
    shell: &ShellState,
    navigate_page: &Callback<ShellPage>,
) -> Option<Html> {
    match page {
        ShellPage::Home => Some(landing::page(
            navigate_page.clone(),
            shell.active_architecture_layer.clone(),
        )),
        ShellPage::Guide => Some(guide::page(navigate_page.clone())),
        ShellPage::Console | ShellPage::Webview => None,
    }
}

/// Resolves the page to display, forcing [`ShellPage::Console`] in extension mode.
fn effective_shell_page(shell: &ShellState, extension_mode: bool) -> ShellPage {
    if extension_mode {
        ShellPage::Console
    } else {
        *shell.active_page
    }
}

/// Builds a callback that navigates to a new [`ShellPage`] and updates browser history.
fn navigate_page_callback(shell: &ShellState) -> Callback<ShellPage> {
    let active_page = shell.active_page.clone();
    let active_dialog = shell.active_dialog.clone();
    Callback::from(move |page| routes::navigate_shell_page(page, &active_page, &active_dialog))
}

/// Builds the open and close callbacks for console modal dialogs.
fn dialog_actions(shell: &ShellState) -> controls::DialogActions {
    let open_page = shell.active_page.clone();
    let open_dialog = shell.active_dialog.clone();
    let close_page = shell.active_page.clone();
    let close_dialog = shell.active_dialog.clone();
    controls::DialogActions {
        open: Callback::from(move |dialog| {
            routes::open_shell_dialog(dialog, &open_page, &open_dialog);
        }),
        close: Callback::from(move |_| {
            routes::close_shell_dialog(&close_page, &close_dialog);
        }),
    }
}

/// A document page: the header, then one scroll container holding the page body and the site
/// footer. The container is keyed by the page so switching pages replaces it and the new page
/// opens at its top instead of inheriting the previous page's scroll offset.
fn render_document_shell(
    page: ShellPage,
    header: Html,
    body: Html,
    navigate_page: Callback<ShellPage>,
) -> Html {
    html! {
        <main class="app-shell document-shell">
            <style>{ styles::app_css() }</style>
            { header }
            <div class="site-document" key={page.slug()}>
                { body }
                { footer::site_footer(navigate_page) }
            </div>
        </main>
    }
}

/// Renders the full-viewport console shell with topology stage, link control, and sidebar.
fn render_console_shell(ctx: AppRenderContext<'_>, header: Html) -> Html {
    let link_control = render_link_control(ctx.node, ctx.link, ctx.shell);
    let workbench_body = render_workbench_body(&ctx);
    let dialog_actions = dialog_actions(ctx.shell);
    let webview_availability = webview_availability(
        webview_gateway_ready(ctx.extension_mode, &ctx.node.did, *ctx.node.webview_ready),
        ctx.extension_mode,
    );
    let workbench_control = controls::workbench_control(
        *ctx.shell.active_panel,
        ctx.shell.active_panel.clone(),
        *ctx.shell.active_dialog,
        dialog_actions.clone(),
        workbench_body,
        true,
        ctx.extension_mode,
    );
    let webview_control = Some({
        let status = ctx.node.status.clone();
        let ready = webview_availability.ready;
        controls::webview_control(
            ready,
            webview_availability.unavailable_reason.clone(),
            Callback::from(move |_| {
                if let Err(error) = webview::open_webview_popup() {
                    status.set(format!("open webview: {error}"));
                }
            }),
        )
    });
    let control_sidebar = controls::control_sidebar(
        control_view(ctx.node),
        ctx.launch_actions,
        controls::ControlSidebarShell {
            workbench_control,
            webview_control,
            active_dialog: *ctx.shell.active_dialog,
            dialog_actions,
            collapsed: ctx.shell.control_sidebar_collapsed.clone(),
            extension_mode: ctx.extension_mode,
        },
    );
    let shell_class = console_shell_class(ctx.extension_mode);
    html! {
        <main class={shell_class}>
            <style>{ styles::app_css() }</style>
            { header }
            { controls::network_stage(
                session_view(ctx.node),
                &ctx.node.status,
                link_control,
                control_sidebar,
            ) }
        </main>
    }
}

/// Returns the CSS class list for the console shell root element.
fn console_shell_class(extension_mode: bool) -> &'static str {
    if extension_mode {
        "app-shell topology-shell extension-mode"
    } else {
        "app-shell topology-shell"
    }
}

/// Returns whether the WebView gateway is ready in either extension or browser mode.
fn webview_gateway_ready(extension_mode: bool, node_did: &str, local_gateway_ready: bool) -> bool {
    if extension_mode {
        !node_did.is_empty()
    } else {
        local_gateway_ready
    }
}

/// Computes the WebView button readiness and disabled tooltip text.
fn webview_availability(gateway_ready: bool, extension_mode: bool) -> WebviewAvailability {
    if !gateway_ready {
        let reason = if extension_mode {
            "WebView is available after the extension node is online"
        } else {
            "WebView is available after the local gateway is ready"
        }
        .to_string();
        return WebviewAvailability {
            ready: false,
            unavailable_reason: reason,
        };
    }

    WebviewAvailability {
        ready: true,
        unavailable_reason: String::new(),
    }
}

/// Projects [`NodeState`] into a borrowed [`ControlView`] for the control sidebar.
fn control_view(node: &NodeState) -> ControlView<'_> {
    ControlView {
        wallet_kind: *node.wallet_kind,
        wallet_account: (*node.wallet_account).clone(),
        node_starting: *node.node_starting,
        did: &node.did,
        status: &node.status,
        peers: &node.peers,
        network_id: &node.network_id,
        ice_servers: &node.ice_servers,
        stabilize_interval: &node.stabilize_interval,
        storage_name: &node.storage_name,
        seed_url: &node.seed_url,
    }
}

/// Projects [`NodeState`] into a borrowed [`SessionView`] for the network stage.
fn session_view(node: &NodeState) -> SessionView<'_> {
    SessionView {
        wallet_account: (*node.wallet_account).clone(),
        did: &node.did,
        peers: &node.peers,
    }
}

/// Renders the peer connection launcher button and modal dialog.
fn render_link_control(node: &NodeState, link: &LinkState, shell: &ShellState) -> Html {
    connect::link_control(
        connect_state(link, shell),
        node.node_ref.clone(),
        node.generation.clone(),
        node.peers.clone(),
        node.status.clone(),
    )
}

/// Projects [`LinkState`] and [`ShellState`] into a borrowed [`ConnectState`].
fn connect_state<'a>(link: &'a LinkState, shell: &ShellState) -> ConnectState<'a> {
    ConnectState {
        http_endpoint: &link.http_endpoint,
        sdp_remote_did: &link.sdp_remote_did,
        generated_offer: &link.generated_offer,
        remote_offer: &link.remote_offer,
        generated_answer: &link.generated_answer,
        remote_answer: &link.remote_answer,
        sdp_mode: &link.sdp_mode,
        link_dialog_open: &link.link_dialog_open,
        link_tab: &link.link_tab,
        launcher_hidden: (*shell.active_dialog).is_open(),
    }
}

/// Renders the active workbench panel body (onion proxy or custom protocol).
fn render_workbench_body(ctx: &AppRenderContext<'_>) -> Html {
    match *ctx.shell.active_panel {
        Panel::Onion => render_onion_panel(ctx.node, ctx.onion),
        Panel::Custom => render_custom_panel(ctx.node, ctx.custom_state),
    }
}

/// Renders the onion HTTP proxy workbench panel.
fn render_onion_panel(node: &NodeState, onion: &OnionState) -> Html {
    workbench::onion_proxy_panel(
        workbench::OnionProxyState {
            url: &onion.url,
            method: &onion.method,
            headers: &onion.headers,
            body: &onion.body,
            route_result: &onion.route_result,
            response_status: &onion.response_status,
            response_headers: &onion.response_headers,
            response_body: &onion.response_body,
        },
        node.node_ref.clone(),
        node.generation.clone(),
        node.status.clone(),
    )
}

/// Renders the custom protocol workbench panel.
fn render_custom_panel(node: &NodeState, custom_state: &CustomState) -> Html {
    workbench::custom_panel(
        &custom_state.namespace,
        &custom_state.registered,
        &custom_state.peer,
        &custom_state.payload,
        &custom_state.events,
        node.node_ref.clone(),
        node.status.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_webview_gateway_unready_disables_the_control() {
        let availability = webview_availability(false, false);

        assert!(!availability.ready);
        assert_eq!(
            availability.unavailable_reason,
            "WebView is available after the local gateway is ready"
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_webview_gateway_readiness_does_not_guess_onion_routes_from_direct_peers() {
        let availability = webview_availability(true, false);

        assert_eq!(availability, WebviewAvailability {
            ready: true,
            unavailable_reason: String::new(),
        });
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_extension_webview_requires_an_online_node_identity() {
        assert!(!webview_gateway_ready(true, "", true));
        assert!(webview_gateway_ready(true, "did:ring:online", false));
        assert_eq!(
            webview_availability(false, true).unavailable_reason,
            "WebView is available after the extension node is online"
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_browser_webview_uses_the_local_gateway_witness() {
        assert!(!webview_gateway_ready(false, "did:ring:online", false));
        assert!(webview_gateway_ready(false, "", true));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_ice_servers_migrates_legacy_default_and_preserves_custom_values() {
        extension::save_setting(extension::SETTING_ICE_SERVERS, LEGACY_DEFAULT_ICE_SERVERS);
        assert_eq!(load_ice_servers_setting_or_default(), DEFAULT_ICE_SERVERS);
        assert_eq!(
            extension::load_setting(extension::SETTING_ICE_SERVERS).as_deref(),
            Some(DEFAULT_ICE_SERVERS)
        );

        let custom = "turn://turn.example.com:3478";
        extension::save_setting(extension::SETTING_ICE_SERVERS, custom);
        assert_eq!(load_ice_servers_setting_or_default(), custom);
        extension::save_setting(extension::SETTING_ICE_SERVERS, DEFAULT_ICE_SERVERS);
    }
}
