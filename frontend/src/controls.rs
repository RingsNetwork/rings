//! Control sidebar, settings dialog, and shell UI.

use wasm_bindgen::JsCast;
use web_sys::Event;
use web_sys::HtmlInputElement;
use web_sys::MouseEvent;
use yew::prelude::*;

use crate::browser_api::js_global_prop;
use crate::browser_api::js_string_field;
use crate::extension;
use crate::forms::text_input;
use crate::node::PeerView;
use crate::topology;
use crate::wallet::WalletAccount;
use crate::wallet::WalletKind;

mod sidebar;
pub(crate) use sidebar::control_sidebar;
pub(crate) use sidebar::ControlSidebarShell;

const CHROME_WEBRTC_DEBUG_URL: &str = "chrome://webrtc-internals/";
const FIREFOX_WEBRTC_DEBUG_URL: &str = "about:webrtc";
const CHROME_EXTENSION_MANAGER_URL: &str = "chrome://extensions/";
const FIREFOX_EXTENSION_MANAGER_URL: &str = "about:debugging#/runtime/this-firefox";

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ShellPage {
    Guide,
    Console,
    Webview,
}

impl ShellPage {
    fn label(self) -> &'static str {
        match self {
            Self::Guide => "Home",
            Self::Console => "Node",
            Self::Webview => "WebView",
        }
    }
}

/// Destinations outside the shell that the landing hero and the header both link to, in
/// presentation order. One finite index set, rendered by [`ProjectLink::anchor`] into whichever
/// class the surrounding surface styles: the set of links is defined once, the surfaces differ
/// only in their class (`anchor : ProjectLink × Class → Html`).
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ProjectLink {
    Docs,
    Repository,
    Whitepaper,
}

impl ProjectLink {
    /// Every link, in the order the surfaces present them.
    pub(crate) const ALL: [Self; 3] = [Self::Docs, Self::Repository, Self::Whitepaper];

    fn label(self) -> &'static str {
        match self {
            Self::Docs => "Docs",
            Self::Repository => "GitHub",
            Self::Whitepaper => "Whitepaper",
        }
    }

    /// Absolute URLs: the same shell runs from `rings.rs` and from an extension origin, where a
    /// site-relative `/docs/` would resolve inside the extension.
    fn href(self) -> &'static str {
        match self {
            Self::Docs => "https://rings.rs/docs/",
            Self::Repository => "https://github.com/RingsNetwork/rings",
            Self::Whitepaper => {
                "https://github.com/RingsNetwork/rings/blob/master/papers/rings.pdf"
            }
        }
    }

    /// The link as an anchor of the given class, opened in a new tab without a referrer.
    pub(crate) fn anchor(self, class: &'static str) -> Html {
        html! {
            <a class={class} href={self.href()} target="_blank" rel="noreferrer">
                { self.label() }
            </a>
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Panel {
    Onion,
    Custom,
}

impl Panel {
    fn label(self) -> &'static str {
        match self {
            Self::Onion => "Onion Proxy",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ActiveDialog {
    None,
    Settings,
    Workbench,
}

impl ActiveDialog {
    pub(crate) fn is_open(self) -> bool {
        self != Self::None
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum UiIcon {
    Power,
    PowerOff,
    Terminal,
    Globe,
    Sliders,
    PanelOpen,
    PanelClose,
}

fn ui_icon(icon: UiIcon) -> Html {
    let content = match icon {
        UiIcon::Power => html! {
            <>
                <path d="M12 3.5v7" />
                <path d="M7.2 6.8a7 7 0 1 0 9.6 0" />
            </>
        },
        UiIcon::PowerOff => html! {
            <>
                <path d="M4.5 4.5l15 15" />
                <path d="M12 3.5v4.6" />
                <path d="M8.1 7.3a7 7 0 1 0 8.2.3" />
            </>
        },
        UiIcon::Terminal => html! {
            <>
                <rect x="4.5" y="5" width="15" height="14" rx="2.2" />
                <path d="M8 10l2.6 2L8 14" />
                <path d="M13 15h3.5" />
            </>
        },
        UiIcon::Globe => html! {
            <>
                <circle cx="12" cy="12" r="8" />
                <path d="M4 12h16" />
                <path d="M12 4c2.1 2.2 3.2 4.9 3.2 8S14.1 17.8 12 20" />
                <path d="M12 4C9.9 6.2 8.8 8.9 8.8 12s1.1 5.8 3.2 8" />
            </>
        },
        UiIcon::Sliders => html! {
            <>
                <path d="M5 6h14" />
                <path d="M5 12h14" />
                <path d="M5 18h14" />
                <circle cx="9" cy="6" r="1.7" />
                <circle cx="15" cy="12" r="1.7" />
                <circle cx="11.5" cy="18" r="1.7" />
            </>
        },
        UiIcon::PanelOpen => html! {
            <>
                <rect x="4.5" y="5" width="15" height="14" rx="2.2" />
                <path d="M14.5 5v14" />
                <path d="M13 9l-3 3 3 3" />
            </>
        },
        UiIcon::PanelClose => html! {
            <>
                <rect x="4.5" y="5" width="15" height="14" rx="2.2" />
                <path d="M14.5 5v14" />
                <path d="M9.5 9l3 3-3 3" />
            </>
        },
    };

    html! {
        <svg
            class="ui-icon"
            viewBox="0 0 24 24"
            aria-hidden="true"
            focusable="false"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
            stroke-linejoin="round"
        >
            { content }
        </svg>
    }
}

#[derive(Clone)]
pub(crate) struct LaunchActions {
    pub(crate) on_wallet_kind: Callback<Event>,
    pub(crate) on_start: Callback<MouseEvent>,
    pub(crate) on_disconnect: Callback<MouseEvent>,
}

#[derive(Clone)]
pub(crate) struct DialogActions {
    pub(crate) open: Callback<ActiveDialog>,
    pub(crate) close: Callback<MouseEvent>,
}

pub(crate) struct ControlView<'a> {
    pub(crate) wallet_kind: WalletKind,
    pub(crate) wallet_account: Option<WalletAccount>,
    pub(crate) node_starting: bool,
    pub(crate) did: &'a UseStateHandle<String>,
    pub(crate) status: &'a UseStateHandle<String>,
    pub(crate) peers: &'a UseStateHandle<Vec<PeerView>>,
    pub(crate) network_id: &'a UseStateHandle<String>,
    pub(crate) ice_servers: &'a UseStateHandle<String>,
    pub(crate) stabilize_interval: &'a UseStateHandle<String>,
    pub(crate) storage_name: &'a UseStateHandle<String>,
    pub(crate) webview_allow_short_paths: &'a UseStateHandle<bool>,
    pub(crate) seed_url: &'a UseStateHandle<String>,
}

pub(crate) struct SessionView<'a> {
    pub(crate) wallet_account: Option<WalletAccount>,
    pub(crate) did: &'a UseStateHandle<String>,
    pub(crate) peers: &'a UseStateHandle<Vec<PeerView>>,
}

/// Render the Node-only entry point for the controlled browser WebView.
pub(crate) fn webview_control(
    ready: bool,
    unavailable_reason: String,
    on_open: Callback<MouseEvent>,
) -> Html {
    let title = if ready {
        "Open WebView".to_string()
    } else {
        unavailable_reason
    };
    html! {
        <button
            class="secondary action-button command-button webview-button"
            type="button"
            aria-label="Open WebView"
            title={title}
            disabled={!ready}
            onclick={on_open}
        >
            <span class="label-desktop">{ "WebView" }</span>
            <span class="label-mobile command-icon" aria-hidden="true">
                { ui_icon(UiIcon::Globe) }
                <span class="command-caption">{ "WebView" }</span>
            </span>
        </button>
    }
}

struct SettingsDialogView<'a> {
    wallet_kind: WalletKind,
    actions: LaunchActions,
    network_id: &'a UseStateHandle<String>,
    ice_servers: &'a UseStateHandle<String>,
    stabilize_interval: &'a UseStateHandle<String>,
    storage_name: &'a UseStateHandle<String>,
    webview_allow_short_paths: &'a UseStateHandle<bool>,
    seed_url: &'a UseStateHandle<String>,
    status: &'a UseStateHandle<String>,
    did_value: String,
    on_copy_did: Callback<MouseEvent>,
    can_copy_did: bool,
    wallet_account: Option<WalletAccount>,
    close_dialog: Callback<MouseEvent>,
}

pub(crate) fn app_header(
    active_page: ShellPage,
    navigate_page: Callback<ShellPage>,
    show_nav: bool,
) -> Html {
    let guide_class = header_page_class(active_page, ShellPage::Guide);
    let console_class = header_page_class(active_page, ShellPage::Console);
    let open_guide = {
        let navigate_page = navigate_page.clone();
        Callback::from(move |_| navigate_page.emit(ShellPage::Guide))
    };
    let open_console = {
        let navigate_page = navigate_page.clone();
        Callback::from(move |_| navigate_page.emit(ShellPage::Console))
    };
    html! {
        <header class="app-header landing-header">
            <div class="landing-header-brand" aria-label="Rings Network">
                <span class="landing-header-mark" aria-hidden="true">
                    <img
                        class="landing-header-logo"
                        src="assets/icons/rings.svg"
                        alt=""
                        decoding="async"
                    />
                </span>
                <div>
                    <strong>{ "Rings Network" }</strong>
                    <span>{ "Browser-native P2P" }</span>
                </div>
            </div>
            if show_nav {
                <nav class="header-nav" aria-label="Primary">
                    <button
                        class={guide_class}
                        type="button"
                        aria-current={if active_page == ShellPage::Guide { "page" } else { "false" }}
                        onclick={open_guide}
                    >
                        { ShellPage::Guide.label() }
                    </button>
                    <button
                        class={console_class}
                        type="button"
                        aria-current={if active_page == ShellPage::Console { "page" } else { "false" }}
                        onclick={open_console}
                    >
                        { ShellPage::Console.label() }
                    </button>
                    { for ProjectLink::ALL.into_iter().map(|link| link.anchor("header-external-link")) }
                </nav>
            }
        </header>
    }
}

fn header_page_class(active: ShellPage, page: ShellPage) -> &'static str {
    if active == page {
        "header-nav-button active"
    } else {
        "header-nav-button"
    }
}

fn settings_dialog(view: SettingsDialogView<'_>) -> Html {
    let close_dialog = view.close_dialog;
    html! {
        <div class="modal-shell">
            <button class="dialog-backdrop" aria-label="Close settings" onclick={close_dialog.clone()}></button>
            <section class="link-dialog setup-dialog" role="dialog" aria-modal="true" aria-labelledby="settings-dialog-title">
                <header class="dialog-header">
                    <div>
                        <p class="eyebrow">{ "Node settings" }</p>
                        <h2 id="settings-dialog-title">{ "Settings" }</h2>
                    </div>
                    <button class="secondary dialog-close" onclick={close_dialog}>{ "Close" }</button>
                </header>
                <div class="dialog-body">
                    <div class="dialog-pane setup-pane">
                        <div class="setup-grid">
                            <section class="node-control-group setup-launch-section">
                                <div class="tool-header compact">
                                    <div>
                                        <p class="eyebrow">{ "Account" }</p>
                                        <h3>{ "Standard" }</h3>
                                    </div>
                                </div>
                                { wallet_provider_control(
                                    view.wallet_kind,
                                    view.actions.on_wallet_kind.clone(),
                                ) }
                            </section>
                            <section class="node-control-group setup-runtime-section">
                                <div class="tool-header compact">
                                    <div>
                                        <p class="eyebrow">{ "Runtime" }</p>
                                        <h3>{ "Settings" }</h3>
                                    </div>
                                </div>
                                { settings_controls(
                                    view.network_id,
                                    view.ice_servers,
                                    view.stabilize_interval,
                                    view.storage_name,
                                    view.webview_allow_short_paths,
                                    view.seed_url,
                                    view.status,
                                ) }
                            </section>
                            <section class="node-control-group setup-identity">
                                <div class="tool-header compact">
                                    <div>
                                        <p class="eyebrow">{ "Identity" }</p>
                                        <h3>{ "Local node" }</h3>
                                    </div>
                                </div>
                                { copyable_identity_value(
                                    "DID",
                                    view.did_value,
                                    view.on_copy_did,
                                    view.can_copy_did,
                                ) }
                                { account_details(view.wallet_account) }
                            </section>
                        </div>
                    </div>
                </div>
            </section>
        </div>
    }
}

pub(crate) fn workbench_control(
    active: Panel,
    active_panel: UseStateHandle<Panel>,
    active_dialog: ActiveDialog,
    dialog_actions: DialogActions,
    body: Html,
    available: bool,
    extension_mode: bool,
) -> Html {
    let open_dialog = {
        let open_dialog = dialog_actions.open.clone();
        Callback::from(move |_| {
            if available {
                open_dialog.emit(ActiveDialog::Workbench);
            }
        })
    };
    let close_dialog = dialog_actions.close.clone();
    let button_class = if available {
        "secondary action-button command-button workbench-button"
    } else {
        "secondary action-button command-button workbench-button disabled"
    };
    let button_title = if available {
        "WorkBench"
    } else {
        "Start the node to use WorkBench"
    };
    html! {
        <div class="workbench-control">
            <button
                class={button_class}
                type="button"
                aria-label="WorkBench"
                title={button_title}
                disabled={!available}
                onclick={open_dialog}
            >
                <span class="label-desktop">{ "WorkBench" }</span>
                <span class="label-mobile command-icon" aria-hidden="true">
                    { ui_icon(UiIcon::Terminal) }
                    <span class="command-caption">{ "WorkBench" }</span>
                </span>
            </button>
            {
                if available && active_dialog == ActiveDialog::Workbench {
                    html! {
                        <div class="modal-shell">
                            <button class="dialog-backdrop" aria-label="Close workbench" onclick={close_dialog.clone()}></button>
                            <section class="link-dialog workbench-dialog" role="dialog" aria-modal="true" aria-labelledby="workbench-dialog-title">
                                <header class="dialog-header">
                                    <div>
                                        <p class="eyebrow">{ "Node workbench" }</p>
                                        <h2 id="workbench-dialog-title">{ active.label() }</h2>
                                    </div>
                                    <button class="secondary dialog-close" onclick={close_dialog}>{ "Close" }</button>
                                </header>
                                { workspace_tabs(active, active_panel, extension_mode) }
                                <div class="dialog-body workbench-dialog-body">
                                    { body }
                                </div>
                            </section>
                        </div>
                    }
                } else {
                    html! {}
                }
            }
        </div>
    }
}

pub(crate) fn network_stage(
    view: SessionView<'_>,
    status: &UseStateHandle<String>,
    link_control: Html,
    control_sidebar: Html,
) -> Html {
    let account_label = view
        .wallet_account
        .as_ref()
        .map(|account| account.account_type.as_str())
        .unwrap_or("not authorized");
    let account_standard = view
        .wallet_account
        .as_ref()
        .map(|account| account.kind.label().to_string())
        .unwrap_or_else(|| "none".to_string());
    let did_label = if (**view.did).is_empty() {
        "not started".to_string()
    } else {
        (**view.did).clone()
    };
    let node_label = if (**view.did).is_empty() {
        "offline".to_string()
    } else {
        "ready".to_string()
    };
    let transport_label = if view.peers.is_empty() {
        "standby".to_string()
    } else {
        "linked".to_string()
    };
    let can_copy_did = !(**view.did).is_empty();
    let on_copy_did = copy_local_did_callback(view.did, status);
    html! {
        <section class="network-stage topology-stage" aria-label="Network topology console">
            <div class="topology-hud">
                <div class="section-heading compact">
                    <p class="eyebrow">{ "Network / inferred" }</p>
                </div>
                <div class="session-strip" aria-label="Session summary">
                    { local_did_metric(did_label, on_copy_did, can_copy_did) }
                    { metric("Session", account_label.to_string()) }
                    { metric("Peers", view.peers.len().to_string()) }
                </div>
                <div class="mobile-telemetry-strip" aria-label="Mobile topology telemetry">
                    { metric("Node", node_label) }
                    { metric("Standard", account_standard) }
                    { metric("Transport", transport_label) }
                    { metric("Exchange", "SDP / HTTP".to_string()) }
                </div>
            </div>
            <div class="topology-layout">
                <div class="topology-wrap">
                    { link_control }
                    { topology::view((**view.did).as_str(), view.peers) }
                </div>
                { control_sidebar }
            </div>
            <div class="node-status-line" aria-label="Node status">
                <span>{ "Status" }</span>
                <p class="status">{ (**status).clone() }</p>
            </div>
        </section>
    }
}

fn workspace_tabs(
    active: Panel,
    active_panel: UseStateHandle<Panel>,
    extension_mode: bool,
) -> Html {
    html! {
        <nav class="workspace-tabs" aria-label="Node workspace">
            { for [Panel::Onion, Panel::Custom].into_iter().map(|panel| {
                let active_panel = active_panel.clone();
                let class = if panel == active { "workspace-tab active" } else { "workspace-tab" };
                let disabled = extension_mode && panel != Panel::Onion;
                let title = if disabled {
                    "This panel uses the page-local node; use Onion Proxy with the extension background node"
                } else {
                    panel.label()
                };
                html! {
                    <button
                        class={class}
                        disabled={disabled}
                        title={title}
                        onclick={Callback::from(move |_| {
                            if !disabled {
                                active_panel.set(panel);
                            }
                        })}
                    >
                        { panel.label() }
                    </button>
                }
            })}
        </nav>
    }
}

pub(crate) fn metric(label: &'static str, value: String) -> Html {
    html! {
        <div class="metric">
            <span>{ label }</span>
            <strong>{ value }</strong>
        </div>
    }
}

fn local_did_metric(value: String, on_copy: Callback<MouseEvent>, enabled: bool) -> Html {
    html! {
        <div class="metric local-did-metric copyable-metric">
            <div class="metric-label-row">
                <span>{ "Local DID" }</span>
                <button
                    class="copy-button metric-copy"
                    type="button"
                    aria-label="Copy local DID"
                    title="Copy local DID"
                    disabled={!enabled}
                    onclick={on_copy}
                >
                    { "Copy" }
                </button>
            </div>
            <strong title={value.clone()}>{ value }</strong>
        </div>
    }
}

fn copy_local_did_callback(
    did: &UseStateHandle<String>,
    status: &UseStateHandle<String>,
) -> Callback<MouseEvent> {
    let did = (**did).clone();
    let status = (*status).clone();
    Callback::from(move |_| {
        if did.trim().is_empty() {
            status.set("start the node first".to_string());
            return;
        }
        let did = did.clone();
        let status = status.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match extension::copy_text_to_clipboard(did).await {
                Ok(()) => status.set("local DID copied".to_string()),
                Err(error) => status.set(format!("copy DID failed: {error}")),
            }
        });
    })
}

fn rail_row(label: &'static str, value: String) -> Html {
    html! {
        <div class="rail-row">
            <span>{ label }</span>
            <strong>{ value }</strong>
        </div>
    }
}

fn identity_value(label: &'static str, value: String) -> Html {
    html! {
        <div class="identity-value">
            <span>{ label }</span>
            <code>{ value }</code>
        </div>
    }
}

fn copyable_identity_value(
    label: &'static str,
    value: String,
    on_copy: Callback<MouseEvent>,
    enabled: bool,
) -> Html {
    html! {
        <div class="identity-value copyable-identity">
            <span>{ label }</span>
            <code title={value.clone()}>{ value }</code>
            <button
                class="copy-button"
                type="button"
                aria-label="Copy local DID"
                title="Copy local DID"
                disabled={!enabled}
                onclick={on_copy}
            >
                { "Copy" }
            </button>
        </div>
    }
}

fn account_details(account: Option<WalletAccount>) -> Html {
    match account {
        Some(account) => html! {
            <div class="account-details">
                { identity_value("Standard", account.kind.label().to_string()) }
                { identity_value("Account type", account.account_type) }
                { identity_value("Account", account.account) }
            </div>
        },
        None => html! {},
    }
}

fn wallet_provider_control(wallet_kind: WalletKind, on_wallet_kind: Callback<Event>) -> Html {
    html! {
        <label class="field">
            <span>{ "Account standard" }</span>
            <select onchange={on_wallet_kind} value={wallet_kind.value()}>
                <option value="webcrypto" selected={wallet_kind == WalletKind::WebCrypto}>{ "WebCrypto P-256" }</option>
                <option value="eip191" selected={wallet_kind == WalletKind::EthereumEip191}>{ "Ethereum EIP-191" }</option>
                <option value="ed25519" selected={wallet_kind == WalletKind::SolanaEd25519}>{ "Solana Ed25519" }</option>
            </select>
        </label>
    }
}

fn settings_controls(
    network_id: &UseStateHandle<String>,
    ice_servers: &UseStateHandle<String>,
    stabilize_interval: &UseStateHandle<String>,
    storage_name: &UseStateHandle<String>,
    webview_allow_short_paths: &UseStateHandle<bool>,
    seed_url: &UseStateHandle<String>,
    status: &UseStateHandle<String>,
) -> Html {
    html! {
        <>
            { text_input("Seed URL", seed_url.clone()) }
            { text_input("Network ID", network_id.clone()) }
            { text_input("ICE servers", ice_servers.clone()) }
            { text_input("Stabilize interval seconds", stabilize_interval.clone()) }
            { text_input("IndexedDB storage", storage_name.clone()) }
            { webview_short_paths_control(webview_allow_short_paths.clone()) }
            { webrtc_debug_controls(status) }
        </>
    }
}

fn webview_short_paths_control(state: UseStateHandle<bool>) -> Html {
    let onchange = {
        let state = state.clone();
        Callback::from(move |event: Event| {
            if let Some(input) = event
                .target()
                .and_then(|target| target.dyn_into::<HtmlInputElement>().ok())
            {
                state.set(input.checked());
            }
        })
    };
    html! {
        <label class="field checkbox-field">
            <span>{ "Allow short WebView onion paths" }</span>
            <input type="checkbox" checked={*state} {onchange} />
        </label>
    }
}

fn webrtc_debug_controls(status: &UseStateHandle<String>) -> Html {
    let open_auto_webrtc = open_detected_debug_callback(DebugTarget::WebRtc, status.clone());
    let open_auto_extensions =
        open_detected_debug_callback(DebugTarget::ExtensionManager, status.clone());
    html! {
        <div class="debug-actions" aria-label="Debug shortcuts">
            <span>{ "Debug" }</span>
            <div class="debug-action-row">
                <button class="secondary" type="button" onclick={open_auto_webrtc}>{ "WebRTC dashboard" }</button>
                <button class="secondary" type="button" onclick={open_auto_extensions}>{ "Extension manager" }</button>
            </div>
        </div>
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BrowserKind {
    Chrome,
    Firefox,
    Unknown,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DebugTarget {
    WebRtc,
    ExtensionManager,
}

impl BrowserKind {
    fn label(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::Firefox => "Firefox",
            Self::Unknown => "current browser",
        }
    }
}

impl DebugTarget {
    fn label(self) -> &'static str {
        match self {
            Self::WebRtc => "WebRTC debug",
            Self::ExtensionManager => "extension manager",
        }
    }
}

fn open_detected_debug_callback(
    target: DebugTarget,
    status: UseStateHandle<String>,
) -> Callback<MouseEvent> {
    Callback::from(move |_| {
        let browser = detect_browser();
        let Some(url) = debug_url(browser, target) else {
            status.set(format!(
                "cannot detect supported browser for {}",
                target.label()
            ));
            return;
        };
        match extension::open_debug_url(url) {
            Ok(extension::DebugUrlOpenResult::Opened) => {
                status.set(format!("opened {} {}", browser.label(), target.label()));
            }
            Ok(extension::DebugUrlOpenResult::CopiedInternalUrl) => {
                status.set(format!(
                    "{} blocks direct {} links from webpages; URL copied, paste it into the address bar",
                    browser.label(),
                    target.label(),
                ));
            }
            Ok(extension::DebugUrlOpenResult::ManualInternalUrl) => {
                status.set(format!(
                    "{} blocks direct {} links from webpages; paste {url} into the address bar",
                    browser.label(),
                    target.label(),
                ));
            }
            Err(error) => status.set(format!(
                "open {} {} failed: {error}",
                browser.label(),
                target.label()
            )),
        }
    })
}

fn debug_url(browser: BrowserKind, target: DebugTarget) -> Option<&'static str> {
    match (browser, target) {
        (BrowserKind::Chrome, DebugTarget::WebRtc) => Some(CHROME_WEBRTC_DEBUG_URL),
        (BrowserKind::Chrome, DebugTarget::ExtensionManager) => Some(CHROME_EXTENSION_MANAGER_URL),
        (BrowserKind::Firefox, DebugTarget::WebRtc) => Some(FIREFOX_WEBRTC_DEBUG_URL),
        (BrowserKind::Firefox, DebugTarget::ExtensionManager) => {
            Some(FIREFOX_EXTENSION_MANAGER_URL)
        }
        (BrowserKind::Unknown, _) => None,
    }
}

fn detect_browser() -> BrowserKind {
    let user_agent = navigator_user_agent()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if user_agent.contains("firefox/") {
        BrowserKind::Firefox
    } else if user_agent.contains("chrome/") || user_agent.contains("chromium/") {
        BrowserKind::Chrome
    } else {
        BrowserKind::Unknown
    }
}

fn navigator_user_agent() -> Result<String, String> {
    let navigator = js_global_prop("navigator")?;
    js_string_field(&navigator, "userAgent")
}
