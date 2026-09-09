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
    Docs,
    Repository,
    Whitepaper,
    Releases,
    CratesIo,
    Npm,
    Security,
    Roadmap,
    Sponsor,
    License,
    LlmsTxt,
}

impl ProjectLink {
    /// The links the landing hero presents beside its call to action.
    pub(crate) const HERO: [Self; 3] = [Self::Docs, Self::Repository, Self::Whitepaper];

    /// The links the header presents after the page buttons. The repository and the node
    /// console are entered from the landing page, so the header carries only the two
    /// destinations the landing page does not lead to on its own.
    pub(crate) const HEADER: [Self; 2] = [Self::Docs, Self::Whitepaper];

    fn label(self) -> &'static str {
        match self {
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
        }
    }

    pub(crate) fn href(self) -> &'static str {
        match self {
            Self::Docs => docs_url!(""),
            Self::Repository => "https://github.com/RingsNetwork/rings",
            Self::Whitepaper => {
                "https://github.com/RingsNetwork/rings/blob/master/papers/rings.pdf"
            }
            Self::Releases => "https://github.com/RingsNetwork/rings/releases",
            Self::CratesIo => "https://crates.io/crates/rings-node",
            Self::Npm => "https://www.npmjs.com/package/@ringsnetwork/rings-node",
            Self::Security => "https://github.com/RingsNetwork/rings/blob/master/SECURITY.md",
            Self::Roadmap => "https://github.com/RingsNetwork/rings/blob/master/ROADMAP.md",
            Self::Sponsor => "https://github.com/sponsors/RingsNetwork",
            Self::License => "https://github.com/RingsNetwork/rings/blob/master/LICENSE",
            Self::LlmsTxt => "https://rings.rs/llms.txt",
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

    pub(crate) fn href(self) -> &'static str {
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
    Page(ShellPage),
    Project(ProjectLink),
    Chapter(Chapter),
}

impl SiteLink {
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
