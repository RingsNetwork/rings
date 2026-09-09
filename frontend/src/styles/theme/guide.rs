use super::border;
use super::rule;
use super::Theme;

/// The guide page: its heading, the runtime cards, and the first-step cards. Cards share the
/// document card surface defined with the landing cards (`landing::append_feature_cards`);
/// only the guide's own parts are styled here.
pub(super) fn append(css: &mut String, theme: Theme) {
    append_heading(css, theme);
    append_runtime_cards(css, theme);
    append_steps(css, theme);
}

fn append_heading(css: &mut String, theme: Theme) {
    rule(css, ".guide-heading", &[
        ("display", "grid"),
        ("max-width", "720px"),
        ("gap", "18px"),
        ("padding", "64px 0 8px"),
    ]);
    rule(css, ".guide-heading h2", &[
        ("margin", "0"),
        ("color", theme.ink),
        ("font-size", "3rem"),
        ("letter-spacing", "0"),
        ("line-height", "1.06"),
        ("text-transform", "none"),
    ]);
}

fn append_runtime_cards(css: &mut String, theme: Theme) {
    rule(css, ".guide-runtime-grid", &[
        ("display", "grid"),
        ("grid-template-columns", "repeat(2, minmax(0, 1fr))"),
        ("gap", "16px"),
    ]);
    rule(css, ".guide-card", &[
        ("align-content", "start"),
        ("padding", "22px"),
    ]);
    rule(css, ".guide-card .guide-card-label", &[
        ("margin", "0"),
        ("color", theme.accent),
        ("font-size", "0.72rem"),
        ("font-weight", "900"),
        ("letter-spacing", "0.08em"),
        ("text-transform", "uppercase"),
    ]);
    rule(css, ".guide-card-links", &[
        ("display", "flex"),
        ("flex-wrap", "wrap"),
        ("gap", "8px"),
        ("padding-top", "6px"),
    ]);
    rule(css, ".guide-card-link", &[
        ("display", "inline-flex"),
        ("min-height", "32px"),
        ("align-items", "center"),
        ("border", border(theme.line_strong)),
        ("border-radius", "6px"),
        ("padding", "0 10px"),
        ("background", theme.panel_alt),
        ("color", theme.ink_soft),
        ("font-family", "inherit"),
        ("font-size", "0.76rem"),
        ("font-weight", "800"),
        ("line-height", "1"),
        ("text-decoration", "none"),
        ("text-transform", "uppercase"),
        ("box-shadow", "none"),
        ("cursor", "pointer"),
    ]);
    rule(css, ".guide-card-link:hover", &[
        ("border-color", "#111827"),
        ("background", "#111827"),
        ("color", "#fff"),
    ]);
}

fn append_steps(css: &mut String, theme: Theme) {
    rule(css, ".guide-step-grid", &[
        ("display", "grid"),
        ("grid-template-columns", "minmax(0, 1fr)"),
        ("gap", "16px"),
    ]);
    // A step is one row: its copy on the left, its commands on the right. The commands get the
    // wider column because a command line does not wrap; the copy does.
    rule(css, ".guide-step", &[
        ("display", "grid"),
        ("min-width", "0"),
        ("grid-template-columns", "minmax(0, 0.8fr) minmax(0, 1.2fr)"),
        ("gap", "24px"),
        ("align-items", "start"),
        ("border", border(theme.line)),
        ("border-radius", "8px"),
        ("padding", "20px"),
        ("background", theme.panel),
        ("color", theme.ink_soft),
    ]);
    rule(css, ".guide-step-copy", &[
        ("display", "grid"),
        ("min-width", "0"),
        ("align-content", "start"),
        ("gap", "12px"),
    ]);
    rule(css, ".guide-step-heading", &[
        ("display", "grid"),
        ("grid-template-columns", "auto minmax(0, 1fr)"),
        ("gap", "10px"),
        ("align-items", "center"),
    ]);
    rule(css, ".guide-step-index", &[
        ("color", theme.accent),
        (
            "font-family",
            "\"SFMono-Regular\", Consolas, \"Liberation Mono\", Menlo, monospace",
        ),
        ("font-size", "0.8rem"),
        ("font-weight", "900"),
    ]);
    rule(css, ".guide-step .landing-code", &[
        ("padding", "14px"),
        ("font-size", "0.76rem"),
    ]);
    rule(css, ".guide-step .guide-card-link", &[(
        "justify-self",
        "start",
    )]);
}
