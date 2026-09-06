use super::border;
use super::rule;
use super::themed_nav;
use super::Theme;

pub(super) fn append(css: &mut String, theme: Theme) {
    append_base_nav(css);
    append_landing_header(css, theme);
    append_header_brand(css, theme);
    append_header_nav(css, theme);
}

fn append_base_nav(css: &mut String) {
    rule(css, ".header-nav", &[
        ("display", "flex"),
        ("flex-wrap", "wrap"),
        ("gap", "8px"),
        ("align-items", "center"),
        ("justify-content", "flex-end"),
        ("min-width", "0"),
    ]);
    rule(css, ".header-nav-button,.header-external-link", &[
        ("display", "inline-flex"),
        ("min-height", "32px"),
        ("align-items", "center"),
        ("justify-content", "center"),
        ("border", "1px solid var(--line)"),
        ("border-radius", "3px"),
        ("padding", "6px 10px"),
        ("background", "rgba(255, 255, 255, 0.03)"),
        ("color", "var(--muted)"),
        ("font-size", "0.76rem"),
        ("font-weight", "800"),
        ("text-decoration", "none"),
        ("text-transform", "uppercase"),
        ("white-space", "nowrap"),
    ]);
    rule(
        css,
        ".header-nav-button.active,.header-nav-button:hover,.header-external-link:hover",
        &[
            ("border-color", "rgba(0, 229, 255, 0.34)"),
            ("background", "rgba(0, 229, 255, 0.11)"),
            ("color", "var(--blue)"),
        ],
    );
}

fn append_landing_header(css: &mut String, theme: Theme) {
    rule(css, ".landing-header", &[
        ("display", "grid"),
        ("min-height", "64px"),
        ("grid-template-columns", "minmax(0, 1fr) auto"),
        ("align-items", "center"),
        ("padding", "0 10vw"),
        ("border", "0"),
        ("border-bottom", border(theme.line)),
        ("border-radius", "0"),
        ("background", "rgba(251, 246, 235, 0.96)"),
        ("box-shadow", "0 1px 0 rgba(17, 24, 39, 0.04)"),
    ]);
}

fn append_header_brand(css: &mut String, theme: Theme) {
    rule(
        css,
        ".landing-header-brand",
        &[
            ("display", "inline-flex"),
            ("min-width", "0"),
            ("align-items", "center"),
            ("gap", "10px"),
            ("color", theme.ink),
            ("font-family", "Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, \"Segoe UI\", sans-serif"),
        ],
    );
    rule(css, ".landing-header-brand>div", &[
        ("display", "grid"),
        ("gap", "1px"),
        ("min-width", "0"),
    ]);
    rule(css, ".landing-header-brand strong", &[
        ("overflow", "hidden"),
        ("font-size", "0.92rem"),
        ("line-height", "1.1"),
        ("text-overflow", "ellipsis"),
        ("white-space", "nowrap"),
    ]);
    rule(css, ".landing-header-brand span:last-child", &[
        ("color", "#667085"),
        ("font-size", "0.72rem"),
        ("line-height", "1.1"),
    ]);
    rule(css, ".landing-header-mark", &[
        ("display", "grid"),
        ("width", "34px"),
        ("height", "34px"),
        ("flex", "0 0 auto"),
        ("place-items", "center"),
        ("border-radius", "8px"),
        ("background", "#111827"),
        ("color", "#fff"),
    ]);
    rule(css, ".landing-header-logo", &[
        ("display", "block"),
        ("width", "23px"),
        ("height", "23px"),
        ("object-fit", "contain"),
    ]);
}

fn append_header_nav(css: &mut String, theme: Theme) {
    rule(css, ".landing-header .header-nav", &[
        ("display", "grid"),
        ("grid-template-columns", "68px 68px 78px 112px"),
        ("gap", "6px"),
    ]);
    rule(
        css,
        ".landing-header .header-nav-button,.landing-header .header-external-link",
        &[
            ("box-sizing", "border-box"),
            ("height", "34px"),
            ("min-height", "34px"),
            ("padding", "0 10px"),
            (
                "font-family",
                "\"SFMono-Regular\", Consolas, \"Liberation Mono\", Menlo, monospace",
            ),
            ("font-size", "12px"),
            ("font-weight", "800"),
            ("line-height", "1"),
            ("letter-spacing", "0"),
            ("text-transform", "uppercase"),
        ],
    );
    themed_nav(css, ".landing-header", theme);
    rule(css, ".extension-mode .landing-header", &[(
        "grid-template-columns",
        "minmax(0, 1fr)",
    )]);
}
