use std::fmt::Write as _;

/// Initial string capacity reserved when appending generated theme CSS.
pub(super) const THEME_CSS_CAPACITY: usize = 32_000;

/// Classical serif font stack aligned with the Pacioli-derived Rings geometric mark.
pub(super) const SERIF_FONT: &str = "'Iowan Old Style', 'Palatino Linotype', Palatino, 'Book Antiqua', Baskerville, 'Source Han Serif SC', 'Noto Serif CJK SC', 'Songti SC', Georgia, serif";

/// Monospace font stack reserved strictly for code samples, cryptographic identifiers, and numeric badges.
pub(super) const MONO_FONT: &str =
    "ui-monospace, 'SF Mono', 'SFMono-Regular', Menlo, Consolas, 'Liberation Mono', monospace";

/// Console page theme rules.
mod console;
/// Dialog and modal theme rules.
mod dialogs;
/// Site footer theme rules.
mod footer;
/// Guide and documentation theme rules.
mod guide;
/// Landing page theme rules.
mod landing;
/// Navigation bar theme rules.
mod navigation;
/// Responsive layout override theme rules.
mod responsive;
/// Topology view theme rules.
mod topology;

/// Color palette tokens shared across generated theme stylesheet sections.
#[derive(Clone, Copy)]
struct Theme {
    /// Primary page background color.
    page: &'static str,
    /// Alternate page background color.
    page_alt: &'static str,
    /// Primary panel surface color.
    panel: &'static str,
    /// Alternate panel surface color.
    panel_alt: &'static str,
    /// Standard divider and border color.
    line: &'static str,
    /// Emphasized border color.
    line_strong: &'static str,
    /// Primary foreground text color.
    ink: &'static str,
    /// Secondary foreground text color.
    ink_soft: &'static str,
    /// Muted caption and helper text color.
    muted: &'static str,
    /// Primary accent color.
    accent: &'static str,
    /// Teal semantic highlight color.
    teal: &'static str,
    /// Amber semantic highlight color.
    amber: &'static str,
}

/// Default warm parchment color palette used by the application.
const WARM: Theme = Theme {
    page: "#f3ead8",
    page_alt: "#fbf4e6",
    panel: "#fffaf0",
    panel_alt: "#fff6e6",
    line: "#dfd0b7",
    line_strong: "#d7c6aa",
    ink: "#101828",
    ink_soft: "#344054",
    muted: "#736453",
    accent: "#b42318",
    teal: "#0f766e",
    amber: "#8a5a12",
};

/// Appends all generated theme stylesheet rules to `css`.
pub(super) fn append(css: &mut String) {
    navigation::append(css, WARM);
    landing::append(css, WARM);
    guide::append(css, WARM);
    footer::append(css, WARM);
    console::append(css, WARM);
    topology::append(css, WARM);
    dialogs::append(css, WARM);
    responsive::append(css);
}

/// Appends themed header navigation button and external link rules under `scope`.
fn themed_nav(css: &mut String, scope: &str, theme: Theme) {
    rule(
        css,
        &format!("{scope} .header-nav-button,{scope} .header-external-link"),
        &[
            ("min-height", "34px"),
            ("border-color", theme.line_strong),
            ("border-radius", "8px"),
            ("background", theme.panel),
            ("color", theme.ink_soft),
            ("box-shadow", "none"),
        ],
    );
    rule(
        css,
        &format!("{scope} .header-nav-button.active,{scope} .header-nav-button:hover,{scope} .header-external-link:hover"),
        &[
            ("border-color", "#111827"),
            ("background", "#111827"),
            ("color", "#fff"),
        ],
    );
}

/// Returns a `1px solid` CSS border shorthand for a known theme line color.
fn border(color: &'static str) -> &'static str {
    match color {
        "#dfd0b7" => "1px solid #dfd0b7",
        "#d7c6aa" => "1px solid #d7c6aa",
        _ => "1px solid currentColor",
    }
}

/// Appends a single CSS rule block for `selector` and its property `declarations`.
fn rule(css: &mut String, selector: &str, declarations: &[(&str, &str)]) {
    css.push('\n');
    css.push_str(selector);
    css.push('{');
    for (name, value) in declarations {
        let _ = write!(css, "{name}:{value};");
    }
    css.push('}');
}

/// Appends a CSS `@media` block containing the given selector rules.
fn media(css: &mut String, query: &str, rules: &[(&str, &[(&'static str, &'static str)])]) {
    let _ = write!(css, "\n@media {query}{{");
    for (selector, declarations) in rules {
        rule(css, selector, declarations);
    }
    css.push('}');
}
