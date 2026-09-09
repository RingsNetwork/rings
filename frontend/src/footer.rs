//! Site footer for the document pages (landing and guide).
//!
//! Brand, three link columns, the project's standing declarations, and the copyright line. The
//! console is an application screen rather than a document and does not carry it, and below
//! the mobile breakpoint the fixed bottom navigation bar owns the same space, so the stylesheet
//! hides it there (`styles::theme::responsive`).

use yew::prelude::*;

use crate::controls::ProjectLink;
use crate::controls::ShellPage;

pub(crate) fn site_footer(navigate_page: Callback<ShellPage>) -> Html {
    html! {
        <footer class="site-footer" aria-label="Site footer">
            <div class="site-footer-grid">
                <div class="site-footer-brand">
                    <img
                        class="site-footer-logo"
                        src="assets/icons/rings.svg"
                        alt=""
                        decoding="async"
                    />
                    <strong>{ "Rings Network" }</strong>
                    <p>
                        { "A peer-to-peer network for the sovereign age. Browser tabs and native daemons join one overlay, find each other by DID, and talk over direct WebRTC datachannels routed by a Chord DHT." }
                    </p>
                </div>
                { for COLUMNS.iter().map(|column| column.render(&navigate_page)) }
            </div>
            <dl class="site-footer-notices">
                { for NOTICES.iter().map(Notice::render) }
            </dl>
            <div class="site-footer-bottom">
                <span>{ format!("© 2021–{} Rings Network", current_year()) }</span>
                <span>{ "AGPL-3.0-only · Built with Rust, Yew, and WebAssembly" }</span>
            </div>
        </footer>
    }
}

/// The year the page is rendered in, so the copyright range never goes stale.
fn current_year() -> u32 {
    js_sys::Date::new_0().get_full_year()
}

/// A footer destination: a page of this shell or a project resource.
#[derive(Clone, Copy)]
enum FooterLink {
    Page(ShellPage),
    Project(ProjectLink),
}

impl FooterLink {
    fn render(self, navigate_page: &Callback<ShellPage>) -> Html {
        match self {
            Self::Page(page) => page.button("site-footer-link", false, navigate_page),
            Self::Project(link) => link.anchor("site-footer-link"),
        }
    }
}

/// A footer column: a heading over its destinations.
struct Column {
    title: &'static str,
    links: &'static [FooterLink],
}

impl Column {
    fn render(&self, navigate_page: &Callback<ShellPage>) -> Html {
        html! {
            <nav class="site-footer-column" aria-label={self.title}>
                <h3>{ self.title }</h3>
                <ul>
                    { for self.links.iter().map(|link| html! { <li>{ link.render(navigate_page) }</li> }) }
                </ul>
            </nav>
        }
    }
}

const COLUMNS: [Column; 3] = [
    Column {
        title: "Product",
        links: &[
            FooterLink::Page(ShellPage::Home),
            FooterLink::Page(ShellPage::Guide),
            FooterLink::Page(ShellPage::Console),
        ],
    },
    Column {
        title: "Developers",
        links: &[
            FooterLink::Project(ProjectLink::Docs),
            FooterLink::Project(ProjectLink::Repository),
            FooterLink::Project(ProjectLink::Releases),
            FooterLink::Project(ProjectLink::CratesIo),
            FooterLink::Project(ProjectLink::Npm),
            FooterLink::Project(ProjectLink::LlmsTxt),
        ],
    },
    Column {
        title: "Project",
        links: &[
            FooterLink::Project(ProjectLink::Whitepaper),
            FooterLink::Project(ProjectLink::Security),
            FooterLink::Project(ProjectLink::Roadmap),
            FooterLink::Project(ProjectLink::Sponsor),
            FooterLink::Project(ProjectLink::License),
        ],
    },
];

/// A standing declaration: its heading and its statement.
struct Notice {
    title: &'static str,
    body: &'static str,
}

impl Notice {
    fn render(&self) -> Html {
        html! {
            <div class="site-footer-notice">
                <dt>{ self.title }</dt>
                <dd>{ self.body }</dd>
            </div>
        }
    }
}

const NOTICES: [Notice; 5] = [
    Notice {
        title: "License",
        body: "The Rings source code is released under the GNU Affero General Public License, version 3.0 only (AGPL-3.0-only). Works derived from it, including services that offer it over a network, must be released under the same license.",
    },
    Notice {
        title: "Commercial use",
        body: "Commercial use is permitted only on the AGPL's terms: products and hosted services built on Rings must publish their source. A commercial license for use outside those terms is available from Rings Network on request. The Rings Network name, logo, and the hosted rings.rs service are not covered by the license.",
    },
    Notice {
        title: "No warranty",
        body: "The software and this site are provided as is, without warranty of any kind, express or implied, as set out in sections 15 and 16 of the AGPL.",
    },
    Notice {
        title: "Security boundary",
        body: "DID authentication proves control of a key. It is not Sybil or eclipse resistance for permissionless membership; read the threat model before deploying.",
    },
    Notice {
        title: "Privacy",
        body: "rings.rs is a static site: no accounts, no analytics, no tracking cookies. Browser-node settings are kept in your browser's local storage.",
    },
];
