//! Collapsible node control sidebar, command deck, and telemetry rail.

use web_sys::MouseEvent;
use yew::prelude::*;

use super::copy_local_did_callback;
use super::rail_row;
use super::settings_dialog;
use super::ui_icon;
use super::ActiveDialog;
use super::ControlView;
use super::DialogActions;
use super::LaunchActions;
use super::SettingsDialogView;
use super::UiIcon;
use crate::topology;
use crate::wallet::WalletAccount;

/// Derived display values for the control sidebar and its telemetry rail.
struct ControlSidebarDerived {
    /// Full local DID string or `"not started"`.
    did_value: String,
    /// Whether the local DID is available to copy.
    can_copy_did: bool,
    /// Whether the node is active or currently starting.
    node_control_active: bool,
    /// Human-readable node lifecycle state (`"ready"`, `"starting"`, or `"offline"`).
    node_state: &'static str,
    /// CSS class name corresponding to [`Self::node_state`].
    node_state_class: &'static str,
    /// Active wallet standard label or `"none"`.
    account_standard: String,
    /// Active account delegation type or `"not authorized"`.
    session_label: String,
    /// Formatted connected peer count summary.
    peer_summary: String,
    /// Transport status label (`"linked"` or `"standby"`).
    transport_state: String,
    /// Shortened DID for the compact telemetry rail.
    rail_did: String,
    /// Most recent node status message.
    last_signal: String,
}

impl ControlSidebarDerived {
    /// Compute derived sidebar display fields from the current [`ControlView`].
    fn from_view(view: &ControlView<'_>) -> Self {
        let can_copy_did = !(**view.did).is_empty();
        Self {
            did_value: did_value(view.did),
            can_copy_did,
            node_control_active: can_copy_did || view.node_starting,
            node_state: node_state(can_copy_did, view.node_starting),
            node_state_class: node_state_class(can_copy_did, view.node_starting),
            account_standard: account_standard(view.wallet_account.as_ref()),
            session_label: session_label(view.wallet_account.as_ref()),
            peer_summary: peer_summary(view.peers.len()),
            transport_state: transport_state(view.peers.is_empty()),
            rail_did: rail_did(view.did, can_copy_did),
            last_signal: (**view.status).clone(),
        }
    }
}

/// Shell-level controls and dialog state injected into the control sidebar.
pub(crate) struct ControlSidebarShell {
    /// Rendered workbench trigger and modal control.
    pub(crate) workbench_control: Html,
    /// Optional WebView launch control rendered in the command deck.
    pub(crate) webview_control: Option<Html>,
    /// Currently open modal dialog, if any.
    pub(crate) active_dialog: ActiveDialog,
    /// Callbacks for opening and closing modal dialogs.
    pub(crate) dialog_actions: DialogActions,
    /// Collapsed state handle for the web console sidebar.
    pub(crate) collapsed: UseStateHandle<bool>,
    /// Whether the UI is running inside the browser extension side panel.
    pub(crate) extension_mode: bool,
}

/// Render the node control sidebar, command deck, telemetry rail, and settings dialog.
pub(crate) fn control_sidebar(
    view: ControlView<'_>,
    actions: LaunchActions,
    shell: ControlSidebarShell,
) -> Html {
    let derived = ControlSidebarDerived::from_view(&view);
    let on_copy_did = copy_local_did_callback(view.did, view.status);
    let open_settings_dialog = {
        let open_dialog = shell.dialog_actions.open.clone();
        Callback::from(move |_| open_dialog.emit(ActiveDialog::Settings))
    };
    let close_settings_dialog = shell.dialog_actions.close.clone();
    let toggle_sidebar = {
        let collapsed = shell.collapsed.clone();
        Callback::from(move |_| collapsed.set(!*collapsed))
    };
    let sidebar_class = control_sidebar_class(shell.extension_mode, *shell.collapsed);
    html! {
        <aside class={sidebar_class} aria-label="Node controls">
            if !shell.extension_mode {
                { sidebar_toggle_button(*shell.collapsed, toggle_sidebar) }
            }
            if shell.extension_mode || !*shell.collapsed {
                { sidebar_content(
                    &derived,
                    &actions,
                    shell.workbench_control,
                    shell.webview_control,
                    open_settings_dialog,
                    on_copy_did.clone(),
                ) }
            }
            { settings_dialog_if_open(shell.active_dialog, &view, actions, &derived, on_copy_did, close_settings_dialog) }
        </aside>
    }
}

/// Return the full DID string, or `"not started"` when the node is offline.
fn did_value(did: &UseStateHandle<String>) -> String {
    if (**did).is_empty() {
        "not started".to_string()
    } else {
        (**did).clone()
    }
}

/// Return the human-readable node lifecycle label.
fn node_state(can_copy_did: bool, node_starting: bool) -> &'static str {
    if can_copy_did {
        "ready"
    } else if node_starting {
        "starting"
    } else {
        "offline"
    }
}

/// Return the CSS class for the node lifecycle badge.
fn node_state_class(can_copy_did: bool, node_starting: bool) -> &'static str {
    if can_copy_did {
        "rail-state ready"
    } else if node_starting {
        "rail-state starting"
    } else {
        "rail-state"
    }
}

/// Return the display label for the connected wallet standard, or `"none"`.
fn account_standard(account: Option<&WalletAccount>) -> String {
    account
        .map(|account| account.kind.label().to_string())
        .unwrap_or_else(|| "none".to_string())
}

/// Return the delegation account type label, or `"not authorized"`.
fn session_label(account: Option<&WalletAccount>) -> String {
    account
        .map(|account| account.account_type.clone())
        .unwrap_or_else(|| "not authorized".to_string())
}

/// Format the connected peer count for the transport telemetry card.
fn peer_summary(count: usize) -> String {
    match count {
        0 => "0 connected".to_string(),
        1 => "1 connected".to_string(),
        count => format!("{count} connected"),
    }
}

/// Return `"standby"` when no peers are connected, otherwise `"linked"`.
fn transport_state(peers_empty: bool) -> String {
    if peers_empty {
        "standby".to_string()
    } else {
        "linked".to_string()
    }
}

/// Return the abbreviated DID for the sidebar rail, or `"not started"`.
fn rail_did(did: &UseStateHandle<String>, can_copy_did: bool) -> String {
    if can_copy_did {
        topology::short_did((**did).as_str())
    } else {
        "not started".to_string()
    }
}

/// Compute the CSS class list for the outer sidebar `<aside>` element.
fn control_sidebar_class(extension_mode: bool, collapsed: bool) -> &'static str {
    if extension_mode {
        "control-sidebar extension-action-tabs"
    } else if collapsed {
        "control-sidebar collapsed"
    } else {
        "control-sidebar"
    }
}

/// Render the button that collapses or expands the sidebar in web mode.
fn sidebar_toggle_button(collapsed: bool, toggle_sidebar: Callback<MouseEvent>) -> Html {
    html! {
        <button
            class="sidebar-toggle"
            type="button"
            aria-label={if collapsed { "Open controls" } else { "Collapse controls" }}
            aria-expanded={(!collapsed).to_string()}
            aria-controls="node-control-sidebar-content"
            title={if collapsed { "Open controls" } else { "Collapse controls" }}
            onclick={toggle_sidebar}
        >
            <span class="sidebar-toggle-icon" aria-hidden="true">
                { ui_icon(if collapsed { UiIcon::PanelOpen } else { UiIcon::PanelClose }) }
            </span>
            <span class="sidebar-toggle-label">
                { if collapsed { "Setup" } else { "Hide" } }
            </span>
        </button>
    }
}

/// Render the expanded sidebar body with the command deck and telemetry cards.
fn sidebar_content(
    derived: &ControlSidebarDerived,
    actions: &LaunchActions,
    workbench_control: Html,
    webview_control: Option<Html>,
    open_settings_dialog: Callback<MouseEvent>,
    on_copy_did: Callback<MouseEvent>,
) -> Html {
    html! {
        <div id="node-control-sidebar-content" class="sidebar-content sidebar-command-panel">
            { command_panel_header(webview_control.is_some()) }
            <div class="command-grid">
                { node_action_button(derived, actions) }
                { workbench_control }
                { webview_control }
                { settings_button(open_settings_dialog) }
            </div>
            { rail_telemetry(derived, on_copy_did) }
        </div>
    }
}

/// Render the header for the sidebar command deck showing the command count.
fn command_panel_header(has_webview: bool) -> Html {
    html! {
        <div class="command-panel-header">
            <div>
                <p class="eyebrow">{ "Control" }</p>
                <h3>{ "Command deck" }</h3>
            </div>
            <span>{ if has_webview { "04" } else { "03" } }</span>
        </div>
    }
}

/// Render the primary Start/Stop node action button.
fn node_action_button(derived: &ControlSidebarDerived, actions: &LaunchActions) -> Html {
    let label = if derived.node_control_active {
        "Stop"
    } else {
        "Start"
    };
    let icon = if derived.node_control_active {
        UiIcon::PowerOff
    } else {
        UiIcon::Power
    };
    let action = if derived.node_control_active {
        actions.on_disconnect.clone()
    } else {
        actions.on_start.clone()
    };
    let class = if derived.node_control_active {
        "secondary action-button command-button stop-button"
    } else {
        "link-open command-button start-button"
    };
    html! {
        <button class={class} type="button" aria-label={label} title={label} onclick={action}>
            <span class="label-desktop">{ label }</span>
            <span class="label-mobile command-icon" aria-hidden="true">
                { ui_icon(icon) }
                <span class="command-caption">{ label }</span>
            </span>
        </button>
    }
}

/// Render the button that opens the node settings dialog.
fn settings_button(open_settings_dialog: Callback<MouseEvent>) -> Html {
    html! {
        <button class="secondary action-button command-button settings-button" type="button" aria-label="Settings" title="Settings" onclick={open_settings_dialog}>
            <span class="label-desktop">{ "Settings" }</span>
            <span class="label-mobile command-icon" aria-hidden="true">
                { ui_icon(UiIcon::Sliders) }
                <span class="command-caption">{ "Settings" }</span>
            </span>
        </button>
    }
}

/// Render the stack of telemetry cards inside the sidebar rail.
fn rail_telemetry(derived: &ControlSidebarDerived, on_copy_did: Callback<MouseEvent>) -> Html {
    html! {
        <div class="rail-telemetry" aria-label="Node telemetry">
            { node_rail_card(derived) }
            { identity_rail_card(derived, on_copy_did) }
            { transport_rail_card(derived) }
            { signal_rail_card(&derived.last_signal) }
        </div>
    }
}

/// Render the node status, account standard, and delegation telemetry card.
fn node_rail_card(derived: &ControlSidebarDerived) -> Html {
    html! {
        <section class="rail-card">
            <div class="rail-card-header">
                <span>{ "Node" }</span>
                <strong class={derived.node_state_class}>{ derived.node_state }</strong>
            </div>
            { rail_row("Standard", derived.account_standard.clone()) }
            { rail_row("Delegation", derived.session_label.clone()) }
        </section>
    }
}

/// Render the local DID telemetry card with a copy action button.
fn identity_rail_card(derived: &ControlSidebarDerived, on_copy_did: Callback<MouseEvent>) -> Html {
    html! {
        <section class="rail-card">
            <div class="rail-card-header">
                <span>{ "Identity" }</span>
                <button
                    class="copy-button rail-copy"
                    type="button"
                    disabled={!derived.can_copy_did}
                    onclick={on_copy_did}
                >
                    { "Copy" }
                </button>
            </div>
            <code class="rail-did" title={derived.did_value.clone()}>{ derived.rail_did.clone() }</code>
        </section>
    }
}

/// Render the transport exchange and peer count telemetry card.
fn transport_rail_card(derived: &ControlSidebarDerived) -> Html {
    html! {
        <section class="rail-card">
            <div class="rail-card-header">
                <span>{ "Transport" }</span>
                <strong class="rail-state">{ derived.transport_state.clone() }</strong>
            </div>
            { rail_row("Exchange", "SDP / HTTP".to_string()) }
            { rail_row("Peers", derived.peer_summary.clone()) }
        </section>
    }
}

/// Render the telemetry card displaying the latest status signal message.
fn signal_rail_card(last_signal: &str) -> Html {
    html! {
        <section class="rail-card signal-card">
            <div class="rail-card-header">
                <span>{ "Last signal" }</span>
            </div>
            <p>{ last_signal }</p>
        </section>
    }
}

/// Render the settings modal dialog when [`ActiveDialog::Settings`] is active.
fn settings_dialog_if_open(
    active_dialog: ActiveDialog,
    view: &ControlView<'_>,
    actions: LaunchActions,
    derived: &ControlSidebarDerived,
    on_copy_did: Callback<MouseEvent>,
    close_dialog: Callback<MouseEvent>,
) -> Html {
    if active_dialog != ActiveDialog::Settings {
        return html! {};
    }
    settings_dialog(SettingsDialogView {
        wallet_kind: view.wallet_kind,
        actions,
        network_id: view.network_id,
        ice_servers: view.ice_servers,
        stabilize_interval: view.stabilize_interval,
        storage_name: view.storage_name,
        seed_url: view.seed_url,
        status: view.status,
        did_value: derived.did_value.clone(),
        on_copy_did,
        can_copy_did: derived.can_copy_did,
        wallet_account: view.wallet_account.clone(),
        close_dialog,
    })
}
