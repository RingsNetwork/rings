//! Landing page for the browser frontend: hero, features, architecture, runtime, examples.

use yew::prelude::*;

use crate::controls::ShellPage;
use crate::links::ProjectLink;
use crate::project_content::PROJECT_INTRODUCTION;
use crate::project_content::PROJECT_NAME;
use crate::project_content::PROJECT_TAGLINE;

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
            { examples_section() }
            { final_section(open_console) }
        </section>
    }
}

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

fn features_section() -> Html {
    html! {
        <section class="landing-section landing-feature-section" aria-label="Features">
            <div class="landing-section-heading">
                <p>{ "Features" }</p>
            </div>
            <div class="landing-feature-grid">
                { landing_feature("Browser-native peers", "Runs in browsers through WebAssembly and web_sys, and on native hosts through the same Rust node stack. WebRTC datachannels carry browser-to-browser and daemon traffic without an application server in the data path.", "assets/images/feature-network-background.png") }
                { landing_feature("DID identity and cryptography", "Peers are addressed by decentralized identifiers backed by selectable signature schemes, including secp256k1, secp256r1, ed25519, BLS, and bip137.", "assets/images/feature-did-identity.png") }
                { landing_feature("Structured peer routing", "A Chord DHT provides successor and finger-table routing, DID lookup, message relay, stabilization, and network_id isolation for independent overlays.", "assets/images/feature-peer-routing.png") }
                { landing_feature("Protocol runtime", "Application protocols are namespace-scoped. A pure step function owns state transitions while an Interpret shell performs side effects through a scoped capability.", "assets/images/feature-protocol-runtime.png") }
            </div>
        </section>
    }
}

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

fn selected_architecture_index(index: usize) -> usize {
    index.min(ARCHITECTURE_LAYERS.len().saturating_sub(1))
}

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
                <pre class="landing-code"><code>{ "provider.register_protocol(Echo, EchoShell)?;\nprovider.set_backend()?;\n\nlet relay = RelayHandle::install(&provider.extensions())?;\nrelay\n    .register_tcp_service(\"web\".into(), \"example.com:80\".parse()?)\n    .await?;\nrelay\n    .open_tcp_tunnel(local_addr, peer_did, \"web\".into())\n    .await?;" }</code></pre>
            </div>
        </section>
    }
}

fn examples_section() -> Html {
    html! {
        <section class="landing-section landing-examples" aria-labelledby="landing-examples-title">
            <div class="landing-section-heading">
                <p>{ "Examples" }</p>
                <h2 id="landing-examples-title">{ "Runnable surfaces from the repository." }</h2>
            </div>
            <div class="landing-example-grid">
                { landing_link_card("native", "Start here for a minimal native node. Demonstrates wallet setup, node bootstrapping, and registering a custom namespaced protocol in pure Rust.", "https://github.com/RingsNetwork/rings/tree/master/examples/native") }
                { landing_link_card("relay", "Open direct TCP and UDP tunnels through the overlay to expose and consume peer services end-to-end.", "https://github.com/RingsNetwork/rings/tree/master/examples/relay") }
                { landing_link_card("dweb", "Explore the decentralized-web application shape, serving and addressing web content directly across the Rings peer overlay.", "https://github.com/RingsNetwork/rings/tree/master/examples/dweb") }
                { landing_link_card("ffi", "Drive a Rings node from Python, Swift, Kotlin, or C through the C FFI to embed peer-to-peer networking in desktop and mobile apps.", "https://github.com/RingsNetwork/rings/tree/master/examples/ffi") }
            </div>
        </section>
    }
}

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

const ARCHITECTURE_LAYERS: [ArchitectureLayer; 7] = [
    ArchitectureLayer {
        index: "01",
        label: "applications",
        role: "runs user-facing workflows.",
        title: "dWeb, relay, and custom apps",
        summary: "Apps run directly over the peer-to-peer protocol layer with end-to-end sovereignty.",
        detail: "Application surfaces are repository examples and browser node panels. They compose wallet login, dWeb content, relay tunnels, and custom protocol messages on top of the same peer runtime. The application layer reads as product-facing behavior: it chooses what to ask the network to do, while the lower layers handle addressing, routing, privacy, and transport.",
        surface: "frontend Node page, examples/dweb, examples/relay, examples/mailbox",
        contract: "Application code addresses peers and namespaces while lower layers manage overlay routing and transport.",
    },
    ArchitectureLayer {
        index: "02",
        label: "protocols",
        role: "defines namespaced behavior.",
        title: "relay, echo, and user namespaces",
        summary: "Built-ins cover TCP/UDP relay and echo; user protocols are addressed by namespace.",
        detail: "Protocols are registered behind stable namespaces. Built-in protocols cover relay and echo flows, while external applications can install their own protocol state machines on the shared overlay. This layer is the extension boundary: new behavior is added by registering a protocol and its interpreter.",
        surface: "protocol registry, relay handles, echo protocol, custom namespaces",
        contract: "Every inbound envelope is dispatched by namespace before it reaches application-specific logic.",
    },
    ArchitectureLayer {
        index: "03",
        label: "runtime",
        role: "executes protocol state.",
        title: "pure Protocol::step plus Interpret shell",
        summary: "Protocol logic stays pure while side effects run through namespace-scoped capabilities.",
        detail: "The runtime keeps deterministic protocol transitions separate from IO. Pure step logic computes the next state and effects; the interpreter shell executes scoped side effects. This makes protocol behavior straightforward to test and reason about, with replayable state transitions cleanly separated from browser APIs, native sockets, storage, and wallet interaction.",
        surface: "Protocol::step, Interpret shell, provider extension hooks",
        contract: "State transitions are reproducible; IO passes through explicit provider capabilities.",
    },
    ArchitectureLayer {
        index: "04",
        label: "privacy",
        role: "provides anonymous circuits.",
        title: "Sphinx onion loops and exits",
        summary: "Client-sealed Sphinx/AEZ loops with constant-rate cover, pinned entry guards, and TCP/HTTPS exits.",
        detail: "The privacy layer builds multi-hop onion loops anchored at the client's pinned entry guard. Each relay peels a single Sphinx/AEZ layer and learns only its immediate predecessor and successor, while constant-rate link emission substitutes queued cells for cover traffic and exits serve TCP streams and HTTPS fetches.",
        surface: "crates/node/src/onion, crates/gateway, crates/webview",
        contract: "Each hop learns only its immediate neighbors; exit replies return along the client-sealed loop without revealing client identity.",
    },
    ArchitectureLayer {
        index: "05",
        label: "overlay",
        role: "routes peer messages.",
        title: "Chord DHT routing",
        summary: "Successor and finger tables route DID-addressed messages with stabilization and network isolation.",
        detail: "The overlay maps DID identifiers into a Chord ring. Stabilization keeps successor context current, while finger links reduce lookup distance and keep routing fully decentralized. The overlay handles peer discovery, message forwarding, replicated storage, and E2E stream encryption so applications see a unified DID-addressed network.",
        surface: "Chord identifiers, successor tables, finger routing, network_id isolation",
        contract: "Routing selects peer paths by cryptographic identifier space with end-to-end origin authentication.",
    },
    ArchitectureLayer {
        index: "06",
        label: "transport",
        role: "moves data between peers.",
        title: "WebRTC datachannels",
        summary: "Native and browser transports use STUN, ICE, and SDP to establish direct peer connections.",
        detail: "Browser and native peers share the same transport shape. WebRTC handles NAT traversal through ICE and SDP exchange, then carries overlay messages through credit-flow-controlled direct datachannels across four traffic lanes.",
        surface: "browser WebRTC, native WebRTC, STUN, SDP exchange",
        contract: "Transport establishes peer connectivity and credit flow control while overlay and protocol layers decide what is carried.",
    },
    ArchitectureLayer {
        index: "07",
        label: "identity",
        role: "authenticates peers.",
        title: "DID plus selectable signatures",
        summary: "Bridges browser, daemon, and wallet identity workflows across standard signature schemes.",
        detail: "Identity is represented as DID-addressable cryptographic material with short-lived session delegations. The implementation supports multiple signature families so browser wallets, native daemons, and embedded apps share a unified addressing model across WebCrypto, Ethereum, Solana, and native keys.",
        surface: "DID documents, wallet account selection, secp256k1, secp256r1, ed25519, BLS, bip137",
        contract: "Peers authenticate as DIDs through delegated session keys across all supported wallet and key backends.",
    },
];

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
