use web_sys::MouseEvent;
use yew::prelude::*;

use super::copy_local_did_callback;
use super::rail_row;
use super::settings_dialog;
use super::ui_icon;
use super::ActiveDialog;
use super::ControlView;
use super::LaunchActions;
use super::SettingsDialogView;
use super::UiIcon;
use crate::topology;
use crate::wallet::WalletAccount;

struct ControlSidebarDerived {
    did_value: String,
    can_copy_did: bool,
    node_control_active: bool,
    node_state: &'static str,
    node_state_class: &'static str,
    account_standard: String,
    session_label: String,
    peer_summary: String,
    transport_state: String,
    rail_did: String,
    last_signal: String,
}

impl ControlSidebarDerived {
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

pub(crate) fn control_sidebar(
    view: ControlView<'_>,
    actions: LaunchActions,
    workbench_control: Html,
    active_dialog: UseStateHandle<ActiveDialog>,
    collapsed: UseStateHandle<bool>,
    extension_mode: bool,
) -> Html {
    let derived = ControlSidebarDerived::from_view(&view);
    let on_copy_did = copy_local_did_callback(view.did, view.status);
    let open_settings_dialog = {
        let active_dialog = active_dialog.clone();
        Callback::from(move |_| active_dialog.set(ActiveDialog::Settings))
    };
    let close_settings_dialog = {
        let active_dialog = active_dialog.clone();
        Callback::from(move |_| active_dialog.set(ActiveDialog::None))
    };
    let toggle_sidebar = {
        let collapsed = collapsed.clone();
        Callback::from(move |_| collapsed.set(!*collapsed))
    };
    let sidebar_class = control_sidebar_class(extension_mode, *collapsed);
    html! {
        <aside class={sidebar_class} aria-label="Node controls">
            if !extension_mode {
                { sidebar_toggle_button(*collapsed, toggle_sidebar) }
            }
            if extension_mode || !*collapsed {
                { sidebar_content(&derived, &actions, workbench_control, open_settings_dialog, on_copy_did.clone()) }
            }
            { settings_dialog_if_open(*active_dialog, &view, actions, &derived, on_copy_did, close_settings_dialog) }
        </aside>
    }
}

fn did_value(did: &UseStateHandle<String>) -> String {
    if (**did).is_empty() {
        "not started".to_string()
    } else {
        (**did).clone()
    }
}

fn node_state(can_copy_did: bool, node_starting: bool) -> &'static str {
    if can_copy_did {
        "ready"
    } else if node_starting {
        "starting"
    } else {
        "offline"
    }
}

fn node_state_class(can_copy_did: bool, node_starting: bool) -> &'static str {
    if can_copy_did {
        "rail-state ready"
    } else if node_starting {
        "rail-state starting"
    } else {
        "rail-state"
    }
}

fn account_standard(account: Option<&WalletAccount>) -> String {
    account
        .map(|account| account.kind.label().to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn session_label(account: Option<&WalletAccount>) -> String {
    account
        .map(|account| account.account_type.clone())
        .unwrap_or_else(|| "not authorized".to_string())
}

fn peer_summary(count: usize) -> String {
    match count {
        0 => "0 connected".to_string(),
        1 => "1 connected".to_string(),
        count => format!("{count} connected"),
    }
}

fn transport_state(peers_empty: bool) -> String {
    if peers_empty {
        "standby".to_string()
    } else {
        "linked".to_string()
    }
}

fn rail_did(did: &UseStateHandle<String>, can_copy_did: bool) -> String {
    if can_copy_did {
        topology::short_did((**did).as_str())
    } else {
        "not started".to_string()
    }
}

fn control_sidebar_class(extension_mode: bool, collapsed: bool) -> &'static str {
    if extension_mode {
        "control-sidebar extension-action-tabs"
    } else if collapsed {
        "control-sidebar collapsed"
    } else {
        "control-sidebar"
    }
}

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

fn sidebar_content(
    derived: &ControlSidebarDerived,
    actions: &LaunchActions,
    workbench_control: Html,
    open_settings_dialog: Callback<MouseEvent>,
    on_copy_did: Callback<MouseEvent>,
) -> Html {
    html! {
        <div id="node-control-sidebar-content" class="sidebar-content sidebar-command-panel">
            { command_panel_header() }
            <div class="command-grid">
                { node_action_button(derived, actions) }
                { workbench_control }
                { settings_button(open_settings_dialog) }
            </div>
            { rail_telemetry(derived, on_copy_did) }
        </div>
    }
}

fn command_panel_header() -> Html {
    html! {
        <div class="command-panel-header">
            <div>
                <p class="eyebrow">{ "Control" }</p>
                <h3>{ "Command deck" }</h3>
            </div>
            <span>{ "03" }</span>
        </div>
    }
}

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

fn node_rail_card(derived: &ControlSidebarDerived) -> Html {
    html! {
        <section class="rail-card">
            <div class="rail-card-header">
                <span>{ "Node" }</span>
                <strong class={derived.node_state_class}>{ derived.node_state }</strong>
            </div>
            { rail_row("Standard", derived.account_standard.clone()) }
            { rail_row("Session", derived.session_label.clone()) }
        </section>
    }
}

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
