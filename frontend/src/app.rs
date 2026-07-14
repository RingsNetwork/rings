//! Yew application for the Rings browser frontend.

use std::cell::RefCell;
use std::rc::Rc;

use gloo_timers::callback::Interval;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::Event;
use web_sys::Window;
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
use crate::forms::select_value;
use crate::generation::GenerationClock;
use crate::guide;
use crate::node;
use crate::node::DemoNode;
use crate::node::PeerView;
use crate::styles;
use crate::wallet::WalletAccount;
use crate::wallet::WalletKind;
use crate::workbench;

mod actions;

#[derive(Clone, PartialEq)]
struct SettingsSnapshot {
    wallet_kind: String,
    network_id: String,
    ice_servers: String,
    stabilize_interval: String,
    storage_name: String,
    seed_url: String,
    http_endpoint: String,
}

struct ShellState {
    active_page: UseStateHandle<ShellPage>,
    active_architecture_layer: UseStateHandle<usize>,
    active_panel: UseStateHandle<Panel>,
    active_dialog: UseStateHandle<ActiveDialog>,
    control_sidebar_collapsed: UseStateHandle<bool>,
}

struct NodeState {
    wallet_kind: UseStateHandle<WalletKind>,
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    node_starting: UseStateHandle<bool>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    site: dweb::Site,
    did: UseStateHandle<String>,
    status: UseStateHandle<String>,
    network_id: UseStateHandle<String>,
    ice_servers: UseStateHandle<String>,
    stabilize_interval: UseStateHandle<String>,
    storage_name: UseStateHandle<String>,
    peers: UseStateHandle<Vec<PeerView>>,
    seed_url: UseStateHandle<String>,
}

struct LinkState {
    http_endpoint: UseStateHandle<String>,
    sdp_remote_did: UseStateHandle<String>,
    generated_offer: UseStateHandle<String>,
    remote_offer: UseStateHandle<String>,
    generated_answer: UseStateHandle<String>,
    remote_answer: UseStateHandle<String>,
    sdp_mode: UseStateHandle<SdpMode>,
    link_dialog_open: UseStateHandle<bool>,
    link_tab: UseStateHandle<LinkTab>,
}

struct ProofState {
    prover_did: UseStateHandle<String>,
    r1cs_url: UseStateHandle<String>,
    wasm_url: UseStateHandle<String>,
}

struct CustomState {
    namespace: UseStateHandle<String>,
    registered: UseStateHandle<Vec<String>>,
    peer: UseStateHandle<String>,
    payload: UseStateHandle<String>,
    events: UseStateHandle<Vec<custom::CustomEvent>>,
}

struct OnionState {
    url: UseStateHandle<String>,
    method: UseStateHandle<String>,
    hop_count: UseStateHandle<String>,
    allow_short_paths: UseStateHandle<bool>,
    headers: UseStateHandle<String>,
    body: UseStateHandle<String>,
    route_result: UseStateHandle<String>,
    response_status: UseStateHandle<String>,
    response_headers: UseStateHandle<String>,
    response_body: UseStateHandle<String>,
}

struct AppRenderContext<'a> {
    shell: &'a ShellState,
    node: &'a NodeState,
    link: &'a LinkState,
    proof: &'a ProofState,
    custom_state: &'a CustomState,
    onion: &'a OnionState,
    launch_actions: LaunchActions,
    extension_mode: bool,
}

#[hook]
fn use_shell_state() -> ShellState {
    ShellState {
        active_page: use_state(initial_shell_page),
        active_architecture_layer: use_state(|| 0_usize),
        active_panel: use_state(|| Panel::Onion),
        active_dialog: use_state(|| ActiveDialog::None),
        control_sidebar_collapsed: use_state(|| false),
    }
}

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
        ice_servers: use_state(|| {
            load_setting_or_default(
                extension::SETTING_ICE_SERVERS,
                extension::LEGACY_SETTING_ICE_SERVERS,
                "stun://stun.l.google.com:19302",
            )
        }),
        stabilize_interval: use_state(|| {
            load_setting_or_default(
                extension::SETTING_STABILIZE_INTERVAL,
                extension::LEGACY_SETTING_STABILIZE_INTERVAL,
                "3",
            )
        }),
        storage_name: use_state(|| {
            load_setting_or_default(
                extension::SETTING_STORAGE_NAME,
                extension::LEGACY_SETTING_STORAGE_NAME,
                "rings-frontend",
            )
        }),
        peers: use_state(Vec::<PeerView>::new),
        seed_url: use_state(|| {
            extension::load_setting_with_legacy(
                extension::SETTING_SEED_URL,
                extension::LEGACY_SETTING_SEED_URL,
            )
            .unwrap_or_default()
        }),
    }
}

#[hook]
fn use_link_state() -> LinkState {
    LinkState {
        http_endpoint: use_state(|| {
            load_setting_or_default(
                extension::SETTING_HTTP_ENDPOINT,
                extension::LEGACY_SETTING_HTTP_ENDPOINT,
                "http://127.0.0.1:50001",
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

#[hook]
fn use_proof_state() -> ProofState {
    ProofState {
        prover_did: use_state(String::new),
        r1cs_url: use_state(|| "http://127.0.0.1:8080/simple_bn256.r1cs".to_string()),
        wasm_url: use_state(|| "http://127.0.0.1:8080/simple_bn256.wasm".to_string()),
    }
}

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

#[hook]
fn use_onion_state() -> OnionState {
    OnionState {
        url: use_state(|| "https://example.com/".to_string()),
        method: use_state(|| "GET".to_string()),
        hop_count: use_state(|| "3".to_string()),
        allow_short_paths: use_state(|| true),
        headers: use_state(String::new),
        body: use_state(String::new),
        route_result: use_state(String::new),
        response_status: use_state(|| "idle".to_string()),
        response_headers: use_state(String::new),
        response_body: use_state(String::new),
    }
}

fn initial_wallet_kind() -> WalletKind {
    extension::load_setting_with_legacy(
        extension::SETTING_WALLET_KIND,
        extension::LEGACY_SETTING_WALLET_KIND,
    )
    .map(|value| WalletKind::from_value(&value))
    .unwrap_or(WalletKind::WebCrypto)
}

fn load_setting_or_default(key: &str, legacy_key: &str, default: &'static str) -> String {
    extension::load_setting_with_legacy(key, legacy_key).unwrap_or_else(|| default.to_string())
}

/// Rings browser frontend app.
#[function_component(App)]
pub fn app() -> Html {
    let shell = use_shell_state();
    let node = use_node_state();
    let link = use_link_state();
    let proof = use_proof_state();
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
    use_shell_history(shell.active_page.clone());
    render_app(AppRenderContext {
        shell: &shell,
        node: &node,
        link: &link,
        proof: &proof,
        custom_state: &custom_state,
        onion: &onion,
        launch_actions,
        extension_mode,
    })
}

fn wallet_kind_callback(wallet_kind: &UseStateHandle<WalletKind>) -> Callback<Event> {
    let wallet_kind = wallet_kind.clone();
    Callback::from(move |event: Event| {
        if let Some(value) = select_value(&event) {
            wallet_kind.set(WalletKind::from_value(&value));
        }
    })
}

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

#[hook]
fn use_extension_status(node: &NodeState) {
    let did = node.did.clone();
    let peers = node.peers.clone();
    let wallet_account = node.wallet_account.clone();
    let status = node.status.clone();
    use_effect_with((), move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            let Some(bridge) = extension::extension_node_bridge() else {
                return;
            };
            match extension::extension_node_status(&bridge).await {
                Ok(snapshot) if snapshot.online => {
                    did.set(snapshot.did);
                    peers.set(snapshot.peers);
                    wallet_account.set(snapshot.wallet_account);
                    status.set("background node active".to_string());
                }
                Ok(_) => {}
                Err(error) => status.set(format!("background status failed: {error}")),
            }
        });
    });
}

#[hook]
fn use_peer_refresh(node: &NodeState) {
    let node_ref = node.node_ref.clone();
    let generation = node.generation.clone();
    let peers = node.peers.clone();
    let did = node.did.clone();
    let wallet_account = node.wallet_account.clone();
    let node_starting = node.node_starting.clone();
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
                );
            }))
        } else {
            None
        };
        move || drop(interval)
    });
}

fn refresh_peer_snapshot(
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    peers: UseStateHandle<Vec<PeerView>>,
    did: UseStateHandle<String>,
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    node_starting: UseStateHandle<bool>,
) {
    if let Some(bridge) = extension::extension_node_bridge() {
        refresh_extension_snapshot(
            bridge,
            generation,
            peers,
            did,
            wallet_account,
            node_starting,
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
                peers.set(next);
            }
        }
    });
}

fn refresh_extension_snapshot(
    bridge: JsValue,
    generation: GenerationClock,
    peers: UseStateHandle<Vec<PeerView>>,
    did: UseStateHandle<String>,
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    node_starting: UseStateHandle<bool>,
) {
    let refresh_token = generation.token();
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(snapshot) = extension::extension_node_status(&bridge).await {
            if !refresh_token.is_current() {
                return;
            }
            if snapshot.online {
                did.set(snapshot.did);
                peers.set(snapshot.peers);
                wallet_account.set(snapshot.wallet_account);
            }
            node_starting.set(snapshot.starting);
        }
    });
}

#[hook]
fn use_extension_panel_reset(active_panel: UseStateHandle<Panel>, extension_mode: bool) {
    use_effect_with(extension_mode, move |extension_mode| {
        if *extension_mode {
            active_panel.set(Panel::Onion);
        }
        || {}
    });
}

#[hook]
fn use_shell_history(active_page: UseStateHandle<ShellPage>) {
    use_effect_with((), move |_| {
        let listener = web_sys::window().map(|window| {
            let page = active_page.clone();
            let listener = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_| {
                page.set(current_shell_page());
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

fn render_app(ctx: AppRenderContext<'_>) -> Html {
    let effective_page = effective_shell_page(ctx.shell, ctx.extension_mode);
    let navigate_page = navigate_page_callback(&ctx.shell.active_page);
    let header = controls::app_header(effective_page, navigate_page.clone(), !ctx.extension_mode);
    if effective_page == ShellPage::Guide {
        render_guide_shell(header, navigate_page, ctx.shell)
    } else {
        render_console_shell(ctx, header)
    }
}

fn effective_shell_page(shell: &ShellState, extension_mode: bool) -> ShellPage {
    if extension_mode {
        ShellPage::Console
    } else {
        *shell.active_page
    }
}

fn navigate_page_callback(active_page: &UseStateHandle<ShellPage>) -> Callback<ShellPage> {
    let active_page = active_page.clone();
    Callback::from(move |page| navigate_shell_page(page, &active_page))
}

fn render_guide_shell(
    header: Html,
    navigate_page: Callback<ShellPage>,
    shell: &ShellState,
) -> Html {
    html! {
        <main class="app-shell guide-shell">
            <style>{ styles::app_css() }</style>
            { header }
            { guide::page(navigate_page, shell.active_architecture_layer.clone()) }
        </main>
    }
}

fn render_console_shell(ctx: AppRenderContext<'_>, header: Html) -> Html {
    let link_control = render_link_control(ctx.node, ctx.link, ctx.shell);
    let workbench_body = render_workbench_body(&ctx);
    let workbench_control = controls::workbench_control(
        *ctx.shell.active_panel,
        ctx.shell.active_panel.clone(),
        ctx.shell.active_dialog.clone(),
        workbench_body,
        true,
        ctx.extension_mode,
    );
    let control_sidebar = controls::control_sidebar(
        control_view(ctx.node),
        ctx.launch_actions,
        workbench_control,
        ctx.shell.active_dialog.clone(),
        ctx.shell.control_sidebar_collapsed.clone(),
        ctx.extension_mode,
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

fn console_shell_class(extension_mode: bool) -> &'static str {
    if extension_mode {
        "app-shell topology-shell extension-mode"
    } else {
        "app-shell topology-shell"
    }
}

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

fn session_view(node: &NodeState) -> SessionView<'_> {
    SessionView {
        wallet_account: (*node.wallet_account).clone(),
        did: &node.did,
        peers: &node.peers,
    }
}

fn render_link_control(node: &NodeState, link: &LinkState, shell: &ShellState) -> Html {
    connect::link_control(
        connect_state(link, shell),
        node.node_ref.clone(),
        node.generation.clone(),
        node.peers.clone(),
        node.status.clone(),
    )
}

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

fn render_workbench_body(ctx: &AppRenderContext<'_>) -> Html {
    match *ctx.shell.active_panel {
        Panel::Onion => render_onion_panel(ctx.node, ctx.onion),
        Panel::Proof => render_proof_panel(ctx.node, ctx.proof),
        Panel::Custom => render_custom_panel(ctx.node, ctx.custom_state),
    }
}

fn render_onion_panel(node: &NodeState, onion: &OnionState) -> Html {
    workbench::onion_proxy_panel(
        workbench::OnionProxyState {
            url: &onion.url,
            method: &onion.method,
            hop_count: &onion.hop_count,
            allow_short_paths: &onion.allow_short_paths,
            headers: &onion.headers,
            body: &onion.body,
            route_result: &onion.route_result,
            response_status: &onion.response_status,
            response_headers: &onion.response_headers,
            response_body: &onion.response_body,
        },
        node.node_ref.clone(),
        node.status.clone(),
    )
}

fn render_proof_panel(node: &NodeState, proof: &ProofState) -> Html {
    workbench::proof_panel(
        &proof.prover_did,
        &proof.r1cs_url,
        &proof.wasm_url,
        node.node_ref.clone(),
        node.status.clone(),
    )
}

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

fn initial_shell_page() -> ShellPage {
    if extension::extension_node_bridge().is_some() {
        return ShellPage::Console;
    }
    match routed_shell_page() {
        Some(page) => page,
        None => ShellPage::Guide,
    }
}

fn current_shell_page() -> ShellPage {
    if extension::extension_node_bridge().is_some() {
        return ShellPage::Console;
    }
    routed_shell_page().unwrap_or(ShellPage::Guide)
}

fn routed_shell_page() -> Option<ShellPage> {
    let hash = web_sys::window()?.location().hash().ok()?;
    match hash.trim_start_matches('#').trim_start_matches('/') {
        "" | "home" => Some(ShellPage::Guide),
        "node" => Some(ShellPage::Console),
        _ => None,
    }
}

fn navigate_shell_page(page: ShellPage, active_page: &UseStateHandle<ShellPage>) {
    if **active_page == page && routed_shell_page() == Some(page) {
        return;
    }
    push_shell_page(page);
    active_page.set(page);
}

fn push_shell_page(page: ShellPage) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(target) = shell_page_url(&window, page) else {
        return;
    };
    if current_path_search_hash(&window) == Some(target.clone()) {
        return;
    }
    let Ok(history) = window.history() else {
        return;
    };
    let _ = history.push_state_with_url(&JsValue::NULL, "", Some(&target));
}

fn shell_page_url(window: &Window, page: ShellPage) -> Option<String> {
    let location = window.location();
    let mut target = location.pathname().ok()?;
    if let Ok(search) = location.search() {
        target.push_str(&search);
    }
    if page == ShellPage::Console {
        target.push_str("#node");
    }
    Some(target)
}

fn current_path_search_hash(window: &Window) -> Option<String> {
    let location = window.location();
    let mut current = location.pathname().ok()?;
    if let Ok(search) = location.search() {
        current.push_str(&search);
    }
    if let Ok(hash) = location.hash() {
        current.push_str(&hash);
    }
    Some(current)
}
