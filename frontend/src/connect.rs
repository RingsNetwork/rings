//! Peer connection controls and SDP exchange dialog.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsValue;
use yew::prelude::*;

use crate::extension;
use crate::forms::readonly_textarea;
use crate::forms::text_input;
use crate::forms::text_input_with_suggestions;
use crate::forms::textarea;
use crate::generation::GenerationClock;
use crate::generation::GenerationToken;
use crate::node;
use crate::node::DemoNode;
use crate::node::PeerView;
use crate::node::SEED_ENDPOINTS;
use crate::peer_sync;

/// Role taken during a manual SDP handshake.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum SdpMode {
    /// Creates an offer for a remote DID and later accepts the remote answer.
    Initiator,
    /// Accepts a remote offer and produces an answer to send back.
    Responder,
}

/// Active tab inside the peer connection dialog.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum LinkTab {
    /// Out-of-band manual SDP offer/answer exchange.
    ManualSdp,
    /// Direct connection via a seed node's HTTP endpoint.
    HttpEndpoint,
}

/// Borrowed reactive state handles backing the peer link control and dialog.
pub(crate) struct ConnectState<'a> {
    /// Seed HTTP endpoint URL input.
    pub(crate) http_endpoint: &'a UseStateHandle<String>,
    /// Remote peer DID input for creating an SDP offer.
    pub(crate) sdp_remote_did: &'a UseStateHandle<String>,
    /// Locally generated SDP offer payload.
    pub(crate) generated_offer: &'a UseStateHandle<String>,
    /// Remote SDP offer payload entered by the responder.
    pub(crate) remote_offer: &'a UseStateHandle<String>,
    /// Locally generated SDP answer payload.
    pub(crate) generated_answer: &'a UseStateHandle<String>,
    /// Remote SDP answer payload entered by the initiator.
    pub(crate) remote_answer: &'a UseStateHandle<String>,
    /// Selected manual SDP role.
    pub(crate) sdp_mode: &'a UseStateHandle<SdpMode>,
    /// Whether the peer connection modal dialog is open.
    pub(crate) link_dialog_open: &'a UseStateHandle<bool>,
    /// Active tab in the peer connection dialog.
    pub(crate) link_tab: &'a UseStateHandle<LinkTab>,
    /// Whether the floating `+` launcher button should be hidden behind another modal.
    pub(crate) launcher_hidden: bool,
}

/// Owned view model passed into the peer connection dialog renderer.
struct ConnectDialogView {
    /// Currently selected dialog tab.
    active_tab: LinkTab,
    /// Currently selected manual SDP role.
    active_sdp_mode: SdpMode,
    /// Seed HTTP endpoint URL state handle.
    http_endpoint: UseStateHandle<String>,
    /// Remote peer DID state handle.
    sdp_remote_did: UseStateHandle<String>,
    /// Locally generated SDP offer state handle.
    generated_offer: UseStateHandle<String>,
    /// Remote SDP offer state handle.
    remote_offer: UseStateHandle<String>,
    /// Locally generated SDP answer state handle.
    generated_answer: UseStateHandle<String>,
    /// Remote SDP answer state handle.
    remote_answer: UseStateHandle<String>,
    /// Shared status message state handle.
    status: UseStateHandle<String>,
}

/// Event callbacks wired to the peer connection dialog controls.
struct ConnectDialogActions {
    /// Switches the dialog to the manual SDP tab.
    set_manual_sdp: Callback<MouseEvent>,
    /// Switches the dialog to the HTTP endpoint tab.
    set_http_endpoint: Callback<MouseEvent>,
    /// Switches manual SDP mode to initiator.
    set_initiator: Callback<MouseEvent>,
    /// Switches manual SDP mode to responder.
    set_responder: Callback<MouseEvent>,
    /// Closes the peer connection dialog.
    close_dialog: Callback<MouseEvent>,
    /// Connects to the entered seed HTTP endpoint.
    on_http_connect: Callback<MouseEvent>,
    /// Generates an SDP offer for the entered remote DID.
    on_create_offer: Callback<MouseEvent>,
    /// Generates an SDP answer for the pasted remote offer.
    on_answer_offer: Callback<MouseEvent>,
    /// Accepts the pasted remote SDP answer to complete the handshake.
    on_accept_answer: Callback<MouseEvent>,
}

/// Active node backend targeted by peer connection operations.
enum NodeBackend {
    /// Background node controlled via the browser extension bridge.
    Extension {
        /// JavaScript bridge handle for the extension background node.
        bridge: JsValue,
        /// Generation token guarding against stale UI updates.
        token: GenerationToken,
    },
    /// In-page [`DemoNode`] running directly in the current window.
    Local {
        /// Running local demo node instance.
        node: DemoNode,
        /// Generation token guarding against stale UI updates.
        token: GenerationToken,
    },
}

/// Deferred peer-list and status update produced by a successful handshake operation.
enum PeerUpdate {
    /// Full peer and status snapshot returned by the extension bridge.
    Snapshot {
        /// Snapshot payload returned by the extension backend.
        snapshot: extension::ExtensionNodeSnapshot,
        /// Generation token checked before applying the snapshot.
        token: GenerationToken,
    },
    /// Post-handshake peer synchronization task for a local node.
    Local {
        /// Local demo node to query for updated peers.
        node: DemoNode,
        /// Generation token checked during peer synchronization.
        token: GenerationToken,
        /// Status prefix describing the completed handshake step.
        context: &'static str,
    },
}

impl NodeBackend {
    /// Resolves the active backend (extension bridge or local node), or returns an error if offline.
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

    /// Connects the node to a seed peer via its HTTP signaling endpoint.
    async fn connect_http(self, endpoint: String) -> Result<PeerUpdate, String> {
        match self {
            Self::Extension { bridge, token } => {
                let snapshot = extension::extension_node_connect_http(&bridge, endpoint).await?;
                Ok(PeerUpdate::Snapshot { snapshot, token })
            }
            Self::Local { node, token } => {
                let seed_did = node::connect_http(&node.provider, endpoint).await?;
                if seed_did.trim().is_empty() {
                    return Err("seed returned an empty DID".to_string());
                }
                Ok(PeerUpdate::Local {
                    node,
                    token,
                    context: "HTTP endpoint connected",
                })
            }
        }
    }

    /// Creates a manual WebRTC SDP offer targeting `remote_did`.
    async fn create_offer(self, remote_did: String) -> Result<String, String> {
        match self {
            Self::Extension { bridge, .. } => {
                extension::extension_node_create_offer(&bridge, remote_did).await
            }
            Self::Local { node, .. } => node::create_offer(&node.provider, remote_did).await,
        }
    }

    /// Processes a remote SDP offer and returns the local SDP answer.
    async fn answer_offer(self, offer: String) -> Result<String, String> {
        match self {
            Self::Extension { bridge, .. } => {
                extension::extension_node_answer_offer(&bridge, offer).await
            }
            Self::Local { node, .. } => node::answer_offer(&node.provider, offer).await,
        }
    }

    /// Accepts a remote SDP answer to complete an initiator handshake.
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
                })
            }
        }
    }
}

impl PeerUpdate {
    /// Applies the peer update to the UI handles if its generation token is still current.
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
            } => {
                peer_sync::sync_peers_after_handshake(node, peers, status, context, move || {
                    token.is_current()
                })
                .await;
            }
        }
    }
}

/// Trims `value` and returns `empty_message` as an error if the result is empty.
fn required_input(value: String, empty_message: &'static str) -> Result<String, String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(empty_message.to_string())
    } else {
        Ok(value)
    }
}

/// Renders the floating peer-connect launcher button and, when open, the connection modal.
pub(crate) fn link_control(
    state: ConnectState<'_>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    peers: UseStateHandle<Vec<PeerView>>,
    status: UseStateHandle<String>,
) -> Html {
    let open_dialog = link_dialog_callback(state.link_dialog_open, true);
    let close_dialog = link_dialog_callback(state.link_dialog_open, false);
    let actions = ConnectDialogActions {
        set_manual_sdp: link_tab_callback(state.link_tab, LinkTab::ManualSdp),
        set_http_endpoint: link_tab_callback(state.link_tab, LinkTab::HttpEndpoint),
        set_initiator: sdp_mode_callback(state.sdp_mode, SdpMode::Initiator),
        set_responder: sdp_mode_callback(state.sdp_mode, SdpMode::Responder),
        close_dialog,
        on_http_connect: http_connect_callback(
            &state,
            node_ref.clone(),
            generation.clone(),
            peers.clone(),
            status.clone(),
        ),
        on_create_offer: create_offer_callback(
            &state,
            node_ref.clone(),
            generation.clone(),
            status.clone(),
        ),
        on_answer_offer: answer_offer_callback(
            &state,
            node_ref.clone(),
            generation.clone(),
            status.clone(),
        ),
        on_accept_answer: accept_answer_callback(
            &state,
            node_ref,
            generation,
            peers,
            status.clone(),
        ),
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
                    connect_dialog(connect_dialog_view(&state, status), actions)
                } else {
                    html! {}
                }
            }
        </div>
    }
}

/// Builds the click callback that connects to a seed HTTP endpoint.
fn http_connect_callback(
    state: &ConnectState<'_>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    peers: UseStateHandle<Vec<PeerView>>,
    status: UseStateHandle<String>,
) -> Callback<MouseEvent> {
    let endpoint = (*state.http_endpoint).clone();
    let link_dialog_open = (*state.link_dialog_open).clone();
    Callback::from(move |_| {
        let backend = match NodeBackend::current(&node_ref, &generation) {
            Ok(backend) => backend,
            Err(error) => return status.set(error),
        };
        let endpoint = match required_input((*endpoint).clone(), "enter a seed HTTP endpoint") {
            Ok(endpoint) => endpoint,
            Err(error) => return status.set(error),
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
}

/// Builds the click callback that generates a manual SDP offer for the entered remote DID.
fn create_offer_callback(
    state: &ConnectState<'_>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    status: UseStateHandle<String>,
) -> Callback<MouseEvent> {
    let remote_did = (*state.sdp_remote_did).clone();
    let generated_offer = (*state.generated_offer).clone();
    Callback::from(move |_| {
        let backend = match NodeBackend::current(&node_ref, &generation) {
            Ok(backend) => backend,
            Err(error) => return status.set(error),
        };
        let remote_did = match required_input((*remote_did).clone(), "enter a remote DID") {
            Ok(remote_did) => remote_did,
            Err(error) => return status.set(error),
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
}

/// Builds the click callback that answers a pasted remote SDP offer.
fn answer_offer_callback(
    state: &ConnectState<'_>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    status: UseStateHandle<String>,
) -> Callback<MouseEvent> {
    let remote_offer = (*state.remote_offer).clone();
    let generated_answer = (*state.generated_answer).clone();
    Callback::from(move |_| {
        let backend = match NodeBackend::current(&node_ref, &generation) {
            Ok(backend) => backend,
            Err(error) => return status.set(error),
        };
        let offer = match required_input((*remote_offer).clone(), "paste an offer first") {
            Ok(offer) => offer,
            Err(error) => return status.set(error),
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
}

/// Builds the click callback that accepts a pasted remote SDP answer.
fn accept_answer_callback(
    state: &ConnectState<'_>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    peers: UseStateHandle<Vec<PeerView>>,
    status: UseStateHandle<String>,
) -> Callback<MouseEvent> {
    let remote_answer = (*state.remote_answer).clone();
    let link_dialog_open = (*state.link_dialog_open).clone();
    Callback::from(move |_| {
        let backend = match NodeBackend::current(&node_ref, &generation) {
            Ok(backend) => backend,
            Err(error) => return status.set(error),
        };
        let answer = match required_input((*remote_answer).clone(), "paste an answer first") {
            Ok(answer) => answer,
            Err(error) => return status.set(error),
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
}

/// Builds a click callback that sets the active manual [`SdpMode`].
fn sdp_mode_callback(sdp_mode: &UseStateHandle<SdpMode>, mode: SdpMode) -> Callback<MouseEvent> {
    let sdp_mode = sdp_mode.clone();
    Callback::from(move |_| sdp_mode.set(mode))
}

/// Builds a click callback that opens or closes the peer connection dialog.
fn link_dialog_callback(
    link_dialog_open: &UseStateHandle<bool>,
    open: bool,
) -> Callback<MouseEvent> {
    let link_dialog_open = link_dialog_open.clone();
    Callback::from(move |_| link_dialog_open.set(open))
}

/// Builds a click callback that selects the active [`LinkTab`].
fn link_tab_callback(link_tab: &UseStateHandle<LinkTab>, tab: LinkTab) -> Callback<MouseEvent> {
    let link_tab = link_tab.clone();
    Callback::from(move |_| link_tab.set(tab))
}

/// Clones handles from [`ConnectState`] into an owned [`ConnectDialogView`].
fn connect_dialog_view(
    state: &ConnectState<'_>,
    status: UseStateHandle<String>,
) -> ConnectDialogView {
    ConnectDialogView {
        active_tab: **state.link_tab,
        active_sdp_mode: **state.sdp_mode,
        http_endpoint: (*state.http_endpoint).clone(),
        sdp_remote_did: (*state.sdp_remote_did).clone(),
        generated_offer: (*state.generated_offer).clone(),
        remote_offer: (*state.remote_offer).clone(),
        generated_answer: (*state.generated_answer).clone(),
        remote_answer: (*state.remote_answer).clone(),
        status,
    }
}

/// Renders the peer connection modal dialog with manual SDP and HTTP seed tabs.
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
                                    { text_input_with_suggestions("Seed HTTP endpoint", view.http_endpoint, "seed-http-endpoints", &SEED_ENDPOINTS) }
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

/// Renders the tab navigation bar switching between manual SDP and HTTP endpoint modes.
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

/// Renders the segmented control switching between initiator and responder SDP roles.
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

/// Renders the three-step manual SDP initiator workflow.
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

/// Renders the two-step manual SDP responder workflow.
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

/// Renders a numbered step container in the manual SDP exchange flow.
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

/// Renders a numbered read-only SDP payload step with a clipboard copy action.
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
