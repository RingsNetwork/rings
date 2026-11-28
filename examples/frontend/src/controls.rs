//! Control sidebar, settings dialog, and shell UI.

use web_sys::Event;
use web_sys::MouseEvent;
use yew::prelude::*;

use crate::browser_api::js_global_prop;
use crate::browser_api::js_string_field;
use crate::extension;
use crate::forms::text_input;
use crate::forms::text_input_with_suggestions;
use crate::links::ProjectLink;
use crate::node::PeerView;
use crate::node::SEED_ENDPOINTS;
use crate::topology;
use crate::wallet::WalletAccount;
use crate::wallet::WalletKind;

mod sidebar;
pub(crate) use sidebar::control_sidebar;
pub(crate) use sidebar::ControlSidebarShell;

/// Chrome internal URL for inspecting live WebRTC peer connections.
const CHROME_WEBRTC_DEBUG_URL: &str = "chrome://webrtc-internals/";
/// Firefox internal URL for inspecting live WebRTC peer connections.
const FIREFOX_WEBRTC_DEBUG_URL: &str = "about:webrtc";
/// Chrome internal URL for managing installed browser extensions.
const CHROME_EXTENSION_MANAGER_URL: &str = "chrome://extensions/";
/// Firefox internal URL for debugging installed browser extensions.
const FIREFOX_EXTENSION_MANAGER_URL: &str = "about:debugging#/runtime/this-firefox";

/// Top-level pages routed within the frontend application shell.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ShellPage {
    /// Marketing and quick-start landing page.
    Home,
    /// Documentation and setup guide page.
    Guide,
    /// Interactive node topology and control console.
    Console,
    /// Controlled browser WebView surface.
    Webview,
}

impl ShellPage {
    /// The pages the header navigates between, in presentation order. The console is entered
    /// from the landing page's calls to action and the WebView from the console, so neither is
    /// a header destination.
    pub(crate) const HEADER: [Self; 2] = [Self::Home, Self::Guide];

    /// Return the human-readable navigation label for this page.
    fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Guide => "Guide",
            Self::Console => "Node",
            Self::Webview => "WebView",
        }
    }

    /// The page's name in URLs and DOM keys: the hash fragment that addresses it (the landing
    /// page's is the empty fragment, see `app::routes`) and the identity of its document.
    pub(crate) const fn slug(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Guide => "guide",
            Self::Console => "node",
            Self::Webview => "webview",
        }
    }

    /// The page as a navigation button of the given class; `current` marks the page the
    /// surface is showing (`aria-current`). The header and the footer both render pages this
    /// way, so a page's label and its click behaviour are defined once
    /// (`button : ShellPage × Class × Bool → Html`).
    pub(crate) fn button(
        self,
        class: &'static str,
        current: bool,
        navigate_page: &Callback<ShellPage>,
    ) -> Html {
        let navigate_page = navigate_page.clone();
        let onclick = Callback::from(move |_| navigate_page.emit(self));
        html! {
            <button
                class={class}
                type="button"
                aria-current={if current { "page" } else { "false" }}
                onclick={onclick}
            >
                { self.label() }
            </button>
        }
    }
}

/// Active tab panel inside the node workbench modal.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Panel {
    /// Onion-routed HTTP proxy panel.
    Onion,
    /// Custom namespace messaging panel.
    Custom,
}

impl Panel {
    /// Return the display label for this workbench panel.
    fn label(self) -> &'static str {
        match self {
            Self::Onion => "Onion Proxy",
            Self::Custom => "Custom",
        }
    }
}

/// Currently open modal dialog in the console shell.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ActiveDialog {
    /// No modal dialog is open.
    None,
    /// Node settings and identity dialog.
    Settings,
    /// Node workbench dialog (Onion Proxy / Custom protocols).
    Workbench,
}

impl ActiveDialog {
    /// Return `true` when any modal dialog is currently open.
    pub(crate) fn is_open(self) -> bool {
        self != Self::None
    }
}

/// Inline SVG icon identifiers used across shell controls.
#[derive(Clone, Copy, Eq, PartialEq)]
enum UiIcon {
    /// Start-node power icon.
    Power,
    /// Stop-node power-off icon.
    PowerOff,
    /// Workbench terminal icon.
    Terminal,
    /// WebView globe icon.
    Globe,
    /// Settings sliders icon.
    Sliders,
    /// Expand-sidebar icon.
    PanelOpen,
    /// Collapse-sidebar icon.
    PanelClose,
}

/// Render an inline SVG element for the requested [`UiIcon`].
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

/// Callbacks wired to node lifecycle and wallet selection controls.
#[derive(Clone)]
pub(crate) struct LaunchActions {
    /// Callback invoked when the user selects a different wallet standard.
    pub(crate) on_wallet_kind: Callback<Event>,
    /// Callback invoked when the user clicks Start.
    pub(crate) on_start: Callback<MouseEvent>,
    /// Callback invoked when the user clicks Stop.
    pub(crate) on_disconnect: Callback<MouseEvent>,
}

/// Callbacks for opening and closing shell modal dialogs.
#[derive(Clone)]
pub(crate) struct DialogActions {
    /// Open the requested [`ActiveDialog`].
    pub(crate) open: Callback<ActiveDialog>,
    /// Close whichever modal dialog is currently open.
    pub(crate) close: Callback<MouseEvent>,
}

/// Borrowed state handles rendered by the control sidebar and settings dialog.
pub(crate) struct ControlView<'a> {
    /// Currently selected wallet standard.
    pub(crate) wallet_kind: WalletKind,
    /// Connected wallet account metadata, if authorized.
    pub(crate) wallet_account: Option<WalletAccount>,
    /// Whether the node is currently starting.
    pub(crate) node_starting: bool,
    /// Local node DID state handle.
    pub(crate) did: &'a UseStateHandle<String>,
    /// Current status line text handle.
    pub(crate) status: &'a UseStateHandle<String>,
    /// Connected peer list handle.
    pub(crate) peers: &'a UseStateHandle<Vec<PeerView>>,
    /// Configured network ID input handle.
    pub(crate) network_id: &'a UseStateHandle<String>,
    /// Configured ICE servers input handle.
    pub(crate) ice_servers: &'a UseStateHandle<String>,
    /// Configured stabilization interval input handle.
    pub(crate) stabilize_interval: &'a UseStateHandle<String>,
    /// Configured IndexedDB storage name input handle.
    pub(crate) storage_name: &'a UseStateHandle<String>,
    /// Configured seed HTTP URL input handle.
    pub(crate) seed_url: &'a UseStateHandle<String>,
}

/// Borrowed session identity and peer state displayed in the network topology stage.
pub(crate) struct SessionView<'a> {
    /// Connected wallet account metadata, if authorized.
    pub(crate) wallet_account: Option<WalletAccount>,
    /// Local node DID state handle.
    pub(crate) did: &'a UseStateHandle<String>,
    /// Connected peer list handle.
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

/// View model passed to [`settings_dialog`].
struct SettingsDialogView<'a> {
    /// Currently selected wallet standard.
    wallet_kind: WalletKind,
    /// Lifecycle and wallet selection callbacks.
    actions: LaunchActions,
    /// Configured network ID input handle.
    network_id: &'a UseStateHandle<String>,
    /// Configured ICE servers input handle.
    ice_servers: &'a UseStateHandle<String>,
    /// Configured stabilization interval input handle.
    stabilize_interval: &'a UseStateHandle<String>,
    /// Configured IndexedDB storage name input handle.
    storage_name: &'a UseStateHandle<String>,
    /// Configured seed HTTP URL input handle.
    seed_url: &'a UseStateHandle<String>,
    /// Current status line text handle.
    status: &'a UseStateHandle<String>,
    /// Display string for the local node DID.
    did_value: String,
    /// Callback that copies the local DID to the clipboard.
    on_copy_did: Callback<MouseEvent>,
    /// Whether copying the local DID is enabled.
    can_copy_did: bool,
    /// Connected wallet account metadata, if authorized.
    wallet_account: Option<WalletAccount>,
    /// Callback that dismisses the settings dialog.
    close_dialog: Callback<MouseEvent>,
}

/// Render the top application header with brand mark and optional primary navigation.
pub(crate) fn app_header(
    active_page: ShellPage,
    navigate_page: Callback<ShellPage>,
    show_nav: bool,
) -> Html {
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
                    <span>{ "A P2P network for the sovereign age." }</span>
                </div>
            </div>
            if show_nav {
                <nav class="header-nav" aria-label="Primary">
                    { for ShellPage::HEADER.into_iter().map(|page| {
                        page.button(
                            header_page_class(active_page, page),
                            active_page == page,
                            &navigate_page,
                        )
                    }) }
                    { for ProjectLink::HEADER.into_iter().map(|link| link.anchor("header-external-link")) }
                </nav>
            }
        </header>
    }
}

/// Return the CSS class for a header navigation button, marking the active page.
fn header_page_class(active: ShellPage, page: ShellPage) -> &'static str {
    if active == page {
        "header-nav-button active"
    } else {
        "header-nav-button"
    }
}

/// Render the modal dialog for wallet standard, runtime settings, and local node identity.
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

/// Render the WorkBench trigger button and, when open, its tabbed modal dialog.
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

/// Render the main Chord ring topology stage, HUD telemetry, sidebar, and status line.
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
                    <p class="eyebrow">{ "Network / Chord ring" }</p>
                </div>
                <div class="session-strip" aria-label="Delegation summary">
                    { local_did_metric(did_label, on_copy_did, can_copy_did) }
                    { metric("Delegation", account_label.to_string()) }
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

/// Render the tab bar switching between [`Panel`] views inside the workbench modal.
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
                    "Custom namespace protocols run in the web console; extension mode exposes Onion Proxy and WebView."
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

/// Render a labelled key-value telemetry metric tile.
pub(crate) fn metric(label: &'static str, value: String) -> Html {
    html! {
        <div class="metric">
            <span>{ label }</span>
            <strong>{ value }</strong>
        </div>
    }
}

/// Render the local DID metric tile with an inline copy button.
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

/// Build a click callback that copies the local node DID to the clipboard and updates `status`.
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

/// Render a single label/value row inside a sidebar telemetry rail card.
fn rail_row(label: &'static str, value: String) -> Html {
    html! {
        <div class="rail-row">
            <span>{ label }</span>
            <strong>{ value }</strong>
        </div>
    }
}

/// Render a read-only monospace identity field inside the settings dialog.
fn identity_value(label: &'static str, value: String) -> Html {
    html! {
        <div class="identity-value">
            <span>{ label }</span>
            <code>{ value }</code>
        </div>
    }
}

/// Render a monospace identity field paired with a copy-to-clipboard button.
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

/// Render authorized wallet account details when a wallet account is connected.
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

/// Render the account standard `<select>` dropdown for choosing a [`WalletKind`].
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

/// Render the runtime configuration inputs and browser debug shortcuts.
fn settings_controls(
    network_id: &UseStateHandle<String>,
    ice_servers: &UseStateHandle<String>,
    stabilize_interval: &UseStateHandle<String>,
    storage_name: &UseStateHandle<String>,
    seed_url: &UseStateHandle<String>,
    status: &UseStateHandle<String>,
) -> Html {
    html! {
        <>
            { text_input_with_suggestions("Seed URL", seed_url.clone(), "seed-url-endpoints", &SEED_ENDPOINTS) }
            { text_input("Network ID", network_id.clone()) }
            { text_input("ICE servers", ice_servers.clone()) }
            { text_input("Stabilize interval seconds", stabilize_interval.clone()) }
            { text_input("IndexedDB storage", storage_name.clone()) }
            { webrtc_debug_controls(status) }
        </>
    }
}

/// Render buttons that open or copy browser-internal WebRTC and extension debug URLs.
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

/// Browser engine family detected from `navigator.userAgent`.
#[derive(Clone, Copy, Eq, PartialEq)]
enum BrowserKind {
    /// Chromium-based browser.
    Chrome,
    /// Firefox browser.
    Firefox,
    /// Unrecognized or unavailable browser user agent.
    Unknown,
}

/// Browser-internal diagnostic page that can be opened from settings.
#[derive(Clone, Copy, Eq, PartialEq)]
enum DebugTarget {
    /// WebRTC internals diagnostic page.
    WebRtc,
    /// Installed extensions management page.
    ExtensionManager,
}

impl BrowserKind {
    /// Return a human-readable browser name for status messages.
    fn label(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::Firefox => "Firefox",
            Self::Unknown => "current browser",
        }
    }
}

impl DebugTarget {
    /// Return a human-readable target name for status messages.
    fn label(self) -> &'static str {
        match self {
            Self::WebRtc => "WebRTC debug",
            Self::ExtensionManager => "extension manager",
        }
    }
}

/// Build a click callback that opens or copies the browser-specific debug URL for `target`.
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

/// Map a `(browser, target)` pair to its browser-internal debug URL, if supported.
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

/// Detect the current [`BrowserKind`] from `navigator.userAgent`.
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

/// Read `navigator.userAgent` from the global JS environment.
fn navigator_user_agent() -> Result<String, String> {
    let navigator = js_global_prop("navigator")?;
    js_string_field(&navigator, "userAgent")
}
