//! Destinations outside the shell: project resources and chapters of the documentation book.
//!
//! The set of destinations is defined once here; the landing hero, the header, the guide, and
//! the footer each present a subset in their own class (`anchor : Link × Class → Html`).
//! Every URL is absolute because the same shell runs from `rings.rs` and from an extension
//! origin, where a site-relative `/docs/` would resolve inside the extension.

use yew::prelude::*;

use crate::controls::ShellPage;

/// Absolute URL of a chapter of the documentation book, from its path inside the book.
macro_rules! docs_url {
    ($chapter:literal) => {
        concat!("https://rings.rs/docs/", $chapter)
    };
}

/// Project resources outside the book.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ProjectLink {
    /// Engineering blog on `rings.rs/blog/`.
    Blog,
    /// Documentation book root on `rings.rs/docs/`.
    Docs,
    /// GitHub repository for Rings Network.
    Repository,
    /// Rings Network whitepaper PDF.
    Whitepaper,
    /// GitHub releases page.
    Releases,
    /// `rings-node` package page on crates.io.
    CratesIo,
    /// `@ringsnetwork/rings-node` package page on npm.
    Npm,
    /// Security model and policy document.
    Security,
    /// Project roadmap document.
    Roadmap,
    /// GitHub Sponsors page for Rings Network.
    Sponsor,
    /// AGPL-3.0-only license text.
    License,
    /// Machine-readable `llms.txt` index on `rings.rs`.
    LlmsTxt,
    /// Official Rings Network X (Twitter) account.
    XAccount,
}

impl ProjectLink {
    /// The links the landing hero presents beside its call to action.
    pub(crate) const HERO: [Self; 4] = [Self::Docs, Self::Blog, Self::Repository, Self::Whitepaper];

    /// The links the header presents after the page buttons. The repository and the node
    /// console are entered from the landing page, so the header carries only the external
    /// destinations the landing page does not lead to on its own.
    pub(crate) const HEADER: [Self; 3] = [Self::Blog, Self::Docs, Self::Whitepaper];

    /// Display text rendered inside the anchor for this resource.
    fn label(self) -> &'static str {
        match self {
            Self::Blog => "Blog",
            Self::Docs => "Docs",
            Self::Repository => "GitHub",
            Self::Whitepaper => "Whitepaper",
            Self::Releases => "Releases",
            Self::CratesIo => "crates.io",
            Self::Npm => "npm",
            Self::Security => "Security model",
            Self::Roadmap => "Roadmap",
            Self::Sponsor => "Sponsor",
            Self::License => "License",
            Self::LlmsTxt => "llms.txt",
            Self::XAccount => "X (Twitter)",
        }
    }

    /// Absolute URL of this project resource.
    pub(crate) fn href(self) -> &'static str {
        match self {
            Self::Blog => "https://rings.rs/blog/",
            Self::Docs => docs_url!(""),
            Self::Repository => "https://github.com/RingsNetwork/rings",
            Self::Whitepaper => "https://rings.rs/papers/",
            Self::Releases => "https://github.com/RingsNetwork/rings/releases",
            Self::CratesIo => "https://crates.io/crates/rings-node",
            Self::Npm => "https://www.npmjs.com/package/@ringsnetwork/rings-node",
            Self::Security => "https://github.com/RingsNetwork/rings/blob/master/SECURITY.md",
            Self::Roadmap => "https://github.com/RingsNetwork/rings/blob/master/ROADMAP.md",
            Self::Sponsor => "https://github.com/sponsors/RingsNetwork",
            Self::License => "https://github.com/RingsNetwork/rings/blob/master/LICENSE",
            Self::LlmsTxt => "https://rings.rs/llms.txt",
            Self::XAccount => "https://x.com/RingsNetworkio",
        }
    }

    /// The link as an anchor of the given class.
    pub(crate) fn anchor(self, class: &'static str) -> Html {
        external_anchor(class, self.href(), self.label())
    }
}

/// Chapters of the documentation book the shell links into, addressed by their path inside
/// the book. The book is the authority for what the guide condenses (`guide ⊑ book`), so a
/// chapter is a destination in its own right.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Chapter {
    /// Installing the native `rings` daemon.
    InstallNativeNode,
    /// Hosting and operating a native node.
    HostNativeNode,
    /// Command-line interface reference.
    Cli,
    /// `rings-tui` terminal dashboard guide.
    Tui,
    /// Operator metrics, logs, and observability guide.
    OperatorObservability,
    /// Native TUN gateway setup and operation.
    NativeGateway,
    /// Authoritative DNS relay and IPv6 ingress guide.
    DnsRelay,
    /// Stateless TURN and Onion TURN server guide.
    TurnServer,
    /// DID mailbox and stateless SMTP gateway guide.
    MailGateway,
    /// Building and embedding the WebAssembly node.
    BuildForWasm,
    /// Browser node console and Chrome extension guide.
    BrowserNode,
    /// Embedding a Rings node via the C FFI (`rings.h`).
    Ffi,
    /// JSON-RPC API reference.
    JsonRpc,
    /// `config.yaml` configuration reference.
    ConfigYaml,
    /// System architecture overview.
    Architecture,
    /// Sphinx/AEZ onion routing and Kleisli invoke pipelines.
    Onion,
    /// Binary wire protocol specification.
    WireProtocol,
    /// Decoupled transport abstraction guide.
    Transport,
    /// Documentation index tailored for AI agents.
    ForAgents,
}

impl Chapter {
    /// Display text rendered inside the anchor for this chapter.
    fn label(self) -> &'static str {
        match self {
            Self::InstallNativeNode => "Install",
            Self::HostNativeNode => "Host a node",
            Self::Cli => "CLI operations",
            Self::Tui => "Terminal UI (TUI)",
            Self::OperatorObservability => "Observability",
            Self::NativeGateway => "Native TUN gateway",
            Self::DnsRelay => "DNS relay",
            Self::TurnServer => "TURN & Onion TURN",
            Self::MailGateway => "Mail gateway",
            Self::BuildForWasm => "Build for Wasm",
            Self::BrowserNode => "Browser node & extension",
            Self::Ffi => "Embed via FFI",
            Self::JsonRpc => "JSON-RPC API",
            Self::ConfigYaml => "config.yaml",
            Self::Architecture => "Architecture",
            Self::Onion => "Onion & Kleisli pipelines",
            Self::WireProtocol => "Wire protocol",
            Self::Transport => "Transport abstraction",
            Self::ForAgents => "For AI agents",
        }
    }

    /// Absolute URL of this documentation book chapter.
    pub(crate) fn href(self) -> &'static str {
        match self {
            Self::InstallNativeNode => docs_url!("install-a-native-node.html"),
            Self::HostNativeNode => docs_url!("host-a-native-node.html"),
            Self::Cli => docs_url!("cli.html"),
            Self::Tui => docs_url!("tui.html"),
            Self::OperatorObservability => docs_url!("operator-observability.html"),
            Self::NativeGateway => docs_url!("native-gateway.html"),
            Self::DnsRelay => docs_url!("dns-relay.html"),
            Self::TurnServer => docs_url!("turn-server.html"),
            Self::MailGateway => docs_url!("mail-gateway.html"),
            Self::BuildForWasm => docs_url!("build-for-wasm.html"),
            Self::BrowserNode => docs_url!("browser-node.html"),
            Self::Ffi => docs_url!("ffi.html"),
            Self::JsonRpc => docs_url!("jsonrpc.html"),
            Self::ConfigYaml => docs_url!("advanced-topic/config.yaml.html"),
            Self::Architecture => docs_url!("advanced-topic/architecture.html"),
            Self::Onion => docs_url!("onion.html"),
            Self::WireProtocol => docs_url!("advanced-topic/wire-protocol.html"),
            Self::Transport => docs_url!("advanced-topic/transport.html"),
            Self::ForAgents => docs_url!("llms.html"),
        }
    }

    /// The chapter as an anchor of the given class.
    pub(crate) fn anchor(self, class: &'static str) -> Html {
        external_anchor(class, self.href(), self.label())
    }
}

/// Any destination a surface lists: a page of this shell, a project resource, or a chapter.
/// The three are one sum type so a surface's links are one list, rendered in one class
/// (`render : SiteLink × Class → Html`); pages navigate in place, the rest open outside.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum SiteLink {
    /// An in-shell page navigated via `ShellPage`.
    Page(ShellPage),
    /// An external project resource.
    Project(ProjectLink),
    /// A chapter of the documentation book.
    Chapter(Chapter),
}

impl SiteLink {
    /// Render this link as an in-shell button or external anchor with the given CSS `class`.
    pub(crate) fn render(self, class: &'static str, navigate_page: &Callback<ShellPage>) -> Html {
        match self {
            Self::Page(page) => page.button(class, false, navigate_page),
            Self::Project(link) => link.anchor(class),
            Self::Chapter(chapter) => chapter.anchor(class),
        }
    }
}

/// An anchor to a destination outside the shell, opened in a new tab without a referrer.
fn external_anchor(class: &'static str, href: &'static str, label: &'static str) -> Html {
    html! {
        <a class={class} href={href} target="_blank" rel="noreferrer">
            { label }
        </a>
    }
}
