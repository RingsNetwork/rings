//! Guide page: the short, link-first orientation for each way to run Rings.
//!
//! The documentation book (`docs/`, published under `rings.rs/docs`) is the authority; this
//! page is its abridgement (`guide ⊑ book`): one card per runtime — native binary, browser
//! (Wasm) node, browser extension, C FFI — the first commands for each, and a pointer to the
//! chapter that carries the full treatment. Anything that needs more than a screen belongs in
//! the book, not here.

use yew::prelude::*;

use crate::controls::ShellPage;
use crate::links::Chapter;
use crate::links::ProjectLink;
use crate::links::SiteLink;
use crate::node::public_seed_endpoint;

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

/// One way to run a node: what it is, and where to install, run, and read about it.
struct Runtime {
    label: &'static str,
    title: &'static str,
    body: &'static str,
    links: &'static [SiteLink],
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
            SiteLink::Chapter(Chapter::InstallNativeNode),
            SiteLink::Chapter(Chapter::HostNativeNode),
            SiteLink::Project(ProjectLink::Releases),
        ],
    },
    Runtime {
        label: "Browser",
        title: "Wasm node",
        body: "The same node compiled to WebAssembly. Run it in the hosted console on this site, or embed the npm package in your own page and drive it from JavaScript.",
        links: &[
            SiteLink::Page(ShellPage::Console),
            SiteLink::Chapter(Chapter::BuildForWasm),
            SiteLink::Project(ProjectLink::Npm),
        ],
    },
    Runtime {
        label: "Extension",
        title: "Browser extension",
        body: "The frontend packaged as a Chrome MV3 extension: a side panel over a retained node, a wallet bridge for MetaMask and Phantom, and the Onion WebView.",
        links: &[
            SiteLink::Chapter(Chapter::BrowserNode),
            SiteLink::Project(ProjectLink::Repository),
        ],
    },
    Runtime {
        label: "FFI",
        title: "C FFI",
        body: "Drive a node from Python, Swift, Kotlin, or any host with a C ABI through rings.h: create a provider with a signer callback, listen, and issue the same JSON-RPC methods.",
        links: &[
            SiteLink::Chapter(Chapter::Ffi),
            SiteLink::Chapter(Chapter::JsonRpc),
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
        code: concat!(
            "cargo install rings-node\n",
            "rings init   # writes ~/.rings/config.yaml\n",
            "rings run    # foreground; JSON-RPC on 127.0.0.1:50000\n",
            "\n",
            "# in a second terminal\n",
            "rings connect node ", public_seed_endpoint!(), "\n",
            "rings peer list",
        ),
        chapter: Chapter::Cli,
    },
    Step {
        index: "02",
        title: "Start a browser node",
        summary: "Load the npm package, construct a provider from your account and a signer, start listening, and join through a seed node's HTTP endpoint.",
        code: concat!(
            "import init, { Provider } from \"@ringsnetwork/rings-node\";\n",
            "\n",
            "await init();\n",
            "const provider = await new Provider(\n",
            "  1, \"stun://stun.l.google.com:19302\", 15,\n",
            "  account, \"eip191\", signer,\n",
            ");\n",
            "provider.listen();\n",
            "await provider.connect_peer_via_http(\"", public_seed_endpoint!(), "\");",
        ),
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

const FURTHER_READING: [SiteLink; 6] = [
    SiteLink::Chapter(Chapter::JsonRpc),
    SiteLink::Chapter(Chapter::ConfigYaml),
    SiteLink::Chapter(Chapter::Architecture),
    SiteLink::Project(ProjectLink::Security),
    SiteLink::Project(ProjectLink::Whitepaper),
    SiteLink::Chapter(Chapter::ForAgents),
];
