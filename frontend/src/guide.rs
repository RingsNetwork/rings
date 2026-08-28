//! Landing guide page for the browser frontend.

use yew::prelude::*;

use crate::controls::ShellPage;

pub(crate) fn page(
    navigate_page: Callback<ShellPage>,
    active_architecture_layer: UseStateHandle<usize>,
) -> Html {
    let open_console = {
        let navigate_page = navigate_page.clone();
        Callback::from(move |_| navigate_page.emit(ShellPage::Console))
    };
    html! {
        <section class="guide-page" aria-labelledby="guide-title">
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
        <section class="landing-hero" aria-labelledby="guide-title">
            <div class="landing-hero-copy">
                <p class="landing-kicker">{ "Rings Network" }</p>
                <h2 id="guide-title">{ "A P2P network for the sovereign age." }</h2>
                <p class="landing-lede">
                    { "Rings is a browser-native, structured peer-to-peer network for applications that need their own network layer instead of a server-owned data path. Browser tabs and native daemons can join the same overlay, discover peers by DID, and exchange messages over direct WebRTC datachannels routed by a Chord DHT." }
                </p>
                <div class="landing-actions" aria-label="Primary actions">
                    <button class="landing-primary-action" type="button" onclick={open_console}>
                        { "Open Node" }
                    </button>
                    <a
                        class="landing-secondary-action"
                        href="https://github.com/RingsNetwork/rings"
                        target="_blank"
                        rel="noreferrer"
                    >
                        { "GitHub" }
                    </a>
                    <a
                        class="landing-secondary-action"
                        href="https://github.com/RingsNetwork/rings/blob/master/papers/rings.pdf"
                        target="_blank"
                        rel="noreferrer"
                    >
                        { "Whitepaper" }
                    </a>
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
                    { "Rings maps applications, protocols, extension runtime, overlay routing, transport, and identity directly to repository crates and modules. Select a layer to inspect its role." }
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
                    { "The README's extension model is the landing page's developer contract: register a protocol, bind its interpreter, then route inbound envelopes by namespace." }
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
                { landing_link_card("native", "Start here for a minimal native node. It shows wallet setup, node bootstrapping, and registration of a custom namespaced protocol without browser-specific APIs.", "https://github.com/RingsNetwork/rings/tree/master/examples/native") }
                { landing_link_card("relay", "Open TCP and UDP tunnels through the overlay. This example is the practical path for exposing a peer service and carrying traffic without a public server hop.", "https://github.com/RingsNetwork/rings/tree/master/examples/relay") }
                { landing_link_card("dweb", "Explore the decentralized-web application shape. It demonstrates how application content can be addressed through Rings instead of relying on a conventional hosted backend.", "https://github.com/RingsNetwork/rings/tree/master/examples/dweb") }
                { landing_link_card("ffi", "Drive a Rings node from another runtime through the C FFI. This is the integration point for embedding Rings into hosts that cannot call the Rust API directly.", "https://github.com/RingsNetwork/rings/tree/master/examples/ffi") }
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
                    { "Wallet login, SDP/HTTP connectivity, topology inspection, onion proxy requests, and custom messages live here." }
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

const ARCHITECTURE_LAYERS: [ArchitectureLayer; 6] = [
    ArchitectureLayer {
        index: "01",
        label: "applications",
        role: "runs user-facing workflows.",
        title: "dWeb, relay, and custom apps",
        summary: "Apps run over the protocol layer instead of a hosted backend data path.",
        detail: "Application surfaces are repository examples and browser node panels. They compose wallet login, dWeb content, relay tunnels, and custom protocol messages on top of the same peer runtime. The application layer should read as product-facing behavior: it chooses what to ask the network to do, while the lower layers keep addressing, routing, and transport concerns out of the UI code.",
        surface: "frontend Node page, examples/dweb, examples/relay",
        contract: "Application code addresses peers and namespaces; it does not own overlay routing or transport setup.",
    },
    ArchitectureLayer {
        index: "02",
        label: "protocols",
        role: "defines namespaced behavior.",
        title: "relay, echo, and user namespaces",
        summary: "Built-ins cover TCP/UDP relay and echo; user protocols are addressed by namespace.",
        detail: "Protocols are registered behind stable namespaces. Built-in protocols cover relay and echo flows, while external applications can install their own protocol state machines without changing the overlay. This layer is the extension boundary: new behavior is added by registering a protocol and its interpreter, not by branching the node or adding a new transport path.",
        surface: "protocol registry, relay handles, echo protocol, custom namespaces",
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
