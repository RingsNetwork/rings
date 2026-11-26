//! Site footer for the document pages (landing and guide).
//!
//! Brand, three link columns, the project's standing declarations, and the copyright line. The
//! console is an application screen rather than a document and does not carry it, and below
//! the mobile breakpoint the fixed bottom navigation bar owns the same space, so the stylesheet
//! hides it there (`styles::theme::responsive`).
//!
//! The documentation book renders the same footer from `docs/theme/index.hbs`, and the
//! engineering blog renders it from `frontend/scripts/build_blog.py`. The three copies are
//! written in different languages, so the test at the bottom of this module holds them equal.

use yew::prelude::*;

use crate::controls::ShellPage;
use crate::links::ProjectLink;
use crate::links::SiteLink;

/// Render the site footer with brand statement, link columns, legal notices, and copyright line.
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
                    <p>{ BRAND_STATEMENT }</p>
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

/// Short project description shown beneath the brand wordmark in the footer.
const BRAND_STATEMENT: &str = "A peer-to-peer network for the sovereign age. Browser tabs and native daemons join one overlay, find each other by DID, and talk over direct WebRTC datachannels routed by a Chord DHT.";

/// A footer column: a heading over its destinations.
struct Column {
    /// Column heading label.
    title: &'static str,
    /// Ordered list of links rendered under this column.
    links: &'static [SiteLink],
}

impl Column {
    /// Render this footer column as a labelled `<nav>` list.
    fn render(&self, navigate_page: &Callback<ShellPage>) -> Html {
        html! {
            <nav class="site-footer-column" aria-label={self.title}>
                <h3>{ self.title }</h3>
                <ul>
                    { for self.links.iter().map(|link| html! {
                        <li>{ link.render("site-footer-link", navigate_page) }</li>
                    }) }
                </ul>
            </nav>
        }
    }
}

/// The three navigation columns rendered in the site footer.
const COLUMNS: [Column; 3] = [
    Column {
        title: "Product",
        links: &[
            SiteLink::Page(ShellPage::Home),
            SiteLink::Page(ShellPage::Guide),
            SiteLink::Page(ShellPage::Console),
        ],
    },
    Column {
        title: "Developers",
        links: &[
            SiteLink::Project(ProjectLink::Docs),
            SiteLink::Project(ProjectLink::Repository),
            SiteLink::Project(ProjectLink::Releases),
            SiteLink::Project(ProjectLink::CratesIo),
            SiteLink::Project(ProjectLink::Npm),
            SiteLink::Project(ProjectLink::LlmsTxt),
        ],
    },
    Column {
        title: "Project",
        links: &[
            SiteLink::Project(ProjectLink::Blog),
            SiteLink::Project(ProjectLink::Whitepaper),
            SiteLink::Project(ProjectLink::Security),
            SiteLink::Project(ProjectLink::Roadmap),
            SiteLink::Project(ProjectLink::Sponsor),
            SiteLink::Project(ProjectLink::XAccount),
            SiteLink::Project(ProjectLink::License),
        ],
    },
];

/// A standing declaration: its heading and its statement.
struct Notice {
    /// Short heading for the declaration.
    title: &'static str,
    /// Explanatory body text for the declaration.
    body: &'static str,
}

impl Notice {
    /// Render this declaration as a definition-list entry.
    fn render(&self) -> Html {
        html! {
            <div class="site-footer-notice">
                <dt>{ self.title }</dt>
                <dd>{ self.body }</dd>
            </div>
        }
    }
}

/// Standing legal, security, and privacy notices displayed in the site footer.
const NOTICES: [Notice; 5] = [
    Notice {
        title: "License",
        body: "The Rings source code is released under the GNU Affero General Public License, version 3.0 only (AGPL-3.0-only). Works derived from it, including services that offer it over a network, must be released under the same license.",
    },
    Notice {
        title: "Commercial use",
        body: "Under AGPL-3.0-only, commercial use is permitted when its copyleft and network source-disclosure obligations are met. Commercial licensing outside AGPL-3.0-only terms is available from Rings Network on request.",
    },
    Notice {
        title: "No warranty",
        body: "The software and this site are provided as is, without warranty of any kind, express or implied, as set out in sections 15 and 16 of the AGPL.",
    },
    Notice {
        title: "Security boundary",
        body: "Rings combines wallet-backed session delegation, end-to-end encrypted peer-to-peer channels, and multi-hop Sphinx onion routing. Review the security model for cryptographic guarantees and deployment best practices.",
    },
    Notice {
        title: "Privacy",
        body: "rings.rs is a static site: no accounts, no analytics, no tracking cookies. Browser-node keys, stores, and settings are kept locally in your browser's IndexedDB and local storage.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The book's page template, which carries this footer in Handlebars.
    const BOOK_TEMPLATE: &str = include_str!("../../docs/theme/index.hbs");

    /// The blog renderer script, which carries this footer in HTML template strings.
    const BLOG_SCRIPT: &str = include_str!("../scripts/build_blog.py");

    fn assert_footer_parity(template: &str, name: &str) {
        assert!(
            template.contains(BRAND_STATEMENT),
            "the {name} footer lacks the brand statement"
        );
        for notice in NOTICES.iter() {
            assert!(
                template.contains(notice.title) && template.contains(notice.body),
                "the {name} footer lacks the {} notice",
                notice.title
            );
        }
        for column in COLUMNS.iter() {
            assert!(
                template.contains(column.title),
                "the {name} footer lacks the {} column",
                column.title
            );
            for link in column.links.iter() {
                let href = match link {
                    SiteLink::Project(project) => project.href().to_string(),
                    SiteLink::Chapter(chapter) => chapter.href().to_string(),
                    // A page is reached from static sub-sites through the site's hash route, the
                    // landing page through the site root.
                    SiteLink::Page(ShellPage::Home) => "https://rings.rs/\"".to_string(),
                    SiteLink::Page(page) => format!("https://rings.rs/#{}", page.slug()),
                };
                assert!(template.contains(&href), "the {name} footer lacks {href}");
            }
        }
    }

    /// Law: every statement and every destination of this footer appears verbatim in the
    /// book's footer and the blog's footer, so the three copies cannot drift apart unnoticed.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_book_footer_carries_the_same_statements_and_destinations() {
        assert_footer_parity(BOOK_TEMPLATE, "book");
        assert_footer_parity(BLOG_SCRIPT, "blog");
    }
}
