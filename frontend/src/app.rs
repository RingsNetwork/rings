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
use crate::controls::ShellPage;
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
use crate::topology;
use crate::wallet;
use crate::wallet::WalletAccount;
use crate::wallet::WalletKind;
use crate::workbench;

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
    let active_page = use_state(|| {
        if extension::extension_node_bridge().is_some() {
            ShellPage::Console
        } else {
            ShellPage::Guide
        }
    });
    let active_architecture_layer = use_state(|| 0_usize);
    let active_panel = use_state(|| Panel::Onion);
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

    let onion_url = use_state(|| "https://example.com/".to_string());
    let onion_method = use_state(|| "GET".to_string());
    let onion_hop_count = use_state(|| "3".to_string());
    let onion_allow_short_paths = use_state(|| true);
    let onion_headers = use_state(String::new);
    let onion_body = use_state(String::new);
    let onion_route_result = use_state(String::new);
    let onion_response_status = use_state(|| "idle".to_string());
    let onion_response_headers = use_state(String::new);
    let onion_response_body = use_state(String::new);

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
                let on_dweb_response = Callback::from(|_: dweb::DwebResponse| {});
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

    let extension_mode = extension::extension_node_bridge().is_some();
    {
        let active_panel = active_panel.clone();
        use_effect_with(extension_mode, move |extension_mode| {
            if *extension_mode {
                active_panel.set(Panel::Onion);
            }
            || {}
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
        Panel::Onion => html! {
            { workbench::onion_proxy_panel(
                workbench::OnionProxyState {
                    url: &onion_url,
                    method: &onion_method,
                    hop_count: &onion_hop_count,
                    allow_short_paths: &onion_allow_short_paths,
                    headers: &onion_headers,
                    body: &onion_body,
                    route_result: &onion_route_result,
                    response_status: &onion_response_status,
                    response_headers: &onion_response_headers,
                    response_body: &onion_response_body,
                },
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
    let workbench_control = controls::workbench_control(
        *active_panel,
        active_panel.clone(),
        workbench_dialog_open.clone(),
        workbench_body,
        true,
        extension_mode,
    );
    let control_sidebar = controls::control_sidebar(
        control_view,
        launch_actions,
        workbench_control,
        settings_dialog_open.clone(),
        control_sidebar_collapsed.clone(),
    );

    let header = controls::app_header(*active_page, active_page.clone());
    if *active_page == ShellPage::Guide {
        html! {
            <main class="app-shell guide-shell">
                <style>{ styles::app_css() }</style>
                { header }
                { guide_page(active_page.clone(), active_architecture_layer.clone()) }
            </main>
        }
    } else {
        html! {
            <main class="app-shell topology-shell">
                <style>{ styles::app_css() }</style>
                { header }
                { controls::network_stage(session_view, &status, link_control, control_sidebar) }
            </main>
        }
    }
}

fn guide_page(
    active_page: UseStateHandle<ShellPage>,
    active_architecture_layer: UseStateHandle<usize>,
) -> Html {
    let open_console = {
        let active_page = active_page.clone();
        Callback::from(move |_| active_page.set(ShellPage::Console))
    };
    let selected_architecture_index =
        (*active_architecture_layer).min(ARCHITECTURE_LAYERS.len() - 1);
    let selected_architecture_layer = &ARCHITECTURE_LAYERS[selected_architecture_index];
    html! {
        <section class="guide-page" aria-labelledby="guide-title">
            <section class="landing-hero">
                <div class="landing-hero-copy">
                    <p class="landing-kicker">{ "Rings Network" }</p>
                    <h2 id="guide-title">{ "A peer-to-peer network for the sovereign age." }</h2>
                    <p class="landing-lede">
                        { "Rings is a browser-native, structured peer-to-peer network for applications that need their own network layer instead of a server-owned data path. Browser tabs and native daemons can join the same overlay, discover peers by DID, and exchange messages over direct WebRTC datachannels routed by a Chord DHT." }
                    </p>
                    <div class="landing-actions" aria-label="Primary actions">
                        <button class="landing-primary-action" type="button" onclick={open_console.clone()}>
                            { "Open WorkBench" }
                        </button>
                        <a
                            class="landing-secondary-action"
                            href="https://github.com/RyanKung/rings"
                            target="_blank"
                            rel="noreferrer"
                        >
                            { "GitHub" }
                        </a>
                        <a
                            class="landing-secondary-action"
                            href="https://github.com/RyanKung/rings/blob/master/papers/rings.pdf"
                            target="_blank"
                            rel="noreferrer"
                        >
                            { "Whitepaper" }
                        </a>
                    </div>
                </div>
                <div class="landing-visual" aria-label="Simulated Rings network topology">
                    <div class="landing-topology-card">
                        { topology::guide_preview() }
                    </div>
                </div>
            </section>

            <section class="landing-section landing-feature-section" aria-label="Features">
                <div class="landing-section-heading">
                    <p>{ "Features" }</p>
                </div>
                <div class="landing-feature-grid">
                    { landing_feature("Browser-native peers", "Runs in browsers through WebAssembly and web_sys, and on native hosts through the same Rust node stack. WebRTC datachannels carry browser-to-browser and daemon traffic without an application server in the data path.") }
                    { landing_feature("DID identity and cryptography", "Peers are addressed by decentralized identifiers backed by selectable signature schemes, including secp256k1, secp256r1, ed25519, BLS, and bip137.") }
                    { landing_feature("Structured peer routing", "A Chord DHT provides successor and finger-table routing, DID lookup, message relay, stabilization, and network_id isolation for independent overlays.") }
                    { landing_feature("Protocol runtime", "Application protocols are namespace-scoped. A pure step function owns state transitions while an Interpret shell performs side effects through a scoped capability.") }
                </div>
            </section>

            <section class="landing-section landing-architecture" aria-labelledby="landing-architecture-title">
                <div class="landing-section-heading">
                    <p>{ "Architecture" }</p>
                    <h2 id="landing-architecture-title">{ "Every layer is decentralized." }</h2>
                    <p class="landing-section-lede">
                        { "Rings maps applications, protocols, extension runtime, overlay routing, transport, and identity directly to repository crates and modules. Select a layer to inspect its role." }
                    </p>
                </div>
                <div class="landing-architecture-grid">
                    <div class="landing-layer-stack" aria-label="Rings architecture layers">
                        { for ARCHITECTURE_LAYERS.iter().enumerate().map(|(index, layer)| {
                            architecture_layer_tab(
                                index,
                                layer,
                                index == selected_architecture_index,
                                active_architecture_layer.clone(),
                            )
                        }) }
                    </div>
                    <aside class="landing-layer-detail" aria-live="polite" aria-label="Selected architecture layer">
                        <div class="landing-layer-detail-heading">
                            <span class="landing-layer-detail-index">{ selected_architecture_layer.index }</span>
                            <div>
                                <span class="landing-layer-label">{ selected_architecture_layer.label }</span>
                                <h3>{ selected_architecture_layer.title }</h3>
                            </div>
                        </div>
                        <p class="landing-layer-detail-summary">{ selected_architecture_layer.summary }</p>
                        <p>{ selected_architecture_layer.detail }</p>
                        <dl class="landing-layer-detail-list">
                            <div>
                                <dt>{ "Surface" }</dt>
                                <dd>{ selected_architecture_layer.surface }</dd>
                            </div>
                            <div>
                                <dt>{ "Contract" }</dt>
                                <dd>{ selected_architecture_layer.contract }</dd>
                            </div>
                        </dl>
                    </aside>
                </div>
            </section>

            <section class="landing-section landing-runtime" aria-labelledby="landing-runtime-title">
                <div class="landing-section-heading">
                    <p>{ "Extending Rings" }</p>
                    <h2 id="landing-runtime-title">{ "Pure protocol core, scoped interpreter shell." }</h2>
                    <p class="landing-section-lede">
                        { "The README's extension model is the landing page's developer contract: register a protocol, bind its interpreter, then route inbound envelopes by namespace." }
                    </p>
                </div>
                <div class="landing-runtime-visual">
                    <pre class="landing-code"><code>{ "provider.register_protocol(Echo, EchoShell)?;\nprovider.set_backend()?;\n\nlet relay = RelayHandle::install(&provider.extensions())?;\nrelay\n    .register_tcp_service(\"web\".into(), \"example.com:80\".parse()?)\n    .await?;\nrelay\n    .open_tcp_tunnel(local_addr, peer_did, \"web\".into())\n    .await?;" }</code></pre>
                </div>
            </section>

            <section class="landing-section landing-examples" aria-labelledby="landing-examples-title">
                <div class="landing-section-heading">
                    <p>{ "Examples" }</p>
                    <h2 id="landing-examples-title">{ "Runnable surfaces from the repository." }</h2>
                </div>
                <div class="landing-example-grid">
                    { landing_link_card("native", "Start here for a minimal native node. It shows wallet setup, node bootstrapping, and registration of a custom namespaced protocol without browser-specific APIs.", "https://github.com/RyanKung/rings/tree/master/examples/native") }
                    { landing_link_card("relay", "Open TCP and UDP tunnels through the overlay. This example is the practical path for exposing a peer service and carrying traffic without a public server hop.", "https://github.com/RyanKung/rings/tree/master/examples/relay") }
                    { landing_link_card("snark", "Run fold-scheme zkSNARK proving and verification over the Rings protocol model. It demonstrates how proof workloads fit beside ordinary peer messages.", "https://github.com/RyanKung/rings/tree/master/examples/snark") }
                    { landing_link_card("proof-demo", "Use the browser proof surface built with Yew and Trunk. It connects the frontend runtime to the proof flow so the browser can drive a live proving interaction.", "https://github.com/RyanKung/rings/tree/master/examples/proof-demo") }
                    { landing_link_card("dweb", "Explore the decentralized-web application shape. It demonstrates how application content can be addressed through Rings instead of relying on a conventional hosted backend.", "https://github.com/RyanKung/rings/tree/master/examples/dweb") }
                    { landing_link_card("ffi", "Drive a Rings node from another runtime through the C FFI. This is the integration point for embedding Rings into hosts that cannot call the Rust API directly.", "https://github.com/RyanKung/rings/tree/master/examples/ffi") }
                </div>
            </section>

            <section class="landing-final" aria-label="Open Rings WorkBench">
                <div>
                    <p>{ "Frontend" }</p>
                    <h2>{ "Use the browser and extension WorkBench for the live network surface." }</h2>
                    <span>
                        { "Wallet login, SDP/HTTP connectivity, topology inspection, onion proxy requests, proof tools, and custom messages live here." }
                    </span>
                </div>
                <button class="landing-primary-action" type="button" onclick={open_console}>
                    { "Open WorkBench" }
                </button>
            </section>
        </section>
    }
}

struct ArchitectureLayer {
    index: &'static str,
    label: &'static str,
    role: &'static str,
    title: &'static str,
    summary: &'static str,
    detail: &'static str,
    surface: &'static str,
    contract: &'static str,
}

const ARCHITECTURE_LAYERS: [ArchitectureLayer; 6] = [
    ArchitectureLayer {
        index: "01",
        label: "applications",
        role: "runs user-facing workflows.",
        title: "dWeb, zk-proof demo, relay, custom apps",
        summary: "Apps run over the protocol layer instead of a hosted backend data path.",
        detail: "Application surfaces are repository examples and browser WorkBench panels. They compose wallet login, dWeb content, proof workflows, relay tunnels, and custom protocol messages on top of the same peer runtime. The application layer should read as product-facing behavior: it chooses what to ask the network to do, while the lower layers keep addressing, routing, and transport concerns out of the UI code.",
        surface: "frontend WorkBench, examples/dweb, examples/snark, examples/relay",
        contract: "Application code addresses peers and namespaces; it does not own overlay routing or transport setup.",
    },
    ArchitectureLayer {
        index: "02",
        label: "protocols",
        role: "defines namespaced behavior.",
        title: "relay, SNARK, echo, user namespaces",
        summary: "Built-ins cover TCP/UDP relay and fold-scheme zkSNARK proving; user protocols are addressed by namespace.",
        detail: "Protocols are registered behind stable namespaces. Built-in protocols cover relay and proving flows, while external applications can install their own protocol state machines without changing the overlay. This layer is the extension boundary: new behavior is added by registering a protocol and its interpreter, not by branching the node or adding a new transport path.",
        surface: "protocol registry, relay handles, proof protocol, custom namespaces",
        contract: "Every inbound envelope is dispatched by namespace before it reaches application-specific logic.",
    },
    ArchitectureLayer {
        index: "03",
        label: "runtime",
        role: "executes protocol state.",
        title: "pure Protocol::step plus Interpret shell",
        summary: "Protocol logic stays pure while side effects are confined to namespace-scoped capabilities.",
        detail: "The runtime keeps deterministic protocol transitions separate from IO. Pure step logic computes the next state and effects; the interpreter shell is the only place where scoped side effects are executed. This makes protocol behavior easier to test and reason about, because replayable state transitions are separated from browser APIs, native sockets, storage, and wallet interaction.",
        surface: "Protocol::step, Interpret shell, provider extension hooks",
        contract: "State transitions must be reproducible; IO must pass through explicit provider capabilities.",
    },
    ArchitectureLayer {
        index: "04",
        label: "overlay",
        role: "routes peer messages.",
        title: "Chord DHT routing",
        summary: "Successor and finger tables route DID-addressed messages with stabilization and network isolation.",
        detail: "The overlay maps DID identifiers into a Chord ring. Stabilization keeps successor context current, while finger links reduce lookup distance and keep routing independent of any central server. The overlay is responsible for peer discovery, message forwarding, and path selection; applications see a DID-addressed network rather than a set of manually managed connections.",
        surface: "Chord identifiers, successor tables, finger routing, network_id isolation",
        contract: "Routing chooses peer paths by identifier space, not by hosted origin or application server.",
    },
    ArchitectureLayer {
        index: "05",
        label: "transport",
        role: "moves data between peers.",
        title: "WebRTC datachannels",
        summary: "Native and browser transports use STUN, ICE, and SDP to establish direct peer connections.",
        detail: "Browser and native peers share the same transport shape. WebRTC handles NAT traversal through ICE and SDP exchange, then carries overlay messages through direct datachannels. This layer is deliberately narrow: it moves bytes between peers and reports connection state, while routing policy and protocol semantics remain above it.",
        surface: "browser WebRTC, native WebRTC, STUN, SDP exchange",
        contract: "Transport establishes peer connectivity; overlay and protocol layers decide what should be carried.",
    },
    ArchitectureLayer {
        index: "06",
        label: "identity",
        role: "authenticates peers.",
        title: "DID plus selectable signatures",
        summary: "The network bridges browser, daemon, and wallet identity workflows without one key system.",
        detail: "Identity is represented as DID-addressable cryptographic material. The implementation supports multiple signature families so browser wallets, native daemons, and tests can share a common addressing model. Higher layers can depend on stable peer identity without knowing whether the key came from WebCrypto, a wallet bridge, or a native node process.",
        surface: "DID documents, wallet account selection, secp256k1, secp256r1, ed25519, BLS, bip137",
        contract: "Peers authenticate as DIDs; higher layers should depend on identity abstractions rather than one wallet backend.",
    },
];

fn landing_feature(title: &'static str, body: &'static str) -> Html {
    html! {
        <article class="landing-feature-card">
            <h3>{ title }</h3>
            <p>{ body }</p>
        </article>
    }
}

fn architecture_layer_tab(
    index: usize,
    layer: &ArchitectureLayer,
    selected: bool,
    active_architecture_layer: UseStateHandle<usize>,
) -> Html {
    let on_click = {
        let active_architecture_layer = active_architecture_layer.clone();
        Callback::from(move |_| active_architecture_layer.set(index))
    };
    let class = if selected {
        "landing-layer active"
    } else {
        "landing-layer"
    };
    html! {
        <button class={class} type="button" onclick={on_click} aria-pressed={selected.to_string()}>
            <span class="landing-layer-index">{ layer.index }</span>
            <div>
                <h3>{ layer.label }</h3>
                <p>{ layer.role }</p>
            </div>
        </button>
    }
}

fn landing_link_card(title: &'static str, body: &'static str, href: &'static str) -> Html {
    html! {
        <a class="landing-example-card" href={href} target="_blank" rel="noreferrer">
            <h3>{ title }</h3>
            <p>{ body }</p>
        </a>
    }
}
