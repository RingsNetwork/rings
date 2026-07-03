//! Peer connection controls and SDP exchange dialog.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsValue;
use yew::prelude::*;

use crate::extension;
use crate::forms::readonly_textarea;
use crate::forms::text_input;
use crate::forms::textarea;
use crate::generation::GenerationClock;
use crate::generation::GenerationToken;
use crate::node;
use crate::node::DemoNode;
use crate::node::PeerView;
use crate::peer_sync;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum SdpMode {
    Initiator,
    Responder,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum LinkTab {
    ManualSdp,
    HttpEndpoint,
}

pub(crate) struct ConnectState<'a> {
    pub(crate) http_endpoint: &'a UseStateHandle<String>,
    pub(crate) sdp_remote_did: &'a UseStateHandle<String>,
    pub(crate) generated_offer: &'a UseStateHandle<String>,
    pub(crate) remote_offer: &'a UseStateHandle<String>,
    pub(crate) generated_answer: &'a UseStateHandle<String>,
    pub(crate) remote_answer: &'a UseStateHandle<String>,
    pub(crate) sdp_mode: &'a UseStateHandle<SdpMode>,
    pub(crate) link_dialog_open: &'a UseStateHandle<bool>,
    pub(crate) link_tab: &'a UseStateHandle<LinkTab>,
    pub(crate) launcher_hidden: bool,
}

struct ConnectDialogView {
    active_tab: LinkTab,
    active_sdp_mode: SdpMode,
    http_endpoint: UseStateHandle<String>,
    sdp_remote_did: UseStateHandle<String>,
    generated_offer: UseStateHandle<String>,
    remote_offer: UseStateHandle<String>,
    generated_answer: UseStateHandle<String>,
    remote_answer: UseStateHandle<String>,
    status: UseStateHandle<String>,
}

struct ConnectDialogActions {
    set_manual_sdp: Callback<MouseEvent>,
    set_http_endpoint: Callback<MouseEvent>,
    set_initiator: Callback<MouseEvent>,
    set_responder: Callback<MouseEvent>,
    close_dialog: Callback<MouseEvent>,
    on_http_connect: Callback<MouseEvent>,
    on_create_offer: Callback<MouseEvent>,
    on_answer_offer: Callback<MouseEvent>,
    on_accept_answer: Callback<MouseEvent>,
}

enum NodeBackend {
    Extension {
        bridge: JsValue,
        token: GenerationToken,
    },
    Local {
        node: DemoNode,
        token: GenerationToken,
    },
}

enum PeerUpdate {
    Snapshot {
        snapshot: extension::ExtensionNodeSnapshot,
        token: GenerationToken,
    },
    Local {
        node: DemoNode,
        token: GenerationToken,
        context: &'static str,
        required_peer: Option<PeerView>,
    },
}

impl NodeBackend {
    fn current(
        node_ref: &Rc<RefCell<Option<DemoNode>>>,
        generation: &GenerationClock,
    ) -> Result<Self, String> {
        if let Some(bridge) = extension::extension_node_bridge() {
            return Ok(Self::Extension {
                bridge,
                token: generation.token(),
            });
        }
        node_ref
            .borrow()
            .clone()
            .map(|node| Self::Local {
                node,
                token: generation.token(),
            })
            .ok_or_else(|| "start the node first".to_string())
    }

    async fn connect_http(self, endpoint: String) -> Result<PeerUpdate, String> {
        match self {
            Self::Extension { bridge, token } => {
                let snapshot = extension::extension_node_connect_http(&bridge, endpoint).await?;
                Ok(PeerUpdate::Snapshot { snapshot, token })
            }
            Self::Local { node, token } => {
                let seed_did = node::connect_http(&node.provider, endpoint).await?;
                let required_peer = PeerView::connected(seed_did)
                    .ok_or_else(|| "seed returned an empty DID".to_string())?;
                Ok(PeerUpdate::Local {
                    node,
                    token,
                    context: "HTTP endpoint connected",
                    required_peer: Some(required_peer),
                })
            }
        }
    }

    async fn create_offer(self, remote_did: String) -> Result<String, String> {
        match self {
            Self::Extension { bridge, .. } => {
                extension::extension_node_create_offer(&bridge, remote_did).await
            }
            Self::Local { node, .. } => node::create_offer(&node.provider, remote_did).await,
        }
    }

    async fn answer_offer(self, offer: String) -> Result<String, String> {
        match self {
            Self::Extension { bridge, .. } => {
                extension::extension_node_answer_offer(&bridge, offer).await
            }
            Self::Local { node, .. } => node::answer_offer(&node.provider, offer).await,
        }
    }

    async fn accept_answer(self, answer: String) -> Result<PeerUpdate, String> {
        match self {
            Self::Extension { bridge, token } => {
                let snapshot = extension::extension_node_accept_answer(&bridge, answer).await?;
                Ok(PeerUpdate::Snapshot { snapshot, token })
            }
            Self::Local { node, token } => {
                node::accept_answer(&node.provider, answer).await?;
                Ok(PeerUpdate::Local {
                    node,
                    token,
                    context: "answer accepted",
                    required_peer: None,
                })
            }
        }
    }
}

impl PeerUpdate {
    async fn apply(self, peers: UseStateHandle<Vec<PeerView>>, status: UseStateHandle<String>) {
        match self {
            Self::Snapshot { snapshot, token } => {
                if token.is_current() {
                    peers.set(snapshot.peers);
                    status.set(snapshot.error.unwrap_or(snapshot.message));
                }
            }
            Self::Local {
                node,
                token,
                context,
                required_peer,
            } => {
                peer_sync::sync_peers_after_handshake(
                    node,
                    peers,
                    status,
                    context,
                    required_peer,
                    move || token.is_current(),
                )
                .await;
            }
        }
    }
}

fn required_input(value: String, empty_message: &'static str) -> Result<String, String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(empty_message.to_string())
    } else {
        Ok(value)
    }
}

pub(crate) fn link_control(
    state: ConnectState<'_>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    peers: UseStateHandle<Vec<PeerView>>,
    status: UseStateHandle<String>,
) -> Html {
    let on_http_connect = {
        let node_ref = node_ref.clone();
        let generation = generation.clone();
        let endpoint = (*state.http_endpoint).clone();
        let peers = peers.clone();
        let status = status.clone();
        let link_dialog_open = (*state.link_dialog_open).clone();
        Callback::from(move |_| {
            let backend = match NodeBackend::current(&node_ref, &generation) {
                Ok(backend) => backend,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };
            let endpoint = match required_input((*endpoint).clone(), "enter a seed HTTP endpoint") {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };
            let peers = peers.clone();
            let status = status.clone();
            let link_dialog_open = link_dialog_open.clone();
            status.set(format!("connecting {endpoint}"));
            wasm_bindgen_futures::spawn_local(async move {
                match backend.connect_http(endpoint).await {
                    Ok(update) => {
                        link_dialog_open.set(false);
                        update.apply(peers, status).await;
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };
    let on_create_offer = {
        let node_ref = node_ref.clone();
        let generation = generation.clone();
        let remote_did = (*state.sdp_remote_did).clone();
        let generated_offer = (*state.generated_offer).clone();
        let status = status.clone();
        Callback::from(move |_| {
            let backend = match NodeBackend::current(&node_ref, &generation) {
                Ok(backend) => backend,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };
            let remote_did = match required_input((*remote_did).clone(), "enter a remote DID") {
                Ok(remote_did) => remote_did,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };
            let generated_offer = generated_offer.clone();
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match backend.create_offer(remote_did).await {
                    Ok(offer) => {
                        generated_offer.set(offer);
                        status.set("offer created".to_string());
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };
    let on_answer_offer = {
        let node_ref = node_ref.clone();
        let generation = generation.clone();
        let remote_offer = (*state.remote_offer).clone();
        let generated_answer = (*state.generated_answer).clone();
        let status = status.clone();
        Callback::from(move |_| {
            let backend = match NodeBackend::current(&node_ref, &generation) {
                Ok(backend) => backend,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };
            let offer = match required_input((*remote_offer).clone(), "paste an offer first") {
                Ok(offer) => offer,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };
            let generated_answer = generated_answer.clone();
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match backend.answer_offer(offer).await {
                    Ok(answer) => {
                        generated_answer.set(answer);
                        status.set("answer created".to_string());
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };
    let on_accept_answer = {
        let node_ref = node_ref.clone();
        let generation = generation.clone();
        let remote_answer = (*state.remote_answer).clone();
        let peers = peers.clone();
        let status = status.clone();
        let link_dialog_open = (*state.link_dialog_open).clone();
        Callback::from(move |_| {
            let backend = match NodeBackend::current(&node_ref, &generation) {
                Ok(backend) => backend,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };
            let answer = match required_input((*remote_answer).clone(), "paste an answer first") {
                Ok(answer) => answer,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };
            let peers = peers.clone();
            let status = status.clone();
            let link_dialog_open = link_dialog_open.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match backend.accept_answer(answer).await {
                    Ok(update) => {
                        link_dialog_open.set(false);
                        update.apply(peers, status).await;
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };
    let set_initiator = {
        let sdp_mode = (*state.sdp_mode).clone();
        Callback::from(move |_| sdp_mode.set(SdpMode::Initiator))
    };
    let set_responder = {
        let sdp_mode = (*state.sdp_mode).clone();
        Callback::from(move |_| sdp_mode.set(SdpMode::Responder))
    };
    let open_dialog = {
        let link_dialog_open = (*state.link_dialog_open).clone();
        Callback::from(move |_| link_dialog_open.set(true))
    };
    let close_dialog = {
        let link_dialog_open = (*state.link_dialog_open).clone();
        Callback::from(move |_| link_dialog_open.set(false))
    };
    let set_manual_sdp = {
        let link_tab = (*state.link_tab).clone();
        Callback::from(move |_| link_tab.set(LinkTab::ManualSdp))
    };
    let set_http_endpoint = {
        let link_tab = (*state.link_tab).clone();
        Callback::from(move |_| link_tab.set(LinkTab::HttpEndpoint))
    };

    html! {
        <div class="node-link-control">
            if !state.launcher_hidden {
                <button class="topology-add-button" type="button" aria-label="Connect peer" title="Connect peer" onclick={open_dialog}>
                    <span aria-hidden="true">{ "+" }</span>
                </button>
            }
            {
                if **state.link_dialog_open {
                    connect_dialog(
                        ConnectDialogView {
                            active_tab: **state.link_tab,
                            active_sdp_mode: **state.sdp_mode,
                            http_endpoint: (*state.http_endpoint).clone(),
                            sdp_remote_did: (*state.sdp_remote_did).clone(),
                            generated_offer: (*state.generated_offer).clone(),
                            remote_offer: (*state.remote_offer).clone(),
                            generated_answer: (*state.generated_answer).clone(),
                            remote_answer: (*state.remote_answer).clone(),
                            status: status.clone(),
                        },
                        ConnectDialogActions {
                            set_manual_sdp,
                            set_http_endpoint,
                            set_initiator,
                            set_responder,
                            close_dialog,
                            on_http_connect,
                            on_create_offer,
                            on_answer_offer,
                            on_accept_answer,
                        },
                    )
                } else {
                    html! {}
                }
            }
        </div>
    }
}

fn connect_dialog(view: ConnectDialogView, actions: ConnectDialogActions) -> Html {
    html! {
        <div class="modal-shell">
            <button class="dialog-backdrop" aria-label="Close link dialog" onclick={actions.close_dialog.clone()}></button>
            <section class="link-dialog" role="dialog" aria-modal="true" aria-labelledby="link-dialog-title">
                <header class="dialog-header">
                    <div>
                        <p class="eyebrow">{ "Peer link" }</p>
                        <h2 id="link-dialog-title">{ "Connection exchange" }</h2>
                    </div>
                    <button class="secondary dialog-close" onclick={actions.close_dialog}>{ "Close" }</button>
                </header>
                { link_dialog_tabs(view.active_tab, actions.set_manual_sdp, actions.set_http_endpoint) }
                <div class="dialog-body">
                    {
                        match view.active_tab {
                            LinkTab::ManualSdp => html! {
                                <div class="dialog-pane sdp-tool">
                                    <div class="tool-header">
                                        <h3>{ "Manual SDP exchange" }</h3>
                                        { sdp_mode_switch(
                                            view.active_sdp_mode,
                                            actions.set_initiator,
                                            actions.set_responder,
                                        ) }
                                    </div>
                                    {
                                        match view.active_sdp_mode {
                                            SdpMode::Initiator => sdp_initiator_flow(
                                                view.sdp_remote_did,
                                                view.generated_offer,
                                                view.remote_answer,
                                                view.status.clone(),
                                                actions.on_create_offer,
                                                actions.on_accept_answer,
                                            ),
                                            SdpMode::Responder => sdp_responder_flow(
                                                view.remote_offer,
                                                view.generated_answer,
                                                view.status.clone(),
                                                actions.on_answer_offer,
                                            ),
                                        }
                                    }
                                </div>
                            },
                            LinkTab::HttpEndpoint => html! {
                                <div class="dialog-pane http-pane">
                                    <div class="tool-header">
                                        <h3>{ "HTTP endpoint" }</h3>
                                        <span class="payload-state">{ "Seed" }</span>
                                    </div>
                                    { text_input("Seed HTTP endpoint", view.http_endpoint) }
                                    <button onclick={actions.on_http_connect}>{ "Connect endpoint" }</button>
                                </div>
                            },
                        }
                    }
                </div>
            </section>
        </div>
    }
}

fn link_dialog_tabs(
    active: LinkTab,
    set_manual_sdp: Callback<MouseEvent>,
    set_http_endpoint: Callback<MouseEvent>,
) -> Html {
    let manual_class = if active == LinkTab::ManualSdp {
        "dialog-tab active"
    } else {
        "dialog-tab"
    };
    let http_class = if active == LinkTab::HttpEndpoint {
        "dialog-tab active"
    } else {
        "dialog-tab"
    };
    html! {
        <nav class="dialog-tabs" aria-label="Connection mode">
            <button class={manual_class} onclick={set_manual_sdp}>{ "Manual SDP" }</button>
            <button class={http_class} onclick={set_http_endpoint}>{ "HTTP endpoint" }</button>
        </nav>
    }
}

fn sdp_mode_switch(
    active: SdpMode,
    set_initiator: Callback<MouseEvent>,
    set_responder: Callback<MouseEvent>,
) -> Html {
    let initiator_class = if active == SdpMode::Initiator {
        "segment active"
    } else {
        "segment"
    };
    let responder_class = if active == SdpMode::Responder {
        "segment active"
    } else {
        "segment"
    };
    html! {
        <div class="segmented" aria-label="SDP role">
            <button class={initiator_class} onclick={set_initiator}>{ "Initiator" }</button>
            <button class={responder_class} onclick={set_responder}>{ "Responder" }</button>
        </div>
    }
}

fn sdp_initiator_flow(
    remote_did: UseStateHandle<String>,
    generated_offer: UseStateHandle<String>,
    remote_answer: UseStateHandle<String>,
    status: UseStateHandle<String>,
    on_create_offer: Callback<MouseEvent>,
    on_accept_answer: Callback<MouseEvent>,
) -> Html {
    html! {
        <div class="sdp-flow">
            { sdp_step(
                "1",
                "Remote DID",
                html! {
                    <>
                        { text_input("Remote DID", remote_did) }
                        <button onclick={on_create_offer}>{ "Create offer" }</button>
                    </>
                },
            ) }
            { sdp_output_step("2", "Local offer", (*generated_offer).clone(), status) }
            { sdp_step(
                "3",
                "Remote answer",
                html! {
                    <>
                        { textarea("Remote answer", remote_answer) }
                        <button onclick={on_accept_answer}>{ "Accept answer" }</button>
                    </>
                },
            ) }
        </div>
    }
}

fn sdp_responder_flow(
    remote_offer: UseStateHandle<String>,
    generated_answer: UseStateHandle<String>,
    status: UseStateHandle<String>,
    on_answer_offer: Callback<MouseEvent>,
) -> Html {
    html! {
        <div class="sdp-flow">
            { sdp_step(
                "1",
                "Remote offer",
                html! {
                    <>
                        { textarea("Remote offer", remote_offer) }
                        <button onclick={on_answer_offer}>{ "Answer offer" }</button>
                    </>
                },
            ) }
            { sdp_output_step("2", "Local answer", (*generated_answer).clone(), status) }
        </div>
    }
}

fn sdp_step(index: &'static str, title: &'static str, body: Html) -> Html {
    html! {
        <div class="sdp-step">
            <div class="sdp-index">{ index }</div>
            <div class="sdp-step-body">
                <h4>{ title }</h4>
                { body }
            </div>
        </div>
    }
}

fn sdp_output_step(
    index: &'static str,
    title: &'static str,
    value: String,
    status: UseStateHandle<String>,
) -> Html {
    let can_copy = !value.trim().is_empty();
    let state = if value.trim().is_empty() {
        "Waiting"
    } else {
        "Ready"
    };
    let on_copy = {
        let value = value.clone();
        Callback::from(move |_| {
            if value.trim().is_empty() {
                status.set("generate SDP first".to_string());
                return;
            }
            let value = value.clone();
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match extension::copy_text_to_clipboard(value).await {
                    Ok(()) => status.set(format!("{title} copied")),
                    Err(error) => status.set(format!("copy SDP failed: {error}")),
                }
            });
        })
    };
    html! {
        <div class="sdp-step">
            <div class="sdp-index">{ index }</div>
            <div class="sdp-step-body">
                <div class="sdp-output-header">
                    <h4>{ title }</h4>
                    <div class="sdp-output-actions">
                        <span class="payload-state">{ state }</span>
                        <button
                            class="copy-button sdp-copy"
                            type="button"
                            disabled={!can_copy}
                            onclick={on_copy}
                        >
                            { "Copy" }
                        </button>
                    </div>
                </div>
                { readonly_textarea(title, value) }
            </div>
        </div>
    }
}
