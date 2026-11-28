//! Landing page for the browser frontend: hero, features, architecture, runtime, examples.

use yew::prelude::*;

use crate::controls::ShellPage;
use crate::links::Chapter;
use crate::links::ProjectLink;
use crate::project_content::PROJECT_INTRODUCTION;
use crate::project_content::PROJECT_NAME;
use crate::project_content::PROJECT_TAGLINE;

/// Render the Rings Network landing page.
pub(crate) fn page(
    navigate_page: Callback<ShellPage>,
    active_architecture_layer: UseStateHandle<usize>,
) -> Html {
    let open_console = {
        let navigate_page = navigate_page.clone();
        Callback::from(move |_| navigate_page.emit(ShellPage::Console))
    };
    html! {
        <section class="site-page landing-page" aria-labelledby="landing-title">
            { hero_section(open_console.clone()) }
            { features_section() }
            { architecture_section(active_architecture_layer) }
            { runtime_section() }
            { services_section() }
            { examples_section() }
            { blog_section() }
            { final_section(open_console) }
        </section>
    }
}

/// Render the top hero section with tagline, introduction, and primary actions.
fn hero_section(open_console: Callback<MouseEvent>) -> Html {
    html! {
        <section class="landing-hero" aria-labelledby="landing-title">
            <div class="landing-hero-copy">
                <p class="landing-kicker">{ PROJECT_NAME }</p>
                <h1 id="landing-title">{ PROJECT_TAGLINE }</h1>
                <p class="landing-lede">
                    { PROJECT_INTRODUCTION }
                </p>
                <div class="landing-actions" aria-label="Primary actions">
                    <button class="landing-primary-action" type="button" onclick={open_console}>
                        { "Open Node" }
                    </button>
                    { for ProjectLink::HERO.into_iter().map(|link| link.anchor("landing-secondary-action")) }
                </div>
            </div>
        </section>
    }
}

/// Render the four core capability feature cards.
fn features_section() -> Html {
    html! {
        <section class="landing-section landing-feature-section" aria-label="Features">
            <div class="landing-section-heading">
                <p>{ "Features" }</p>
            </div>
            <div class="landing-feature-grid">
                { landing_feature("Browser-native peers and TURN", "Runs in browsers through WebAssembly and web_sys, and on native hosts through the same Rust node stack. Direct WebRTC datachannels traverse NATs via UDP/TCP TURN and DHT-discovered multi-hop Onion TURN.", "assets/images/feature-network-background.webp") }
                { landing_feature("DID identity and cryptography", "Peers are addressed by decentralized identifiers backed by 6 signature schemes—secp256k1, EIP-191, BIP-137, secp256r1, Ed25519, and BLS12-381—across WebCrypto, MetaMask, Phantom, and C FFI signers.", "assets/images/feature-did-identity.webp") }
                { landing_feature("Structured peer routing", "A range-proved Chord DHT provides O(log N) finger convergence, non-transitive gap bridging, voluntary Leave handoffs, authenticated CRDT data topics, and network_id isolation.", "assets/images/feature-peer-routing.webp") }
                { landing_feature("Protocol and onion runtime", "Application protocols run as pure Protocol::step machines under an Interpret capability shell, alongside Sphinx/AEZ onion loops for TCP, UDP, HTTPS, and composable Kleisli invoke pipelines.", "assets/images/feature-protocol-runtime.webp") }
            </div>
        </section>
    }
}

/// Render the interactive seven-layer architecture explorer.
fn architecture_section(active_architecture_layer: UseStateHandle<usize>) -> Html {
    let selected_index = selected_architecture_index(*active_architecture_layer);
    let Some(selected_layer) = ARCHITECTURE_LAYERS.get(selected_index) else {
        return html! {};
    };

    html! {
        <section class="landing-section landing-architecture" aria-labelledby="landing-architecture-title">
            <div class="landing-section-heading">
                <p>{ "Architecture" }</p>
                <h2 id="landing-architecture-title">{ "Every layer is decentralized." }</h2>
                <p class="landing-section-lede">
                    { "Rings maps applications, protocols, extension runtime, Sphinx onion privacy, overlay routing, transport, and identity directly to repository crates and modules. Select a layer to inspect its role." }
                </p>
            </div>
            <div class="landing-architecture-grid">
                <div class="landing-layer-stack" aria-label="Rings architecture layers">
                    { for ARCHITECTURE_LAYERS.iter().enumerate().map(|(index, layer)| {
                        architecture_layer_tab(
                            index,
                            layer,
                            index == selected_index,
                            active_architecture_layer.clone(),
                        )
                    }) }
                </div>
                { architecture_layer_detail(selected_layer) }
            </div>
        </section>
    }
}

/// Clamp the active architecture layer index to the valid bounds of [`ARCHITECTURE_LAYERS`].
fn selected_architecture_index(index: usize) -> usize {
    index.min(ARCHITECTURE_LAYERS.len().saturating_sub(1))
}

/// Render the detail panel for the currently selected architecture layer.
fn architecture_layer_detail(layer: &ArchitectureLayer) -> Html {
    html! {
        <aside class="landing-layer-detail" aria-live="polite" aria-label="Selected architecture layer">
            <div class="landing-layer-detail-heading">
                <span class="landing-layer-detail-index">{ layer.index }</span>
                <div>
                    <span class="landing-layer-label">{ layer.label }</span>
                    <h3>{ layer.title }</h3>
                </div>
            </div>
            <section class="landing-layer-detail-section">
                <span>{ "Summary" }</span>
                <p class="landing-layer-detail-summary">{ layer.summary }</p>
            </section>
            <section class="landing-layer-detail-section">
                <span>{ "Responsibilities" }</span>
                <p>{ layer.detail }</p>
            </section>
            <dl class="landing-layer-detail-list">
                <div>
                    <dt>{ "Repository surface" }</dt>
                    <dd>{ layer.surface }</dd>
                </div>
                <div>
                    <dt>{ "Contract / invariant" }</dt>
                    <dd>{ layer.contract }</dd>
                </div>
            </dl>
        </aside>
    }
}

/// Render the protocol-extension runtime code preview section.
fn runtime_section() -> Html {
    html! {
        <section class="landing-section landing-runtime" aria-labelledby="landing-runtime-title">
            <div class="landing-section-heading">
                <p>{ "Extending Rings" }</p>
                <h2 id="landing-runtime-title">{ "Pure protocol core, scoped interpreter shell." }</h2>
                <p class="landing-section-lede">
                    { "One core engine across four deployment targets — embed Rings seamlessly in browsers, native daemons, C/Swift/Kotlin apps, or extensions." }
                </p>
            </div>
            <div class="landing-runtime-visual">
                <pre class="landing-code"><code>{ "provider.register_protocol(Echo, EchoShell)?;\nprovider.set_backend()?;\n\nlet relay = RelayHandle::install(&provider.extensions())?;\nrelay\n    .register_tcp_service(\"web\".into(), \"127.0.0.1:8080\".parse()?)\n    .await?;\nrelay\n    .open_tcp_tunnel(local_addr, peer_did, \"web\".into())\n    .await?;" }</code></pre>
            </div>
        </section>
    }
}

/// Render the built-in services showcase grid.
fn services_section() -> Html {
    html! {
        <section class="landing-section landing-services" aria-labelledby="landing-services-title">
            <div class="landing-section-heading">
                <p>{ "Built-in services" }</p>
                <h2 id="landing-services-title">{ "Operator tooling, decentralized TURN, and gateways." }</h2>
                <p class="landing-section-lede">
                    { "Shipped across the 18 workspace crates to run, inspect, and bridge Rings overlays without central infrastructure." }
                </p>
            </div>
            <div class="landing-example-grid">
                { landing_link_card("rings-tui", "Six-tab terminal dashboard for monitoring peers, sparklines, DHT storage, online/exit registries, and live RPC commands.", Chapter::Tui.href()) }
                { landing_link_card("TURN & Onion TURN", "Stateless signature-as-password UDP/TCP TURN server (rings-turn) and DHT-discovered multi-hop onion::udp bridge (rings-turn-service).", Chapter::TurnServer.href()) }
                { landing_link_card("Onion & Kleisli OP", "Guard-closed Sphinx/AEZ loops serving TCP, UDP, and HTTPS exits plus composable typed invoke pipelines such as turn.ticket.", Chapter::Onion.href()) }
                { landing_link_card("Native TUN gateway", "Selective IPv4/TCP TUN packet gateway bridging captured flows to onion::tcp with privilege separation via gateway-config-unix.", Chapter::NativeGateway.href()) }
                { landing_link_card("DNS relay", "Authoritative <did>.<zone> AAAA/MX/TXT DNS authority and IPv6 TCP ingress relay with optional PROXY protocol v2.", Chapter::DnsRelay.href()) }
                { landing_link_card("Mail gateway", "Stateless SMTP-to-DHT mail gateway delivering sealed messages to <did>@<domain> recipients and local IMAP/submission mailboxes.", Chapter::MailGateway.href()) }
            </div>
        </section>
    }
}

/// Render the runnable repository examples section.
fn examples_section() -> Html {
    html! {
        <section class="landing-section landing-examples" aria-labelledby="landing-examples-title">
            <div class="landing-section-heading">
                <p>{ "Examples" }</p>
                <h2 id="landing-examples-title">{ "Runnable surfaces from the repository." }</h2>
            </div>
            <div class="landing-example-grid">
                { landing_link_card("native", "Start here for a minimal native node. Demonstrates wallet setup, node bootstrapping, extension protocols, and E2E encrypted streams.", "https://github.com/RingsNetwork/rings/tree/master/examples/native") }
                { landing_link_card("relay", "Open direct TCP and UDP tunnels through the overlay to expose and consume peer services end-to-end.", "https://github.com/RingsNetwork/rings/tree/master/examples/relay") }
                { landing_link_card("dweb", "Explore the decentralized-web application shape, serving and addressing web content directly across the Rings peer overlay.", "https://github.com/RingsNetwork/rings/tree/master/examples/dweb") }
                { landing_link_card("mailbox", "Run a complete DID mailbox node and stateless SMTP MX gateway delivering sealed mail over the Chord DHT.", "https://github.com/RingsNetwork/rings/tree/master/examples/mailbox") }
                { landing_link_card("ffi", "Drive a Rings node from Python, Swift, Kotlin, or C through the C ABI in rings.h to embed P2P networking in host apps.", "https://github.com/RingsNetwork/rings/tree/master/examples/ffi") }
                { landing_link_card("frontend & extension", "Inspect the Rust/Yew browser node console, Onion WebView service worker, and Chrome MV3 extension package.", "https://github.com/RingsNetwork/rings/tree/master/examples/frontend") }
            </div>
        </section>
    }
}

/// Render the engineering blog highlights section.
fn blog_section() -> Html {
    html! {
        <section class="landing-section landing-blog" aria-labelledby="landing-blog-title">
            <div class="landing-section-heading">
                <p>{ "Engineering Blog" }</p>
                <h2 id="landing-blog-title">{ "Design notes from rings.rs/blog." }</h2>
                <p class="landing-section-lede">
                    { "Architecture deep dives on stateless cryptographic TURN, Onion OP pipelines, Chord finger convergence, and algebraic correctness." }
                </p>
            </div>
            <div class="landing-example-grid">
                { landing_link_card("We Removed the Last Dark Cloud Over Rings Network", "Bridging stateless cryptographic TURN allocations over multi-hop onion::udp circuits so even Symmetric NAT traversal preserves Tor-grade endpoint separation.", "https://rings.rs/blog/we-removed-the-last-dark-cloud-over-rings-network/") }
                { landing_link_card("From Onion Routing to Onion OP", "Expanding onion privacy from hiding communication endpoints to composing multi-step operations across mutually untrusted nodes.", "https://rings.rs/blog/from-onion-routing-to-onion-op-the-evolution-of-privacy-in-rings/") }
                { landing_link_card("One Lookup, Many Fingers", "Range-proved finger table convergence for browser meshes and the Stateright finite-state models that verified it.", "https://rings.rs/blog/one-lookup-many-fingers/") }
            </div>
            <div class="landing-actions">
                { ProjectLink::Blog.anchor("landing-secondary-action") }
            </div>
        </section>
    }
}

/// Render the closing call-to-action banner linking to the browser node console.
fn final_section(open_console: Callback<MouseEvent>) -> Html {
    html! {
        <section class="landing-final" aria-label="Open Rings Node">
            <div>
                <p>{ "Frontend" }</p>
                <h2>{ "Use the browser node for the live network surface." }</h2>
                <span>
                    { "Launch a browser node right in this tab, or connect a local daemon to explore peer-to-peer routing, decentralized storage, and private services." }
                </span>
            </div>
            <button class="landing-primary-action" type="button" onclick={open_console}>
                { "Open Node" }
            </button>
        </section>
    }
}

/// Descriptor for one layer in the landing page's interactive architecture stack.
struct ArchitectureLayer {
    /// Two-digit ordinal label for the layer.
    index: &'static str,
    /// Short layer identifier shown on the tab button.
    label: &'static str,
    /// One-line role phrase shown beneath the tab label.
    role: &'static str,
    /// Detailed layer heading displayed in the inspector pane.
    title: &'static str,
    /// High-level summary of the layer.
    summary: &'static str,
    /// Expanded description of the layer's responsibilities.
    detail: &'static str,
    /// Workspace crates and modules implementing the layer.
    surface: &'static str,
    /// Architectural contract or invariant upheld by the layer.
    contract: &'static str,
}

/// The seven Rings architecture layers rendered in the landing page explorer.
const ARCHITECTURE_LAYERS: [ArchitectureLayer; 7] = [
    ArchitectureLayer {
        index: "01",
        label: "applications",
        role: "runs user-facing workflows.",
        title: "dWeb, TUI, mail, relay, and custom apps",
        summary: "Apps run directly over the peer-to-peer protocol layer with end-to-end sovereignty.",
        detail: "Application surfaces include the browser console, Chrome MV3 extension, rings-tui terminal workbench, DID mail gateway, and repository examples. They compose wallet login, dWeb content, relay tunnels, and custom protocol messages on top of the same peer runtime.",
        surface: "examples/frontend, crates/tui, examples/dweb, examples/relay, examples/mailbox, examples/ffi",
        contract: "Application code addresses peers and namespaces while lower layers manage overlay routing, privacy, and transport.",
    },
    ArchitectureLayer {
        index: "02",
        label: "protocols",
        role: "defines namespaced behavior.",
        title: "relay, echo, mail, TURN ticket, and user namespaces",
        summary: "Built-ins cover TCP/UDP relay, echo, DID mail, and DHT service registries; user protocols install behind namespaces.",
        detail: "Protocols register behind stable namespaces alongside built-in DHT registries (online_nodes, onion_exits, onion_invokes, turn_services) and onion-invoked backends such as turn.ticket. New application behavior is added by registering a protocol and its interpreter.",
        surface: "crates/node/src/extension, crates/turn-service, protocol registry, relay & echo",
        contract: "Every inbound envelope is dispatched by namespace before it reaches application-specific logic.",
    },
    ArchitectureLayer {
        index: "03",
        label: "runtime",
        role: "executes protocol state.",
        title: "pure Protocol::step plus Interpret shell",
        summary: "Protocol logic stays pure while side effects run through namespace-scoped capabilities and runtime contracts.",
        detail: "The runtime keeps deterministic protocol transitions separate from I/O. Pure step logic computes the next state and effects; the interpreter shell executes scoped side effects over rings-runtime contracts across Tokio native hosts and browser WebAssembly.",
        surface: "crates/runtime, Protocol::step, Interpret shell, provider extension hooks",
        contract: "State transitions are reproducible; I/O passes through explicit provider capabilities.",
    },
    ArchitectureLayer {
        index: "04",
        label: "privacy",
        role: "provides anonymous circuits.",
        title: "Sphinx/AEZ onion loops, exits, and Onion OP",
        summary: "Client-sealed Sphinx/AEZ v5 loops with zero-idle-cover burst shaping, pinned entry guards, TCP/UDP/HTTPS exits, and Kleisli invoke pipelines.",
        detail: "The privacy layer builds guard-closed multi-hop onion loops using rings-aez wide-block enciphering. Each hop peels one layer and learns only its immediate predecessor and successor, while exits serve TCP, UDP, and HTTPS traffic, feed Onion TURN and selective TUN/WebView gateways, and execute composable invoke stages.",
        surface: "crates/node/src/onion, crates/aez, crates/gateway, crates/webview, crates/network-policy",
        contract: "Each hop learns only its immediate neighbors; SURB replies return along the guard-closed loop without revealing client identity.",
    },
    ArchitectureLayer {
        index: "05",
        label: "overlay",
        role: "routes peer messages.",
        title: "Chord DHT, authenticated CRDTs, and replay",
        summary: "Range-proved finger convergence, non-transitive gap bridging, voluntary Leave, authenticated CRDT topics, and replay windows.",
        detail: "The overlay maps DIDs onto a 160-bit Chord ring with K=4 successor lists, 160 finger slots, non-transitive multi-predecessor & bridged-peer hints, and ack-gated carrier handoff on Leave. Replicated CRDT data topics carry per-writer Merkle provenance, bounded by SlotRoster admission, origin quotas, and group-committed replay windows.",
        surface: "crates/core, crates/measure, crates/dns-relay, crates/rpc",
        contract: "Routing selects peer paths by cryptographic identifier space with end-to-end DID origin authentication and replay protection.",
    },
    ArchitectureLayer {
        index: "06",
        label: "transport",
        role: "moves data between peers.",
        title: "decoupled Transport, WebRTC, and LZ4 wire",
        summary: "Pluggable TransportInterface with 0–8 step handshakes, UDP/TCP TURN, and LZ4-compressed WIRE_VERSION=6 framing.",
        detail: "Transport is decoupled from rings-core behind TransportInterface and ConnectionInterface traits (supporting WebRTC 0.17.3, browser RTCPeerConnection, dummy simulation, and custom transports via network: { kind, params }). Messages serialize with rings-codec postcard framing, LZ4 static-dictionary compression, delegation-digest leases, and 4 credit-bounded traffic lanes.",
        surface: "crates/transport, crates/codec, crates/turn, crates/turn-service, TURN over TCP",
        contract: "Transport handles connectivity, N-step binary handshakes, and 4-lane credit flow control while higher layers decide what is carried.",
    },
    ArchitectureLayer {
        index: "07",
        label: "identity",
        role: "authenticates peers.",
        title: "DID plus 6 selectable signature schemes",
        summary: "Bridges browser wallets, native daemons, TURN credentials, and C FFI hosts with delegated session keys.",
        detail: "Identity uses DID-addressable keys and time-bounded DelegateeKey sessions supporting 6 signature schemes (secp256k1, EIP-191, BIP-137, secp256r1, Ed25519, BLS12-381) with in-process rotation, ECDH stateless TURN authentication, and unlinkable TURN session tickets.",
        surface: "crates/core/src/delegation, WebCrypto P-256, MetaMask, Phantom, C FFI signer callbacks",
        contract: "Peers authenticate as DIDs through delegated session keys across all supported wallet, TURN, and native backends.",
    },
];

/// Render one illustrated feature card on the landing page.
fn landing_feature(title: &'static str, body: &'static str, image_src: &'static str) -> Html {
    html! {
        <article class="landing-feature-card">
            <img
                class="landing-feature-illustration"
                src={image_src}
                alt=""
                loading="lazy"
                decoding="async"
                aria-hidden="true"
            />
            <div class="landing-feature-copy">
                <h3>{ title }</h3>
                <p>{ body }</p>
            </div>
        </article>
    }
}

/// Render a selectable tab button for an architecture layer.
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

/// Render an external-link card used in the services, examples, and blog sections.
fn landing_link_card(title: &'static str, body: &'static str, href: &'static str) -> Html {
    html! {
        <a class="landing-example-card" href={href} target="_blank" rel="noreferrer">
            <h3>{ title }</h3>
            <p>{ body }</p>
        </a>
    }
}
