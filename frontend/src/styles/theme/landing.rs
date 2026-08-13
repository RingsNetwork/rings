use super::border;
use super::rule;
use super::Theme;

pub(super) fn append(css: &mut String, theme: Theme) {
    append_guide_shell(css, theme);
    append_hero(css, theme);
    append_landing_actions(css, theme);
    append_landing_sections(css, theme);
}

fn append_guide_shell(css: &mut String, theme: Theme) {
    rule(css, ".guide-shell", &[
        ("position", "relative"),
        ("z-index", "10"),
        ("grid-template-rows", "auto minmax(0, 1fr)"),
        ("gap", "0"),
        ("padding", "0"),
        ("background", theme.page),
    ]);
    rule(
        css,
        ".guide-page",
        &[
            ("display", "grid"),
            ("grid-template-columns", "minmax(0, 1fr)"),
            ("min-height", "0"),
            ("gap", "52px"),
            ("padding", "0 10% 64px"),
            ("background", "linear-gradient(180deg, #fbf4e6 0, #fbf4e6 620px, #f3ead8 620px), #f3ead8"),
            ("color", "#111827"),
            ("font-family", "Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, \"Segoe UI\", sans-serif"),
            ("overflow", "auto"),
        ],
    );
}

fn append_hero(css: &mut String, theme: Theme) {
    rule(css, ".landing-hero", &[
        ("position", "relative"),
        ("display", "grid"),
        ("min-height", "640px"),
        ("width", "100vw"),
        ("margin-left", "calc(50% - 50vw)"),
        ("margin-right", "calc(50% - 50vw)"),
        ("grid-template-columns", "minmax(0, 1fr)"),
        ("align-items", "center"),
        ("overflow", "hidden"),
        ("padding", "68px 10vw 58px"),
    ]);
    rule(
        css,
        ".landing-hero::before",
        &[
            ("content", "\"\""),
            ("position", "absolute"),
            ("inset", "0"),
            ("z-index", "1"),
            ("pointer-events", "none"),
            ("background", "linear-gradient(90deg, rgba(251, 244, 230, 0.96) 0%, rgba(251, 244, 230, 0.88) 34%, rgba(251, 244, 230, 0.38) 62%, rgba(251, 244, 230, 0.08) 100%), linear-gradient(180deg, rgba(251, 244, 230, 0.18) 0%, rgba(251, 244, 230, 0.08) 62%, rgba(251, 244, 230, 0.7) 100%)"),
        ],
    );
    rule(css, ".landing-hero::after", &[
        ("content", "\"\""),
        ("position", "absolute"),
        ("inset", "0"),
        ("z-index", "0"),
        ("pointer-events", "none"),
        (
            "background-image",
            "url(\"assets/images/rings-market-hero.png\")",
        ),
        ("background-position", "center"),
        ("background-size", "cover"),
        ("background-repeat", "no-repeat"),
    ]);
    rule(css, ".landing-hero-copy", &[
        ("position", "relative"),
        ("z-index", "2"),
        ("display", "grid"),
        ("max-width", "680px"),
        ("gap", "22px"),
    ]);
    rule(
        css,
        ".landing-kicker,.landing-section-heading>p:not(.landing-section-lede),.landing-final>div>p",
        &[
            ("margin", "0"),
            ("color", theme.accent),
            ("font-size", "0.78rem"),
            ("font-weight", "900"),
            ("letter-spacing", "0.08em"),
            ("text-transform", "uppercase"),
        ],
    );
    rule(css, ".landing-hero h2", &[
        ("max-width", "820px"),
        ("margin", "0"),
        ("color", theme.ink),
        ("font-size", "4rem"),
        ("letter-spacing", "0"),
        ("line-height", "1.02"),
        ("text-transform", "none"),
    ]);
    rule(css, ".landing-lede", &[
        ("max-width", "640px"),
        ("margin", "0"),
        ("color", "#344054"),
        ("font-size", "1.08rem"),
        ("line-height", "1.72"),
    ]);
}

fn append_landing_actions(css: &mut String, theme: Theme) {
    rule(css, ".landing-actions", &[
        ("display", "flex"),
        ("flex-wrap", "wrap"),
        ("gap", "10px"),
        ("align-items", "center"),
    ]);
    rule(css, ".landing-primary-action,.landing-secondary-action", &[
        ("display", "inline-flex"),
        ("min-height", "42px"),
        ("align-items", "center"),
        ("justify-content", "center"),
        ("border-radius", "8px"),
        ("padding", "10px 14px"),
        ("font-size", "0.84rem"),
        ("font-weight", "900"),
        ("text-decoration", "none"),
        ("text-transform", "uppercase"),
    ]);
    rule(css, ".landing-primary-action", &[
        ("border", "1px solid #111827"),
        ("background", "#111827"),
        ("color", "#fff"),
        ("box-shadow", "0 12px 28px rgba(17, 24, 39, 0.16)"),
    ]);
    rule(css, ".landing-primary-action:hover", &[
        ("border-color", "#344054"),
        ("background", "#344054"),
        ("color", "#fff"),
    ]);
    rule(css, ".landing-secondary-action", &[
        ("border", "0"),
        ("background", theme.panel),
        ("color", theme.ink_soft),
    ]);
    rule(css, ".landing-secondary-action:hover", &[
        ("border-color", theme.accent),
        ("background", "#fff5f4"),
        ("color", theme.accent),
    ]);
}

fn append_landing_sections(css: &mut String, theme: Theme) {
    append_section_shell(css, theme);
    append_feature_cards(css, theme);
    append_architecture(css, theme);
    append_final_callout(css, theme);
}

fn append_section_shell(css: &mut String, theme: Theme) {
    rule(css, ".landing-section", &[
        ("display", "grid"),
        ("gap", "24px"),
        ("padding", "44px 0"),
        ("border-top", border(theme.line)),
        ("scroll-margin-top", "88px"),
    ]);
    rule(css, ".landing-section-heading", &[
        ("display", "grid"),
        ("max-width", "760px"),
        ("gap", "10px"),
    ]);
    rule(css, ".landing-section-heading h2,.landing-final h2", &[
        ("margin", "0"),
        ("color", theme.ink),
        ("font-size", "2rem"),
        ("line-height", "1.12"),
        ("text-transform", "none"),
    ]);
    rule(
        css,
        ".landing-section-lede,.landing-section-heading .landing-section-lede",
        &[
            ("margin", "0"),
            ("color", "#475467"),
            ("font-size", "1rem"),
            ("line-height", "1.7"),
        ],
    );
}

fn append_feature_cards(css: &mut String, theme: Theme) {
    rule(css, ".landing-feature-grid", &[
        ("display", "grid"),
        ("grid-template-columns", "repeat(2, minmax(0, 1fr))"),
        ("gap", "16px"),
    ]);
    rule(css, ".landing-feature-card,.landing-example-card", &[
        ("display", "grid"),
        ("min-width", "0"),
        ("gap", "10px"),
        ("border", border(theme.line)),
        ("border-radius", "8px"),
        ("padding", "18px"),
        ("background", theme.panel),
        ("color", theme.ink_soft),
        ("text-decoration", "none"),
        ("box-shadow", "0 1px 2px rgba(16, 24, 40, 0.04)"),
    ]);
    rule(css, ".landing-feature-card", &[
        (
            "grid-template-columns",
            "minmax(220px, 0.92fr) minmax(0, 1fr)",
        ),
        ("align-items", "stretch"),
        ("min-height", "248px"),
        ("gap", "20px"),
        ("border-color", "#ded0bd"),
        ("background", "#fbf4e8"),
    ]);
    rule(css, ".landing-feature-copy", &[
        ("display", "grid"),
        ("align-content", "start"),
        ("gap", "10px"),
    ]);
    rule(css, ".landing-feature-illustration", &[
        ("display", "block"),
        ("width", "100%"),
        ("height", "100%"),
        ("min-height", "206px"),
        ("align-self", "stretch"),
        ("border-radius", "6px"),
        ("background", theme.panel_alt),
        ("object-fit", "cover"),
        ("object-position", "center"),
    ]);
    rule(
        css,
        ".landing-feature-card h3,.landing-example-card h3,.landing-layer h3,.landing-layer-detail h3",
        &[
            ("margin", "0"),
            ("color", theme.ink),
            ("font-size", "0.98rem"),
            ("line-height", "1.25"),
            ("text-transform", "none"),
        ],
    );
    rule(
        css,
        ".landing-feature-card p,.landing-example-card p,.landing-layer p,.landing-layer-detail>p",
        &[
            ("margin", "0"),
            ("color", "#475467"),
            ("font-size", "0.88rem"),
            ("line-height", "1.58"),
        ],
    );
    append_example_cards(css, theme);
}

fn append_example_cards(css: &mut String, theme: Theme) {
    rule(css, ".landing-example-grid", &[
        ("display", "grid"),
        ("grid-template-columns", "repeat(3, minmax(0, 1fr))"),
        ("gap", "14px"),
    ]);
    rule(css, ".landing-example-card:hover", &[
        ("border-color", theme.accent),
        ("color", theme.ink_soft),
    ]);
}

fn append_architecture(css: &mut String, theme: Theme) {
    append_architecture_layout(css);
    append_layer_stack(css, theme);
    append_layer_button(css, theme);
    append_layer_detail_text(css, theme);
    append_layer_detail_card(css, theme);
    append_layer_detail_list(css, theme);
    append_landing_code(css);
}

fn append_architecture_layout(css: &mut String) {
    rule(css, ".landing-architecture", &[(
        "grid-template-columns",
        "minmax(0, 1fr)",
    )]);
    rule(css, ".landing-architecture-grid", &[
        ("display", "grid"),
        (
            "grid-template-columns",
            "minmax(260px, 0.36fr) minmax(0, 1fr)",
        ),
        ("min-width", "0"),
        ("gap", "24px"),
        ("align-items", "stretch"),
    ]);
    rule(css, ".landing-runtime", &[
        (
            "grid-template-columns",
            "minmax(300px, 0.48fr) minmax(360px, 1fr)",
        ),
        ("align-items", "start"),
    ]);
    rule(css, ".landing-runtime-visual", &[
        ("display", "grid"),
        ("min-width", "0"),
        ("gap", "12px"),
    ]);
}

fn append_layer_stack(css: &mut String, theme: Theme) {
    rule(css, ".landing-layer-stack", &[
        ("display", "grid"),
        ("gap", "0"),
        ("align-content", "start"),
        ("overflow", "hidden"),
        ("border", border(theme.line_strong)),
        ("border-radius", "6px"),
        ("background", "#fbf4e8"),
    ]);
}

fn append_layer_button(css: &mut String, theme: Theme) {
    rule(css, ".landing-layer", &[
        ("display", "grid"),
        ("width", "100%"),
        ("grid-template-columns", "38px minmax(0, 1fr)"),
        ("gap", "12px"),
        ("align-items", "start"),
        ("border", "0"),
        ("border-left", "3px solid transparent"),
        ("border-bottom", border(theme.line)),
        ("border-radius", "0"),
        ("padding", "14px 14px 14px 11px"),
        ("background", "transparent"),
        ("color", theme.ink_soft),
        ("cursor", "pointer"),
        ("font", "inherit"),
        ("overflow", "hidden"),
        ("text-align", "left"),
        ("text-transform", "none"),
        ("white-space", "normal"),
        ("appearance", "none"),
        ("box-shadow", "none"),
    ]);
    rule(css, ".landing-layer>div", &[("min-width", "0")]);
    rule(css, ".landing-layer:last-child", &[("border-bottom", "0")]);
    rule(css, ".landing-layer:hover,.landing-layer.active", &[
        ("border-left-color", theme.accent),
        ("background", "#fffaf0"),
        ("color", theme.ink_soft),
    ]);
    rule(
        css,
        ".landing-layer:hover .landing-layer-index,.landing-layer.active .landing-layer-index",
        &[("color", theme.accent)],
    );
    rule(css, ".landing-layer-index", &[
        ("display", "grid"),
        ("width", "28px"),
        ("height", "24px"),
        ("place-items", "start center"),
        ("border", "0"),
        ("background", "transparent"),
        ("color", "#8a5a12"),
        (
            "font-family",
            "\"SFMono-Regular\", Consolas, \"Liberation Mono\", Menlo, monospace",
        ),
        ("font-size", "0.72rem"),
        ("font-weight", "900"),
        ("line-height", "1"),
    ]);
    rule(css, ".landing-layer h3", &[
        ("margin", "0 0 4px"),
        ("color", theme.ink),
        ("font-size", "0.95rem"),
        ("font-weight", "800"),
        ("line-height", "1.25"),
        ("text-transform", "none"),
        ("white-space", "normal"),
    ]);
    rule(css, ".landing-layer p", &[
        ("margin", "0"),
        ("color", "#5f5140"),
        ("font-size", "0.82rem"),
        ("line-height", "1.45"),
        ("overflow-wrap", "anywhere"),
        ("white-space", "normal"),
    ]);
}

fn append_layer_detail_text(css: &mut String, theme: Theme) {
    rule(css, ".landing-layer-detail-summary", &[
        ("color", theme.ink_soft),
        ("font-size", "0.98rem"),
        ("font-weight", "700"),
        ("line-height", "1.62"),
    ]);
    rule(css, ".landing-layer-label", &[
        ("display", "block"),
        ("margin", "0 0 5px"),
        ("color", theme.accent),
        ("font-size", "0.72rem"),
        ("font-weight", "900"),
        ("letter-spacing", "0"),
        ("text-transform", "none"),
    ]);
    rule(css, ".landing-layer-detail-section", &[
        ("display", "grid"),
        ("gap", "6px"),
    ]);
    rule(css, ".landing-layer-detail-section>span", &[
        ("color", theme.accent),
        (
            "font-family",
            "\"SFMono-Regular\", Consolas, \"Liberation Mono\", Menlo, monospace",
        ),
        ("font-size", "0.72rem"),
        ("font-weight", "900"),
        ("letter-spacing", "0"),
    ]);
}

fn append_layer_detail_card(css: &mut String, theme: Theme) {
    rule(css, ".landing-layer-detail-section p", &[
        ("margin", "0"),
        ("color", "#475467"),
        ("font-size", "0.94rem"),
        ("line-height", "1.68"),
    ]);
    rule(css, ".landing-layer-detail-list dt", &[
        ("display", "block"),
        ("margin", "0 0 5px"),
        ("color", theme.accent),
        ("font-size", "0.72rem"),
        ("font-weight", "900"),
        ("letter-spacing", "0"),
        ("text-transform", "none"),
    ]);
    rule(css, ".landing-layer-detail", &[
        ("display", "grid"),
        ("align-content", "start"),
        ("gap", "20px"),
        ("min-height", "438px"),
        ("border", border(theme.line_strong)),
        ("border-left", "3px solid #b42318"),
        ("border-radius", "6px"),
        ("padding", "26px 28px"),
        ("background", "#fffaf0"),
        ("box-shadow", "none"),
    ]);
    rule(css, ".landing-layer-detail-heading", &[
        ("display", "grid"),
        ("grid-template-columns", "48px minmax(0, 1fr)"),
        ("gap", "14px"),
        ("align-items", "start"),
    ]);
    rule(css, ".landing-layer-detail-index", &[
        ("display", "grid"),
        ("width", "40px"),
        ("height", "40px"),
        ("place-items", "center"),
        ("border", border(theme.line_strong)),
        ("border-radius", "4px"),
        ("background", "#fbf4e8"),
        ("color", theme.accent),
        (
            "font-family",
            "\"SFMono-Regular\", Consolas, \"Liberation Mono\", Menlo, monospace",
        ),
        ("font-size", "0.82rem"),
        ("font-weight", "900"),
        ("line-height", "1"),
    ]);
}

fn append_layer_detail_list(css: &mut String, theme: Theme) {
    rule(css, ".landing-layer-detail-list", &[
        ("display", "grid"),
        ("grid-template-columns", "repeat(2, minmax(0, 1fr))"),
        ("gap", "14px"),
        ("margin", "0"),
    ]);
    rule(css, ".landing-layer-detail-list div", &[
        ("border-top", border(theme.line)),
        ("padding-top", "12px"),
        ("min-width", "0"),
    ]);
    rule(css, ".landing-layer-detail-list dd", &[
        ("margin", "0"),
        ("color", theme.ink_soft),
        ("font-size", "0.92rem"),
        ("line-height", "1.58"),
    ]);
}

fn append_landing_code(css: &mut String) {
    rule(css, ".landing-code", &[
        ("min-width", "0"),
        ("overflow", "auto"),
        ("margin", "0"),
        ("border", "1px solid #1f2937"),
        ("border-radius", "8px"),
        ("padding", "18px"),
        ("background", "#101828"),
        ("color", "#f2f4f7"),
        ("font-size", "0.82rem"),
        ("line-height", "1.65"),
    ]);
    rule(css, ".landing-code code", &[(
        "font-family",
        "\"SFMono-Regular\", Consolas, \"Liberation Mono\", Menlo, monospace",
    )]);
}

fn append_final_callout(css: &mut String, theme: Theme) {
    rule(css, ".landing-final", &[
        ("display", "grid"),
        ("grid-template-columns", "minmax(0, 1fr) auto"),
        ("gap", "24px"),
        ("align-items", "center"),
        ("border", border(theme.line_strong)),
        ("border-radius", "8px"),
        ("padding", "28px"),
        ("background", theme.panel),
    ]);
    rule(css, ".landing-final>div", &[
        ("display", "grid"),
        ("gap", "8px"),
    ]);
    rule(css, ".landing-final span", &[
        ("color", "#475467"),
        ("font-size", "0.98rem"),
        ("line-height", "1.6"),
    ]);
}
