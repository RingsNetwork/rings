//! Yew application for the Rings browser frontend.

use std::time::Duration;

use futures::FutureExt;
use gloo_timers::callback::Interval;
use gloo_timers::future::sleep;
use web_sys::Event;
use yew::prelude::*;

use crate::connect;
use crate::connect::ConnectState;
use crate::connect::LinkTab;
use crate::connect::SdpMode;
use crate::controls;
use crate::controls::ControlView;
use crate::controls::LaunchActions;
use crate::controls::Panel;
use crate::controls::SessionView;
use crate::custom;
use crate::dweb;
use crate::extension;
use crate::forms::select_value;
use crate::generation::GenerationClock;
use crate::node;
use crate::node::DemoNode;
use crate::node::PeerView;
use crate::peer_sync;
use crate::styles;
use crate::wallet;
use crate::wallet::WalletAccount;
use crate::wallet::WalletKind;
use crate::workbench;
use crate::workbench::DwebState;

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

/// Rings browser frontend app.
#[function_component(App)]
pub fn app() -> Html {
    let active_panel = use_state(|| Panel::Dweb);
    let wallet_kind = use_state(|| {
        extension::load_setting_with_legacy(
            extension::SETTING_WALLET_KIND,
            extension::LEGACY_SETTING_WALLET_KIND,
        )
            .map(|value| WalletKind::from_value(&value))
            .unwrap_or(WalletKind::WebCrypto)
    });
    let wallet_account = use_state(|| None::<WalletAccount>);
    let node_starting = use_state(|| false);
    let node_ref = use_mut_ref(|| None::<DemoNode>);
    let generation_ref = use_mut_ref(GenerationClock::default);
    let generation = generation_ref.borrow().clone();
    let site = use_mut_ref(dweb::default_site);

    let did = use_state(String::new);
    let status = use_state(|| "select an account standard and start the browser node".to_string());
    let network_id = use_state(|| {
        extension::load_setting_with_legacy(
            extension::SETTING_NETWORK_ID,
            extension::LEGACY_SETTING_NETWORK_ID,
        )
        .unwrap_or_else(|| "1".to_string())
    });
    let ice_servers = use_state(|| {
        extension::load_setting_with_legacy(
            extension::SETTING_ICE_SERVERS,
            extension::LEGACY_SETTING_ICE_SERVERS,
        )
            .unwrap_or_else(|| "stun://stun.l.google.com:19302".to_string())
    });
    let stabilize_interval = use_state(|| {
        extension::load_setting_with_legacy(
            extension::SETTING_STABILIZE_INTERVAL,
            extension::LEGACY_SETTING_STABILIZE_INTERVAL,
        )
            .unwrap_or_else(|| "3".to_string())
    });
    let storage_name = use_state(|| {
        extension::load_setting_with_legacy(
            extension::SETTING_STORAGE_NAME,
            extension::LEGACY_SETTING_STORAGE_NAME,
        )
        .unwrap_or_else(|| "rings-frontend".to_string())
    });
    let peers = use_state(Vec::<PeerView>::new);

    let seed_url = use_state(|| {
        extension::load_setting_with_legacy(
            extension::SETTING_SEED_URL,
            extension::LEGACY_SETTING_SEED_URL,
        )
        .unwrap_or_default()
    });
    let http_endpoint = use_state(|| {
        extension::load_setting_with_legacy(
            extension::SETTING_HTTP_ENDPOINT,
            extension::LEGACY_SETTING_HTTP_ENDPOINT,
        )
            .unwrap_or_else(|| "http://127.0.0.1:50001".to_string())
    });
    let sdp_remote_did = use_state(String::new);
    let generated_offer = use_state(String::new);
    let remote_offer = use_state(String::new);
    let generated_answer = use_state(String::new);
    let remote_answer = use_state(String::new);
    let sdp_mode = use_state(|| SdpMode::Initiator);
    let link_dialog_open = use_state(|| false);
    let link_tab = use_state(|| LinkTab::ManualSdp);
    let settings_dialog_open = use_state(|| false);
    let control_sidebar_collapsed = use_state(|| false);
    let workbench_dialog_open = use_state(|| false);

    let host_path = use_state(|| "/".to_string());
    let host_body = use_state(dweb::default_page);
    let hosted_pages = use_state(|| vec![("/".to_string(), dweb::default_page())]);
    let fetch_peer = use_state(String::new);
    let fetch_path = use_state(|| "/".to_string());
    let dweb_page = use_state(String::new);

    let prover_did = use_state(String::new);
    let r1cs_url = use_state(|| "http://127.0.0.1:8080/simple_bn256.r1cs".to_string());
    let wasm_url = use_state(|| "http://127.0.0.1:8080/simple_bn256.wasm".to_string());

    let custom_namespace = use_state(|| "custom".to_string());
    let custom_registered = use_state(|| {
        custom::DEMO_NAMESPACES
            .iter()
            .map(|namespace| (*namespace).to_string())
            .collect::<Vec<_>>()
    });
    let custom_peer = use_state(String::new);
    let custom_payload = use_state(|| "hello from Rings".to_string());
    let custom_events = use_state(Vec::<custom::CustomEvent>::new);

    let on_wallet_kind = {
        let wallet_kind = wallet_kind.clone();
        Callback::from(move |event: Event| {
            if let Some(value) = select_value(&event) {
                wallet_kind.set(WalletKind::from_value(&value));
            }
        })
    };

    {
        let settings_snapshot = SettingsSnapshot {
            wallet_kind: (*wallet_kind).value().to_string(),
            network_id: (*network_id).clone(),
            ice_servers: (*ice_servers).clone(),
            stabilize_interval: (*stabilize_interval).clone(),
            storage_name: (*storage_name).clone(),
            seed_url: (*seed_url).clone(),
            http_endpoint: (*http_endpoint).clone(),
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

    {
        let did = did.clone();
        let peers = peers.clone();
        let wallet_account = wallet_account.clone();
        let status = status.clone();
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

    let on_start = {
        let wallet_kind = wallet_kind.clone();
        let wallet_account = wallet_account.clone();
        let node_starting = node_starting.clone();
        let node_ref = node_ref.clone();
        let generation = generation.clone();
        let site = site.clone();
        let did = did.clone();
        let status = status.clone();
        let peers = peers.clone();
        let network_id = network_id.clone();
        let ice_servers = ice_servers.clone();
        let stabilize_interval = stabilize_interval.clone();
        let storage_name = storage_name.clone();
        let seed_url = seed_url.clone();
        let dweb_page = dweb_page.clone();
        let custom_events = custom_events.clone();
        let settings_dialog_open = settings_dialog_open.clone();
        Callback::from(move |_| {
            let status = status.clone();
            let peers = peers.clone();
            let wallet_account = wallet_account.clone();
            let node_starting = node_starting.clone();
            let node_ref = node_ref.clone();
            let generation = generation.clone();
            let site = site.clone();
            let did = did.clone();
            let settings_dialog_open = settings_dialog_open.clone();
            let network_id = (*network_id).clone();
            let ice_servers = (*ice_servers).clone();
            let stabilize_interval = (*stabilize_interval).clone();
            let storage_name = (*storage_name).clone();
            let seed_url = (*seed_url).trim().to_string();
            let dweb_page = dweb_page.clone();
            let custom_events = custom_events.clone();
            let kind = *wallet_kind;
            let start_token = generation.bump();
            node_starting.set(true);
            status.set(format!("connecting {}", kind.label()));
            wasm_bindgen_futures::spawn_local(async move {
                if let Some(bridge) = extension::extension_node_bridge() {
                    match extension::extension_node_start(
                        &bridge,
                        kind,
                        extension::ExtensionStartSettings {
                            network_id,
                            ice_servers,
                            stabilize_interval,
                            storage_name,
                            seed_url,
                        },
                    )
                    .await
                    {
                        Ok(snapshot) => {
                            *node_ref.borrow_mut() = None;
                            settings_dialog_open.set(false);
                            if !extension::apply_extension_snapshot(
                                snapshot,
                                &did,
                                &peers,
                                &wallet_account,
                                &node_starting,
                                &status,
                                &start_token,
                            ) {
                                return;
                            }
                            if let Err(error) = extension::poll_extension_node_start(
                                &bridge,
                                did,
                                peers,
                                wallet_account,
                                node_starting.clone(),
                                status.clone(),
                                start_token.clone(),
                            )
                            .await
                            {
                                if start_token.is_current() {
                                    node_starting.set(false);
                                    status.set(error);
                                }
                            }
                        }
                        Err(error) => {
                            if start_token.is_current() {
                                node_starting.set(false);
                                status.set(error);
                            }
                        }
                    }
                    return;
                }

                let settings = match extension::node_settings(
                    network_id,
                    ice_servers,
                    stabilize_interval,
                    storage_name,
                ) {
                    Ok(settings) => settings,
                    Err(error) => {
                        if start_token.is_current() {
                            node_starting.set(false);
                            status.set(error);
                        }
                        return;
                    }
                };
                let account = match extension::operation_timeout(
                    "account authorization",
                    extension::WALLET_CONNECT_TIMEOUT,
                    wallet::connect(kind),
                )
                .await
                {
                    Ok(account) => account,
                    Err(error) => {
                        if start_token.is_current() {
                            node_starting.set(false);
                            status.set(error);
                        }
                        return;
                    }
                };
                if !start_token.is_current() {
                    return;
                }
                status.set("authorizing session key".to_string());
                let built = match extension::operation_timeout(
                    "session authorization",
                    extension::SESSION_AUTH_TIMEOUT,
                    node::build_node(&account, settings),
                )
                .await
                {
                    Ok(node) => node,
                    Err(error) => {
                        if start_token.is_current() {
                            node_starting.set(false);
                            status.set(error);
                        }
                        return;
                    }
                };
                if !start_token.is_current() {
                    built.stop();
                    return;
                }
                let my_did = built.provider.address();
                site.borrow_mut().insert(
                    "/".to_string(),
                    format!(
                        "<h1>Rings node {my_did}</h1><p>Served by the Rings browser frontend.</p>"
                    ),
                );
                let on_dweb_response = {
                    let dweb_page = dweb_page.clone();
                    Callback::from(move |response: dweb::DwebResponse| {
                        dweb_page.set(format!("<!-- {} -->\n{}", response.path, response.body));
                    })
                };
                if let Err(error) = dweb::register(&built.provider, site.clone(), on_dweb_response)
                {
                    built.stop();
                    if start_token.is_current() {
                        node_starting.set(false);
                        status.set(error);
                    }
                    return;
                }
                let on_custom = {
                    let custom_events = custom_events.clone();
                    Callback::from(move |event: custom::CustomEvent| {
                        let mut next = (*custom_events).clone();
                        next.insert(0, event);
                        next.truncate(20);
                        custom_events.set(next);
                    })
                };
                for namespace in custom::DEMO_NAMESPACES {
                    if let Err(error) =
                        custom::register(&built.provider, namespace.to_string(), on_custom.clone())
                    {
                        built.stop();
                        if start_token.is_current() {
                            node_starting.set(false);
                            status.set(error);
                        }
                        return;
                    }
                }
                if !start_token.is_current() {
                    built.stop();
                    return;
                }
                did.set(my_did);
                wallet_account.set(Some(account));
                *node_ref.borrow_mut() = Some(built.clone());
                settings_dialog_open.set(false);
                node_starting.set(false);
                if seed_url.is_empty() {
                    status.set("node ready".to_string());
                    return;
                }
                status.set(format!("node ready; connecting seed {seed_url}"));
                match node::connect_http(&built.provider, seed_url).await {
                    Ok(seed_did) => {
                        let Some(seed_peer) = PeerView::connected(seed_did) else {
                            if start_token.is_current() {
                                status.set("node ready; seed returned an empty DID".to_string());
                            }
                            return;
                        };
                        if !start_token.is_current() {
                            return;
                        }
                        let seed_token = start_token.clone();
                        peer_sync::sync_peers_after_handshake(
                            built,
                            peers,
                            status,
                            "seed URL connected",
                            Some(seed_peer),
                            move || seed_token.is_current(),
                        )
                        .await;
                    }
                    Err(error) => {
                        if start_token.is_current() {
                            status.set(format!("node ready; seed connect failed: {error}"));
                        }
                    }
                }
            });
        })
    };

    let on_disconnect = {
        let wallet_account = wallet_account.clone();
        let node_starting = node_starting.clone();
        let node_ref = node_ref.clone();
        let generation = generation.clone();
        let did = did.clone();
        let status = status.clone();
        let peers = peers.clone();
        let generated_offer = generated_offer.clone();
        let remote_offer = remote_offer.clone();
        let generated_answer = generated_answer.clone();
        let remote_answer = remote_answer.clone();
        let link_dialog_open = link_dialog_open.clone();
        let settings_dialog_open = settings_dialog_open.clone();
        Callback::from(move |_| {
            if let Some(bridge) = extension::extension_node_bridge() {
                let stop_token = generation.bump();
                node_starting.set(true);
                status.set("stopping background node".to_string());
                let status = status.clone();
                let did = did.clone();
                let wallet_account = wallet_account.clone();
                let node_starting = node_starting.clone();
                let peers = peers.clone();
                let generated_offer = generated_offer.clone();
                let remote_offer = remote_offer.clone();
                let generated_answer = generated_answer.clone();
                let remote_answer = remote_answer.clone();
                let link_dialog_open = link_dialog_open.clone();
                let settings_dialog_open = settings_dialog_open.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match extension::extension_node_stop(&bridge).await {
                        Ok(message) if stop_token.is_current() => {
                            did.set(String::new());
                            wallet_account.set(None);
                            node_starting.set(false);
                            peers.set(Vec::new());
                            generated_offer.set(String::new());
                            remote_offer.set(String::new());
                            generated_answer.set(String::new());
                            remote_answer.set(String::new());
                            link_dialog_open.set(false);
                            settings_dialog_open.set(false);
                            status.set(message);
                        }
                        Ok(_) => {}
                        Err(error) if stop_token.is_current() => {
                            node_starting.set(false);
                            status.set(format!("background stop failed: {error}"));
                        }
                        Err(_) => {}
                    }
                });
                return;
            }

            let was_starting = *node_starting;
            let cleanup_token = generation.bump();
            let Some(node) = node_ref.borrow_mut().take() else {
                node_starting.set(false);
                let message = if was_starting {
                    "node start cancelled"
                } else {
                    "node already offline"
                };
                status.set(message.to_string());
                return;
            };
            let provider = node.provider.clone();
            did.set(String::new());
            wallet_account.set(None);
            node_starting.set(false);
            peers.set(Vec::new());
            generated_offer.set(String::new());
            remote_offer.set(String::new());
            generated_answer.set(String::new());
            remote_answer.set(String::new());
            link_dialog_open.set(false);
            settings_dialog_open.set(false);
            status.set("node disconnected".to_string());

            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let cleanup = node::disconnect_all(&provider).fuse();
                let timeout = sleep(Duration::from_secs(2)).fuse();
                futures::pin_mut!(cleanup, timeout);
                let message = futures::select! {
                    result = cleanup => match result {
                        Ok(0) => "node disconnected".to_string(),
                        Ok(count) => format!("node disconnected; closed {count} peer links"),
                        Err(error) => format!("node disconnected; peer cleanup failed: {error}"),
                    },
                    _ = timeout => "node disconnected; peer cleanup timed out".to_string(),
                };
                node.stop();
                if cleanup_token.is_current() {
                    status.set(message);
                }
            });
        })
    };

    {
        let node_ref = node_ref.clone();
        let generation = generation.clone();
        let peers = peers.clone();
        let did = did.clone();
        let wallet_account = wallet_account.clone();
        let node_starting = node_starting.clone();
        let node_online = !(*did).is_empty();
        use_effect_with(node_online, move |online| {
            let interval = if *online {
                Some(Interval::new(4_000, move || {
                    if let Some(bridge) = extension::extension_node_bridge() {
                        let refresh_token = generation.token();
                        let did = did.clone();
                        let peers = peers.clone();
                        let wallet_account = wallet_account.clone();
                        let node_starting = node_starting.clone();
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
                        return;
                    }
                    let Some(node) = node_ref.borrow().clone() else {
                        return;
                    };
                    let refresh_token = generation.token();
                    let peers = peers.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Ok(next) = node::list_peers(&node.provider).await {
                            if refresh_token.is_current() {
                                peers.set(next);
                            }
                        }
                    });
                }))
            } else {
                None
            };
            move || drop(interval)
        });
    }

    let control_view = ControlView {
        wallet_kind: *wallet_kind,
        wallet_account: (*wallet_account).clone(),
        node_starting: *node_starting,
        did: &did,
        status: &status,
        peers: &peers,
        network_id: &network_id,
        ice_servers: &ice_servers,
        stabilize_interval: &stabilize_interval,
        storage_name: &storage_name,
        seed_url: &seed_url,
    };
    let launch_actions = LaunchActions {
        on_wallet_kind,
        on_start,
        on_disconnect,
    };
    let session_view = SessionView {
        wallet_account: (*wallet_account).clone(),
        did: &did,
        peers: &peers,
    };
    let link_control = connect::link_control(
        ConnectState {
            http_endpoint: &http_endpoint,
            sdp_remote_did: &sdp_remote_did,
            generated_offer: &generated_offer,
            remote_offer: &remote_offer,
            generated_answer: &generated_answer,
            remote_answer: &remote_answer,
            sdp_mode: &sdp_mode,
            link_dialog_open: &link_dialog_open,
            link_tab: &link_tab,
            launcher_hidden: *settings_dialog_open || *workbench_dialog_open,
        },
        node_ref.clone(),
        generation.clone(),
        peers.clone(),
        status.clone(),
    );
    let workbench_body = match *active_panel {
        Panel::Dweb => html! {
            { workbench::dweb_panel(
                DwebState {
                    host_path: &host_path,
                    host_body: &host_body,
                    hosted_pages: &hosted_pages,
                    fetch_peer: &fetch_peer,
                    fetch_path: &fetch_path,
                    dweb_page: &dweb_page,
                },
                site.clone(),
                node_ref.clone(),
                status.clone(),
            ) }
        },
        Panel::Proof => html! {
            { workbench::proof_panel(
                &prover_did,
                &r1cs_url,
                &wasm_url,
                node_ref.clone(),
                status.clone(),
            ) }
        },
        Panel::Custom => html! {
            { workbench::custom_panel(
                &custom_namespace,
                &custom_registered,
                &custom_peer,
                &custom_payload,
                &custom_events,
                node_ref.clone(),
                status.clone(),
            ) }
        },
    };
    let extension_mode = extension::extension_node_bridge().is_some();
    let workbench_control = controls::workbench_control(
        *active_panel,
        active_panel.clone(),
        workbench_dialog_open.clone(),
        workbench_body,
        !extension_mode,
    );
    let control_sidebar = controls::control_sidebar(
        control_view,
        launch_actions,
        workbench_control,
        settings_dialog_open.clone(),
        control_sidebar_collapsed.clone(),
    );

    html! {
        <main class="app-shell topology-shell">
            <style>{ styles::APP_CSS }</style>
            { controls::app_header() }
            { controls::network_stage(session_view, &status, link_control, control_sidebar) }
        </main>
    }
}
