//! Guide page: the short, link-first orientation for each way to run Rings.
//!
//! The documentation book (`docs/`, published under `rings.rs/docs`) is the authority; this
//! page is its abridgement (`guide ⊑ book`): one card per runtime — native binary, browser
//! (Wasm) node, browser extension, C FFI — the first commands for each, and a pointer to the
//! chapter that carries the full treatment. Anything that needs more than a screen belongs in
//! the book, not here.

use yew::prelude::*;

use crate::controls::docs_url;
use crate::controls::external_anchor;
use crate::controls::ProjectLink;
use crate::controls::ShellPage;

pub(crate) fn page(navigate_page: Callback<ShellPage>) -> Html {
    html! {
        <section class="site-page guide-page" aria-labelledby="guide-title">
            { heading_section() }
            { runtimes_section(&navigate_page) }
            { first_steps_section() }
            { further_reading_section(&navigate_page) }
        </section>
    }
}

fn heading_section() -> Html {
    html! {
        <header class="guide-heading">
            <p class="landing-kicker">{ "Guide" }</p>
            <h2 id="guide-title">{ "Run a Rings node your way." }</h2>
            <p class="landing-lede">
                { "Pick a runtime, run its first commands, then follow the linked chapter. The documentation is the full reference; this page is the short route into it." }
            </p>
        </header>
    }
}

fn runtimes_section(navigate_page: &Callback<ShellPage>) -> Html {
    html! {
        <section class="landing-section" aria-labelledby="guide-runtimes-title">
            <div class="landing-section-heading">
                <p>{ "Runtimes" }</p>
                <h2 id="guide-runtimes-title">{ "One node, four ways to run it." }</h2>
            </div>
            <div class="guide-runtime-grid">
                { for RUNTIMES.iter().map(|runtime| runtime.render(navigate_page)) }
            </div>
        </section>
    }
}

fn first_steps_section() -> Html {
    html! {
        <section class="landing-section" aria-labelledby="guide-steps-title">
            <div class="landing-section-heading">
                <p>{ "First steps" }</p>
                <h2 id="guide-steps-title">{ "From install to the first message." }</h2>
                <p class="landing-section-lede">
                    { "Each step is complete on its own; the chapter it links to explains the options behind every command." }
                </p>
            </div>
            <div class="guide-step-grid">
                { for STEPS.iter().map(Step::render) }
            </div>
        </section>
    }
}

fn further_reading_section(navigate_page: &Callback<ShellPage>) -> Html {
    html! {
        <section class="landing-section" aria-labelledby="guide-reading-title">
            <div class="landing-section-heading">
                <p>{ "Go deeper" }</p>
                <h2 id="guide-reading-title">{ "The reference, the model, and the paper." }</h2>
            </div>
            <div class="landing-actions" aria-label="Further reading">
                { for FURTHER_READING.iter().map(|link| link.render("landing-secondary-action", navigate_page)) }
            </div>
        </section>
    }
}

/// Chapters of the book the guide condenses, addressed by their path inside the book.
#[derive(Clone, Copy)]
enum Chapter {
    InstallNativeNode,
    HostNativeNode,
    Cli,
    BuildForWasm,
    BrowserNode,
    Ffi,
    JsonRpc,
    ConfigYaml,
    Architecture,
    ForAgents,
}

impl Chapter {
    fn label(self) -> &'static str {
        match self {
            Self::InstallNativeNode => "Install",
            Self::HostNativeNode => "Host a node",
            Self::Cli => "CLI operations",
            Self::BuildForWasm => "Build for Wasm",
            Self::BrowserNode => "Browser node & extension",
            Self::Ffi => "Embed via FFI",
            Self::JsonRpc => "JSON-RPC API",
            Self::ConfigYaml => "config.yaml",
            Self::Architecture => "Architecture",
            Self::ForAgents => "For AI agents",
        }
    }

    fn href(self) -> &'static str {
        match self {
            Self::InstallNativeNode => docs_url!("install-a-native-node.html"),
            Self::HostNativeNode => docs_url!("host-a-native-node.html"),
            Self::Cli => docs_url!("cli.html"),
            Self::BuildForWasm => docs_url!("build-for-wasm.html"),
            Self::BrowserNode => docs_url!("browser-node.html"),
            Self::Ffi => docs_url!("ffi.html"),
            Self::JsonRpc => docs_url!("jsonrpc.html"),
            Self::ConfigYaml => docs_url!("advanced-topic/config.yaml.html"),
            Self::Architecture => docs_url!("advanced-topic/architecture.html"),
            Self::ForAgents => docs_url!("llms.html"),
        }
    }

    fn anchor(self, class: &'static str) -> Html {
        external_anchor(class, self.href(), self.label())
    }
}

/// A destination a guide card or row points at: a chapter of the book, a project resource,
/// or a page of this shell. The three are one sum type so a card's links are one list.
#[derive(Clone, Copy)]
enum GuideLink {
    Chapter(Chapter),
    Project(ProjectLink),
    Page(ShellPage),
}

impl GuideLink {
    fn render(self, class: &'static str, navigate_page: &Callback<ShellPage>) -> Html {
        match self {
            Self::Chapter(chapter) => chapter.anchor(class),
            Self::Project(link) => link.anchor(class),
            Self::Page(page) => page.button(class, false, navigate_page),
        }
    }
}

/// One way to run a node: what it is, and where to install, run, and read about it.
struct Runtime {
    label: &'static str,
    title: &'static str,
    body: &'static str,
    links: &'static [GuideLink],
}

impl Runtime {
    fn render(&self, navigate_page: &Callback<ShellPage>) -> Html {
        html! {
            <article class="guide-card">
                <p class="guide-card-label">{ self.label }</p>
                <h3>{ self.title }</h3>
                <p>{ self.body }</p>
                <div class="guide-card-links">
                    { for self.links.iter().map(|link| link.render("guide-card-link", navigate_page)) }
                </div>
            </article>
        }
    }
}

const RUNTIMES: [Runtime; 4] = [
    Runtime {
        label: "Native",
        title: "Native node",
        body: "The rings daemon joins the overlay from a terminal, serves a JSON-RPC API on the loopback interface, and can seed other peers. Install it from crates.io or a prebuilt release for macOS and Linux.",
        links: &[
            GuideLink::Chapter(Chapter::InstallNativeNode),
            GuideLink::Chapter(Chapter::HostNativeNode),
            GuideLink::Project(ProjectLink::Releases),
        ],
    },
    Runtime {
        label: "Browser",
        title: "Wasm node",
        body: "The same node compiled to WebAssembly. Run it in the hosted console on this site, or embed the npm package in your own page and drive it from JavaScript.",
        links: &[
            GuideLink::Page(ShellPage::Console),
            GuideLink::Chapter(Chapter::BuildForWasm),
            GuideLink::Project(ProjectLink::Npm),
        ],
    },
    Runtime {
        label: "Extension",
        title: "Browser extension",
        body: "The frontend packaged as a Chrome MV3 extension: a side panel over a retained node, a wallet bridge for MetaMask and Phantom, and the Onion WebView.",
        links: &[
            GuideLink::Chapter(Chapter::BrowserNode),
            GuideLink::Project(ProjectLink::Repository),
        ],
    },
    Runtime {
        label: "FFI",
        title: "C FFI",
        body: "Drive a node from Python, Swift, Kotlin, or any host with a C ABI through rings.h: create a provider with a signer callback, listen, and issue the same JSON-RPC methods.",
        links: &[
            GuideLink::Chapter(Chapter::Ffi),
            GuideLink::Chapter(Chapter::JsonRpc),
        ],
    },
];

/// A first step: its commands and the chapter that explains them.
struct Step {
    index: &'static str,
    title: &'static str,
    summary: &'static str,
    code: &'static str,
    chapter: Chapter,
}

impl Step {
    fn render(&self) -> Html {
        html! {
            <article class="guide-step">
                <div class="guide-step-copy">
                    <div class="guide-step-heading">
                        <span class="guide-step-index">{ self.index }</span>
                        <h3>{ self.title }</h3>
                    </div>
                    <p>{ self.summary }</p>
                    { self.chapter.anchor("guide-card-link") }
                </div>
                <pre class="landing-code"><code>{ self.code }</code></pre>
            </article>
        }
    }
}

const STEPS: [Step; 3] = [
    Step {
        index: "01",
        title: "Run a native node",
        summary: "Install the CLI, write the default configuration, and start the daemon. Then join the overlay through a seed node and list the peers it found.",
        code: "cargo install rings-node\nrings init   # writes ~/.rings/config.yaml\nrings run    # foreground; JSON-RPC on 127.0.0.1:50000\n\n# in a second terminal\nrings connect node https://node.rings.rs\nrings peer list",
        chapter: Chapter::Cli,
    },
    Step {
        index: "02",
        title: "Start a browser node",
        summary: "Load the npm package, construct a provider from your account and a signer, start listening, and join through a seed node's HTTP endpoint.",
        code: "import init, { Provider } from \"@ringsnetwork/rings-node\";\n\nawait init();\nconst provider = await new Provider(\n  1, \"stun://stun.l.google.com:19302\", 15,\n  account, \"eip191\", signer,\n);\nprovider.listen();\nawait provider.connect_peer_via_http(\"https://node.rings.rs\");",
        chapter: Chapter::BuildForWasm,
    },
    Step {
        index: "03",
        title: "Talk to peers",
        summary: "Send a namespaced message to a peer, publish a service under a name so other peers can find you, and inspect the routing tables.",
        code: "rings send message <peer-did> chat \"hello\"\nrings service register web\nrings service lookup web\nrings inspect",
        chapter: Chapter::Cli,
    },
];

const FURTHER_READING: [GuideLink; 6] = [
    GuideLink::Chapter(Chapter::JsonRpc),
    GuideLink::Chapter(Chapter::ConfigYaml),
    GuideLink::Chapter(Chapter::Architecture),
    GuideLink::Project(ProjectLink::Security),
    GuideLink::Project(ProjectLink::Whitepaper),
    GuideLink::Chapter(Chapter::ForAgents),
];
