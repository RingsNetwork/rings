use super::border;
use super::rule;
use super::Theme;
use super::SERIF_FONT;

/// The site footer: a dark band under the warm document pages, with the brand, the link
/// columns, the standing declarations, and the copyright line. Its page buttons and anchors
/// share one class so the two render identically (`.site-footer-link`).
pub(super) fn append(css: &mut String, theme: Theme) {
    append_band(css, theme);
    append_brand(css);
    append_columns(css);
    append_notices(css);
    append_bottom(css);
}

/// Appends outer footer container and column grid rules.
fn append_band(css: &mut String, theme: Theme) {
    rule(css, ".site-footer", &[
        ("display", "grid"),
        ("gap", "36px"),
        ("margin-top", "24px"),
        ("padding", "56px 10% 36px"),
        ("border-top", border(theme.line)),
        ("background", "#101828"),
        ("color", "#d0d5dd"),
        ("font-family", SERIF_FONT),
    ]);
    rule(css, ".site-footer-grid", &[
        ("display", "grid"),
        (
            "grid-template-columns",
            "minmax(0, 1.5fr) repeat(3, minmax(0, 1fr))",
        ),
        ("gap", "32px"),
    ]);
}

/// Appends brand mark, title, and tagline rules in the footer.
fn append_brand(css: &mut String) {
    rule(css, ".site-footer-brand", &[
        ("display", "grid"),
        ("max-width", "380px"),
        ("gap", "10px"),
        ("align-content", "start"),
    ]);
    rule(css, ".site-footer-logo", &[
        ("display", "block"),
        ("width", "38px"),
        ("height", "38px"),
        ("padding", "7px"),
        ("border-radius", "8px"),
        ("background", "#1f2937"),
        ("object-fit", "contain"),
    ]);
    rule(css, ".site-footer-brand strong", &[
        ("color", "#fff"),
        ("font-size", "1.04rem"),
        ("font-weight", "700"),
    ]);
    rule(css, ".site-footer-brand p", &[
        ("margin", "0"),
        ("color", "#98a2b3"),
        ("font-size", "0.9rem"),
        ("line-height", "1.64"),
    ]);
}

/// Appends footer link column and `.site-footer-link` rules.
fn append_columns(css: &mut String) {
    rule(css, ".site-footer-column", &[
        ("display", "grid"),
        ("gap", "12px"),
        ("align-content", "start"),
    ]);
    rule(css, ".site-footer-column h3", &[
        ("margin", "0"),
        ("color", "#fff"),
        ("font-size", "0.75rem"),
        ("font-weight", "700"),
        ("letter-spacing", "0.08em"),
        ("text-transform", "uppercase"),
    ]);
    rule(css, ".site-footer-column ul", &[
        ("display", "grid"),
        ("gap", "8px"),
        ("margin", "0"),
        ("padding", "0"),
        ("list-style", "none"),
    ]);
    rule(css, ".site-footer-link", &[
        ("display", "inline"),
        ("min-height", "0"),
        ("margin", "0"),
        ("padding", "0"),
        ("border", "0"),
        ("background", "none"),
        ("color", "#d0d5dd"),
        ("font-family", "inherit"),
        ("font-size", "0.92rem"),
        ("font-weight", "400"),
        ("line-height", "1.45"),
        ("text-align", "left"),
        ("text-decoration", "none"),
        ("text-transform", "none"),
        ("box-shadow", "none"),
        ("cursor", "pointer"),
    ]);
    rule(css, ".site-footer-link:hover", &[
        ("border", "0"),
        ("background", "none"),
        ("color", "#fff"),
        ("text-decoration", "underline"),
    ]);
}

/// Appends standing declaration list rules in the footer.
fn append_notices(css: &mut String) {
    rule(css, ".site-footer-notices", &[
        ("display", "grid"),
        (
            "grid-template-columns",
            "repeat(auto-fit, minmax(180px, 1fr))",
        ),
        ("gap", "20px 28px"),
        ("margin", "0"),
        ("padding-top", "28px"),
        ("border-top", "1px solid rgba(255, 255, 255, 0.1)"),
    ]);
    rule(css, ".site-footer-notice", &[
        ("display", "grid"),
        ("gap", "6px"),
    ]);
    rule(css, ".site-footer-notice dt", &[
        ("color", "#fff"),
        ("font-size", "0.76rem"),
        ("font-weight", "700"),
        ("letter-spacing", "0.06em"),
        ("text-transform", "uppercase"),
    ]);
    rule(css, ".site-footer-notice dd", &[
        ("margin", "0"),
        ("color", "#98a2b3"),
        ("font-size", "0.84rem"),
        ("line-height", "1.62"),
    ]);
}

/// Appends bottom copyright bar rules for the footer.
fn append_bottom(css: &mut String) {
    rule(css, ".site-footer-bottom", &[
        ("display", "flex"),
        ("flex-wrap", "wrap"),
        ("gap", "8px 24px"),
        ("justify-content", "space-between"),
        ("padding-top", "20px"),
        ("border-top", "1px solid rgba(255, 255, 255, 0.1)"),
        ("color", "#667085"),
        ("font-size", "0.8rem"),
    ]);
}
