use std::fmt::Write as _;

pub(super) const THEME_CSS_CAPACITY: usize = 24_000;

#[derive(Clone, Copy)]
struct Theme {
    page: &'static str,
    page_alt: &'static str,
    panel: &'static str,
    panel_alt: &'static str,
    line: &'static str,
    line_strong: &'static str,
    ink: &'static str,
    ink_soft: &'static str,
    muted: &'static str,
    accent: &'static str,
    teal: &'static str,
    amber: &'static str,
}

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

pub(super) fn append(css: &mut String) {
    append_navigation(css, WARM);
    append_landing(css, WARM);
    append_console(css, WARM);
    append_topology(css, WARM);
    append_dialogs(css, WARM);
    append_responsive(css);
}

fn append_navigation(css: &mut String, theme: Theme) {
    rule(
        css,
        ".header-nav",
        &[
            ("display", "flex"),
            ("flex-wrap", "wrap"),
            ("gap", "8px"),
            ("align-items", "center"),
            ("justify-content", "flex-end"),
            ("min-width", "0"),
        ],
    );
    rule(
        css,
        ".header-nav-button,.header-github-link",
        &[
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
        ],
    );
    rule(
        css,
        ".header-nav-button.active,.header-nav-button:hover,.header-github-link:hover",
        &[
            ("border-color", "rgba(0, 229, 255, 0.34)"),
            ("background", "rgba(0, 229, 255, 0.11)"),
            ("color", "var(--blue)"),
        ],
    );
    rule(
        css,
        ".landing-header",
        &[
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
        ],
    );
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
    rule(
        css,
        ".landing-header-brand>div",
        &[("display", "grid"), ("gap", "1px"), ("min-width", "0")],
    );
    rule(
        css,
        ".landing-header-brand strong",
        &[
            ("overflow", "hidden"),
            ("font-size", "0.92rem"),
            ("line-height", "1.1"),
            ("text-overflow", "ellipsis"),
            ("white-space", "nowrap"),
        ],
    );
    rule(
        css,
        ".landing-header-brand span:last-child",
        &[
            ("color", "#667085"),
            ("font-size", "0.72rem"),
            ("line-height", "1.1"),
        ],
    );
    rule(
        css,
        ".landing-header-mark",
        &[
            ("display", "grid"),
            ("width", "34px"),
            ("height", "34px"),
            ("flex", "0 0 auto"),
            ("place-items", "center"),
            ("border-radius", "8px"),
            ("background", "#111827"),
            ("color", "#fff"),
        ],
    );
    rule(
        css,
        ".landing-header-logo",
        &[
            ("display", "block"),
            ("width", "23px"),
            ("height", "23px"),
            ("object-fit", "contain"),
        ],
    );
    rule(
        css,
        ".landing-header .header-nav",
        &[
            ("display", "grid"),
            ("grid-template-columns", "68px 68px 78px 112px"),
            ("gap", "6px"),
        ],
    );
    rule(
        css,
        ".landing-header .header-nav-button,.landing-header .header-github-link",
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
    rule(
        css,
        ".extension-mode .landing-header",
        &[("grid-template-columns", "minmax(0, 1fr)")],
    );
}

fn append_landing(css: &mut String, theme: Theme) {
    rule(
        css,
        ".guide-shell",
        &[
            ("position", "relative"),
            ("z-index", "10"),
            ("grid-template-rows", "auto minmax(0, 1fr)"),
            ("gap", "0"),
            ("padding", "0"),
            ("background", theme.page),
        ],
    );
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
    rule(
        css,
        ".landing-hero",
        &[
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
        ],
    );
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
    rule(
        css,
        ".landing-hero::after",
        &[
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
        ],
    );
    rule(
        css,
        ".landing-hero-copy",
        &[
            ("position", "relative"),
            ("z-index", "2"),
            ("display", "grid"),
            ("max-width", "680px"),
            ("gap", "22px"),
        ],
    );
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
    rule(
        css,
        ".landing-hero h2",
        &[
            ("max-width", "820px"),
            ("margin", "0"),
            ("color", theme.ink),
            ("font-size", "4rem"),
            ("letter-spacing", "0"),
            ("line-height", "1.02"),
            ("text-transform", "none"),
        ],
    );
    rule(
        css,
        ".landing-lede",
        &[
            ("max-width", "640px"),
            ("margin", "0"),
            ("color", "#344054"),
            ("font-size", "1.08rem"),
            ("line-height", "1.72"),
        ],
    );
    rule(
        css,
        ".landing-actions",
        &[
            ("display", "flex"),
            ("flex-wrap", "wrap"),
            ("gap", "10px"),
            ("align-items", "center"),
        ],
    );
    rule(
        css,
        ".landing-primary-action,.landing-secondary-action",
        &[
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
        ],
    );
    rule(
        css,
        ".landing-primary-action",
        &[
            ("border", "1px solid #111827"),
            ("background", "#111827"),
            ("color", "#fff"),
            ("box-shadow", "0 12px 28px rgba(17, 24, 39, 0.16)"),
        ],
    );
    rule(
        css,
        ".landing-primary-action:hover",
        &[
            ("border-color", "#344054"),
            ("background", "#344054"),
            ("color", "#fff"),
        ],
    );
    rule(
        css,
        ".landing-secondary-action",
        &[
            ("border", "0"),
            ("background", theme.panel),
            ("color", theme.ink_soft),
        ],
    );
    rule(
        css,
        ".landing-secondary-action:hover",
        &[
            ("border-color", theme.accent),
            ("background", "#fff5f4"),
            ("color", theme.accent),
        ],
    );
    append_landing_sections(css, theme);
}

fn append_landing_sections(css: &mut String, theme: Theme) {
    rule(
        css,
        ".landing-section",
        &[
            ("display", "grid"),
            ("gap", "24px"),
            ("padding", "44px 0"),
            ("border-top", border(theme.line)),
            ("scroll-margin-top", "88px"),
        ],
    );
    rule(
        css,
        ".landing-section-heading",
        &[("display", "grid"), ("max-width", "760px"), ("gap", "10px")],
    );
    rule(
        css,
        ".landing-section-heading h2,.landing-final h2",
        &[
            ("margin", "0"),
            ("color", theme.ink),
            ("font-size", "2rem"),
            ("line-height", "1.12"),
            ("text-transform", "none"),
        ],
    );
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
    rule(
        css,
        ".landing-feature-grid",
        &[
            ("display", "grid"),
            ("grid-template-columns", "repeat(2, minmax(0, 1fr))"),
            ("gap", "16px"),
        ],
    );
    rule(
        css,
        ".landing-example-grid",
        &[
            ("display", "grid"),
            ("grid-template-columns", "repeat(3, minmax(0, 1fr))"),
            ("gap", "14px"),
        ],
    );
    rule(
        css,
        ".landing-feature-card,.landing-example-card",
        &[
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
        ],
    );
    rule(
        css,
        ".landing-feature-card",
        &[
            (
                "grid-template-columns",
                "minmax(220px, 0.92fr) minmax(0, 1fr)",
            ),
            ("align-items", "stretch"),
            ("min-height", "248px"),
            ("gap", "20px"),
            ("border-color", "#ded0bd"),
            ("background", "#fbf4e8"),
        ],
    );
    rule(
        css,
        ".landing-feature-copy",
        &[
            ("display", "grid"),
            ("align-content", "start"),
            ("gap", "10px"),
        ],
    );
    rule(
        css,
        ".landing-feature-illustration",
        &[
            ("display", "block"),
            ("width", "100%"),
            ("height", "100%"),
            ("min-height", "206px"),
            ("align-self", "stretch"),
            ("border-radius", "6px"),
            ("background", theme.panel_alt),
            ("object-fit", "cover"),
            ("object-position", "center"),
        ],
    );
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
    rule(
        css,
        ".landing-example-card:hover",
        &[("border-color", theme.accent), ("color", theme.ink_soft)],
    );
    rule(
        css,
        ".landing-architecture",
        &[("grid-template-columns", "minmax(0, 1fr)")],
    );
    rule(
        css,
        ".landing-architecture-grid",
        &[
            ("display", "grid"),
            (
                "grid-template-columns",
                "minmax(260px, 0.36fr) minmax(0, 1fr)",
            ),
            ("min-width", "0"),
            ("gap", "24px"),
            ("align-items", "stretch"),
        ],
    );
    rule(
        css,
        ".landing-runtime",
        &[
            (
                "grid-template-columns",
                "minmax(300px, 0.48fr) minmax(360px, 1fr)",
            ),
            ("align-items", "start"),
        ],
    );
    rule(
        css,
        ".landing-runtime-visual",
        &[("display", "grid"), ("min-width", "0"), ("gap", "12px")],
    );
    rule(
        css,
        ".landing-layer-stack",
        &[
            ("display", "grid"),
            ("gap", "0"),
            ("align-content", "start"),
            ("overflow", "hidden"),
            ("border", border(theme.line_strong)),
            ("border-radius", "6px"),
            ("background", "#fbf4e8"),
        ],
    );
    rule(
        css,
        ".landing-layer",
        &[
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
        ],
    );
    rule(css, ".landing-layer>div", &[("min-width", "0")]);
    rule(css, ".landing-layer:last-child", &[("border-bottom", "0")]);
    rule(
        css,
        ".landing-layer:hover,.landing-layer.active",
        &[
            ("border-left-color", theme.accent),
            ("background", "#fffaf0"),
            ("color", theme.ink_soft),
        ],
    );
    rule(
        css,
        ".landing-layer:hover .landing-layer-index,.landing-layer.active .landing-layer-index",
        &[("color", theme.accent)],
    );
    rule(
        css,
        ".landing-layer-index",
        &[
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
        ],
    );
    rule(
        css,
        ".landing-layer h3",
        &[
            ("margin", "0 0 4px"),
            ("color", theme.ink),
            ("font-size", "0.95rem"),
            ("font-weight", "800"),
            ("line-height", "1.25"),
            ("text-transform", "none"),
            ("white-space", "normal"),
        ],
    );
    rule(
        css,
        ".landing-layer p",
        &[
            ("margin", "0"),
            ("color", "#5f5140"),
            ("font-size", "0.82rem"),
            ("line-height", "1.45"),
            ("overflow-wrap", "anywhere"),
            ("white-space", "normal"),
        ],
    );
    rule(
        css,
        ".landing-layer-detail-summary",
        &[
            ("color", theme.ink_soft),
            ("font-size", "0.98rem"),
            ("font-weight", "700"),
            ("line-height", "1.62"),
        ],
    );
    rule(
        css,
        ".landing-layer-label",
        &[
            ("display", "block"),
            ("margin", "0 0 5px"),
            ("color", theme.accent),
            ("font-size", "0.72rem"),
            ("font-weight", "900"),
            ("letter-spacing", "0"),
            ("text-transform", "none"),
        ],
    );
    rule(
        css,
        ".landing-layer-detail-section",
        &[("display", "grid"), ("gap", "6px")],
    );
    rule(
        css,
        ".landing-layer-detail-section>span",
        &[
            ("color", theme.accent),
            (
                "font-family",
                "\"SFMono-Regular\", Consolas, \"Liberation Mono\", Menlo, monospace",
            ),
            ("font-size", "0.72rem"),
            ("font-weight", "900"),
            ("letter-spacing", "0"),
        ],
    );
    rule(
        css,
        ".landing-layer-detail-section p",
        &[
            ("margin", "0"),
            ("color", "#475467"),
            ("font-size", "0.94rem"),
            ("line-height", "1.68"),
        ],
    );
    rule(
        css,
        ".landing-layer-detail-list dt",
        &[
            ("display", "block"),
            ("margin", "0 0 5px"),
            ("color", theme.accent),
            ("font-size", "0.72rem"),
            ("font-weight", "900"),
            ("letter-spacing", "0"),
            ("text-transform", "none"),
        ],
    );
    rule(
        css,
        ".landing-layer-detail",
        &[
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
        ],
    );
    rule(
        css,
        ".landing-layer-detail-heading",
        &[
            ("display", "grid"),
            ("grid-template-columns", "48px minmax(0, 1fr)"),
            ("gap", "14px"),
            ("align-items", "start"),
        ],
    );
    rule(
        css,
        ".landing-layer-detail-index",
        &[
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
        ],
    );
    rule(
        css,
        ".landing-layer-detail-list",
        &[
            ("display", "grid"),
            ("grid-template-columns", "repeat(2, minmax(0, 1fr))"),
            ("gap", "14px"),
            ("margin", "0"),
        ],
    );
    rule(
        css,
        ".landing-layer-detail-list div",
        &[
            ("border-top", border(theme.line)),
            ("padding-top", "12px"),
            ("min-width", "0"),
        ],
    );
    rule(
        css,
        ".landing-layer-detail-list dd",
        &[
            ("margin", "0"),
            ("color", theme.ink_soft),
            ("font-size", "0.92rem"),
            ("line-height", "1.58"),
        ],
    );
    rule(
        css,
        ".landing-code",
        &[
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
        ],
    );
    rule(
        css,
        ".landing-code code",
        &[(
            "font-family",
            "\"SFMono-Regular\", Consolas, \"Liberation Mono\", Menlo, monospace",
        )],
    );
    rule(
        css,
        ".landing-final",
        &[
            ("display", "grid"),
            ("grid-template-columns", "minmax(0, 1fr) auto"),
            ("gap", "24px"),
            ("align-items", "center"),
            ("border", border(theme.line_strong)),
            ("border-radius", "8px"),
            ("padding", "28px"),
            ("background", theme.panel),
        ],
    );
    rule(
        css,
        ".landing-final>div",
        &[("display", "grid"), ("gap", "8px")],
    );
    rule(
        css,
        ".landing-final span",
        &[
            ("color", "#475467"),
            ("font-size", "0.98rem"),
            ("line-height", "1.6"),
        ],
    );
}

fn append_console(css: &mut String, theme: Theme) {
    rule(
        css,
        ".topology-shell",
        &[
            ("position", "relative"),
            ("z-index", "10"),
            ("background", theme.page),
            ("color", "#111827"),
            ("font-family", "Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, \"Segoe UI\", sans-serif"),
        ],
    );
    rule(
        css,
        ".topology-shell::before",
        &[
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
            ("opacity", "0.72"),
            ("filter", "saturate(0.88) contrast(0.96)"),
        ],
    );
    rule(
        css,
        ".topology-shell::after",
        &[
            ("content", "\"\""),
            ("position", "absolute"),
            ("inset", "0"),
            ("z-index", "0"),
            ("pointer-events", "none"),
            ("background", "linear-gradient(90deg, rgba(243, 234, 216, 0.88) 0%, rgba(243, 234, 216, 0.66) 42%, rgba(243, 234, 216, 0.28) 100%), linear-gradient(180deg, rgba(251, 244, 230, 0.58) 0%, rgba(243, 234, 216, 0.72) 100%)"),
        ],
    );
    rule(
        css,
        ".topology-shell>.landing-header,.topology-shell>.network-stage",
        &[("position", "relative"), ("z-index", "1")],
    );
    rule(
        css,
        ".topology-shell .landing-header",
        &[
            ("width", "calc(100% + 24px)"),
            ("margin", "-12px -12px 0"),
            ("background", "rgba(251, 244, 230, 0.88)"),
            ("backdrop-filter", "blur(10px)"),
        ],
    );
    rule(
        css,
        ".topology-shell .app-header:not(.landing-header),.topology-shell .node-band,.topology-shell .network-stage",
        &[
            ("border-color", theme.line),
            ("background", theme.panel),
            ("box-shadow", "0 1px 2px rgba(112, 84, 48, 0.08)"),
        ],
    );
    rule(
        css,
        ".topology-shell .app-header:not(.landing-header)",
        &[
            ("min-height", "64px"),
            ("padding", "10px 12px"),
            ("border-radius", "8px"),
        ],
    );
    rule(css, ".topology-shell .app-header h1,.topology-shell .section-heading h2,.topology-shell .section-heading h3,.topology-shell .tool-header h3,.topology-shell .command-panel-header h3", &[("color", theme.ink)]);
    rule(
        css,
        ".topology-shell .tool-block h3",
        &[("color", theme.accent)],
    );
    rule(
        css,
        ".topology-shell .app-header .eyebrow",
        &[("color", theme.accent)],
    );
    themed_nav(css, ".topology-shell", theme);
    rule(css, ".topology-shell .node-band", &[("background", "linear-gradient(90deg, rgba(180, 35, 24, 0.06), transparent 42%, rgba(15, 118, 110, 0.05)), rgba(255, 250, 240, 0.9)")]);
    rule(css, ".topology-shell .network-stage", &[("background", "linear-gradient(90deg, rgba(243, 234, 216, 0.44) 0%, rgba(243, 234, 216, 0.28) 46%, rgba(243, 234, 216, 0.08) 100%), linear-gradient(180deg, rgba(255, 250, 240, 0.3), rgba(255, 246, 230, 0.18)), url(\"assets/images/rings-market-hero.png\") center / cover no-repeat"), ("backdrop-filter", "blur(2px)")]);
    rule(
        css,
        ".topology-shell.extension-mode",
        &[("padding-bottom", "calc(84px + env(safe-area-inset-bottom))")],
    );
    rule(
        css,
        ".topology-shell.extension-mode .topology-layout",
        &[("grid-template-columns", "minmax(0, 1fr)")],
    );
    rule(
        css,
        ".topology-shell.extension-mode .network-stage",
        &[("backdrop-filter", "none")],
    );
    append_console_controls(css, theme);
}

fn append_console_controls(css: &mut String, theme: Theme) {
    rule(
        css,
        ".topology-shell button:not(.header-nav-button)",
        &[
            ("border-color", theme.line_strong),
            ("border-radius", "8px"),
            ("background", theme.panel),
            ("color", theme.ink_soft),
            ("box-shadow", "none"),
        ],
    );
    rule(
        css,
        ".topology-shell button:not(.header-nav-button):hover",
        &[
            ("border-color", "#111827"),
            ("background", "#111827"),
            ("color", "#fff"),
        ],
    );
    rule(
        css,
        ".topology-shell button.secondary:not(.header-nav-button)",
        &[
            ("border-color", theme.line_strong),
            ("background", theme.panel),
            ("color", theme.ink_soft),
        ],
    );
    rule(
        css,
        ".topology-shell input,.topology-shell select,.topology-shell textarea",
        &[
            ("border-color", theme.line_strong),
            ("border-radius", "8px"),
            ("background", theme.panel),
            ("color", "#111827"),
            ("box-shadow", "none"),
        ],
    );
    rule(
        css,
        ".topology-shell input:focus,.topology-shell select:focus,.topology-shell textarea:focus",
        &[
            ("border-color", theme.accent),
            ("outline", "3px solid rgba(180, 35, 24, 0.12)"),
        ],
    );
    rule(css, ".topology-shell .surface,.topology-shell .metric,.topology-shell .tool-block,.topology-shell .advanced-settings,.topology-shell .control-sidebar,.topology-shell .sidebar-command-panel,.topology-shell .rail-card,.topology-shell .sidebar-section,.topology-shell .empty-state,.topology-shell .list-item,.topology-shell .segmented,.topology-shell .sdp-step", &[("border-color", theme.line), ("background", theme.panel), ("box-shadow", "none")]);
    rule(css, ".topology-shell .control-sidebar", &[("background", "linear-gradient(180deg, rgba(255, 250, 240, 0.98), rgba(255, 246, 230, 0.96)), #fffaf0")]);
    rule(
        css,
        ".topology-shell .control-sidebar.collapsed",
        &[
            ("border-color", theme.line_strong),
            ("background", theme.panel_alt),
        ],
    );
    rule(css, ".topology-shell .sidebar-toggle,.topology-shell .control-sidebar.collapsed .sidebar-toggle", &[("border-color", theme.line_strong), ("background", theme.panel_alt), ("color", "#5f5140"), ("box-shadow", "none")]);
    rule(
        css,
        ".topology-shell .sidebar-toggle:hover",
        &[
            ("border-color", theme.accent),
            ("background", "#fff5f4"),
            ("color", theme.accent),
        ],
    );
    rule(css, ".topology-shell .sidebar-toggle-icon,.topology-shell .control-sidebar.collapsed .sidebar-toggle-label,.topology-shell .advanced-settings summary,.topology-shell .copy-button,.topology-shell .sdp-copy,.topology-shell .rail-copy,.topology-shell .rail-did", &[("color", theme.accent)]);
    rule(css, ".topology-shell .field span,.topology-shell .eyebrow,.topology-shell .metric span,.topology-shell .identity-value span,.topology-shell .debug-actions>span,.topology-shell .link-meta,.topology-shell .node-status-line span,.topology-shell .rail-card-header span,.topology-shell .rail-row span,.topology-shell .muted", &[("color", theme.muted)]);
    rule(css, ".topology-shell .eyebrow,.topology-shell .rail-card-header>span,.topology-shell .command-panel-header span", &[("color", theme.accent)]);
    rule(
        css,
        ".topology-shell .status,.topology-shell .signal-card p",
        &[("color", theme.amber)],
    );
    rule(css, ".topology-shell .metric strong,.topology-shell .identity-value code,.topology-shell .link-meta strong,.topology-shell .rail-row strong,.topology-shell .rail-did,.topology-shell .signal-card p", &[("color", "#111827")]);
    rule(
        css,
        ".topology-shell .workspace-tab",
        &[
            ("border-color", theme.line_strong),
            ("border-radius", "8px"),
            ("background", theme.panel),
            ("color", "#5f5140"),
        ],
    );
    rule(
        css,
        ".topology-shell .workspace-tab:hover,.topology-shell .workspace-tab.active",
        &[
            ("border-color", "#111827"),
            ("background", "#111827"),
            ("color", "#fff"),
            ("box-shadow", "none"),
        ],
    );
    rule(css, ".topology-shell .metric,.topology-shell .session-strip .metric,.topology-shell .node-control-group,.topology-shell .node-status-line,.topology-shell .sidebar-identity,.topology-shell .command-panel-header", &[("border-color", theme.line)]);
    rule(css, ".topology-shell .header-tags span,.topology-shell .rail-state,.topology-shell .payload-state", &[("border-color", theme.line), ("background", theme.panel_alt), ("color", "#5f5140")]);
    rule(
        css,
        ".topology-shell .rail-state.ready",
        &[
            ("border-color", "rgba(15, 118, 110, 0.24)"),
            ("background", "#dff4ed"),
            ("color", theme.teal),
        ],
    );
    rule(css, ".topology-shell .segment", &[("color", "#5f5140")]);
    rule(
        css,
        ".topology-shell .segment:hover,.topology-shell .segment.active",
        &[
            ("background", "#f6d8d2"),
            ("color", theme.accent),
            ("box-shadow", "none"),
        ],
    );
    rule(
        css,
        ".topology-shell .payload-output textarea",
        &[("background", theme.panel), ("color", "#111827")],
    );
    rule(css, ".topology-shell .topology-add-button", &[("border-color", theme.line_strong), ("background", "radial-gradient(circle at 50% 42%, rgba(180, 35, 24, 0.12), rgba(15, 118, 110, 0.08) 58%, rgba(255, 250, 240, 0.92)), #fff6e6"), ("box-shadow", "0 12px 24px rgba(112, 84, 48, 0.14)"), ("color", theme.accent)]);
    rule(css, ".topology-shell .topology-add-button:hover", &[("border-color", theme.accent), ("background", "radial-gradient(circle at 50% 42%, rgba(180, 35, 24, 0.16), rgba(15, 118, 110, 0.1) 58%, rgba(255, 250, 240, 0.94)), #fff6e6"), ("color", theme.accent)]);
    rule(
        css,
        ".topology-shell .extension-action-tabs",
        &[
            ("position", "fixed"),
            ("top", "auto"),
            ("left", "max(12px, env(safe-area-inset-left))"),
            ("right", "max(12px, env(safe-area-inset-right))"),
            ("bottom", "calc(10px + env(safe-area-inset-bottom))"),
            ("z-index", "60"),
            ("width", "auto"),
            ("min-height", "0"),
            ("padding", "6px"),
            ("border", border(theme.line_strong)),
            ("border-radius", "16px"),
            ("background", "rgba(251, 244, 230, 0.9)"),
            ("backdrop-filter", "none"),
            ("box-shadow", "0 12px 32px rgba(112, 84, 48, 0.18)"),
        ],
    );
    rule(
        css,
        ".topology-shell.extension-mode .modal-shell",
        &[
            ("position", "fixed"),
            ("inset", "0 0 calc(84px + env(safe-area-inset-bottom)) 0"),
            ("z-index", "120"),
            ("display", "grid"),
            ("place-items", "stretch"),
            ("padding", "0"),
            ("overflow", "hidden"),
        ],
    );
    rule(
        css,
        ".topology-shell.extension-mode .dialog-backdrop",
        &[("display", "none")],
    );
    rule(
        css,
        ".topology-shell.extension-mode .link-dialog,.topology-shell.extension-mode .setup-dialog,.topology-shell.extension-mode .workbench-dialog",
        &[
            ("width", "100dvw"),
            ("height", "100%"),
            ("max-height", "none"),
            ("border", "0"),
            ("border-radius", "0"),
            ("box-shadow", "none"),
        ],
    );
    rule(
        css,
        ".topology-shell .extension-action-tabs .sidebar-command-panel",
        &[
            ("display", "grid"),
            ("height", "auto"),
            ("gap", "0"),
            ("padding", "0"),
            ("border", "0"),
            ("background", "transparent"),
            ("box-shadow", "none"),
        ],
    );
    rule(
        css,
        ".topology-shell .extension-action-tabs .command-panel-header,.topology-shell .extension-action-tabs .rail-telemetry",
        &[("display", "none")],
    );
    rule(
        css,
        ".topology-shell .extension-action-tabs .command-grid",
        &[
            ("display", "grid"),
            ("grid-template-columns", "repeat(3, minmax(0, 1fr))"),
            ("gap", "6px"),
        ],
    );
    rule(
        css,
        ".topology-shell .extension-action-tabs .command-grid>*",
        &[("display", "grid"), ("min-width", "0")],
    );
    rule(
        css,
        ".topology-shell .extension-action-tabs .command-grid .action-button,.topology-shell .extension-action-tabs .command-grid .link-open",
        &[
            ("display", "grid"),
            ("min-height", "50px"),
            ("place-items", "center"),
            ("padding", "6px 4px 5px"),
            ("border-radius", "11px"),
            ("font-size", "0.86rem"),
            ("line-height", "1.05"),
        ],
    );
    rule(
        css,
        ".topology-shell .extension-action-tabs .command-grid .label-desktop",
        &[("display", "none")],
    );
    rule(
        css,
        ".topology-shell .extension-action-tabs .command-grid .label-mobile",
        &[
            ("display", "inline-grid"),
            ("gap", "4px"),
            ("place-items", "center"),
        ],
    );
}

fn append_topology(css: &mut String, theme: Theme) {
    append_warm_topology_rules(css, ".topology-shell .topology", theme);
    rule(css, ".topology-shell .topology,.topology-shell .chord-topology", &[("border-color", theme.line_strong), ("background", "radial-gradient(circle at 50% 50%, rgba(180, 35, 24, 0.05), transparent 18%), linear-gradient(90deg, rgba(122, 87, 46, 0.055) 1px, transparent 1px), linear-gradient(180deg, rgba(122, 87, 46, 0.045) 1px, transparent 1px), rgba(255, 246, 230, 0.34)"), ("background-size", "auto, 28px 28px, 28px 28px, auto"), ("box-shadow", "inset 0 0 34px rgba(112, 84, 48, 0.04)")]);
    rule(
        css,
        ".topology-shell .topology .ring-peer-label text,.topology-shell .topology .active-node-id",
        &[("stroke", "rgba(255, 250, 240, 0.92)")],
    );
}

fn append_warm_topology_rules(css: &mut String, selector: &str, theme: Theme) {
    rule(
        css,
        &format!("{selector} .orbit"),
        &[("stroke", "rgba(122, 87, 46, 0.42)")],
    );
    rule(
        css,
        &format!("{selector} .orbit.outer,{selector} .orbit.inner"),
        &[("stroke", "rgba(122, 87, 46, 0.22)")],
    );
    rule(
        css,
        &format!("{selector} .scan"),
        &[("stroke", "rgba(180, 35, 24, 0.34)")],
    );
    rule(
        css,
        &format!("{selector} .ring-edge,{selector} .finger-link"),
        &[("stroke", "rgba(122, 87, 46, 0.42)")],
    );
    rule(
        css,
        &format!("{selector} .ring-flow"),
        &[("stroke", "rgba(15, 118, 110, 0.58)")],
    );
    rule(
        css,
        &format!("{selector} .finger-flow"),
        &[("stroke", "rgba(180, 35, 24, 0.48)")],
    );
    rule(
        css,
        &format!("{selector} .id-space-core"),
        &[
            ("fill", theme.page_alt),
            ("stroke", "rgba(122, 87, 46, 0.42)"),
        ],
    );
    rule(
        css,
        &format!("{selector} .core-label"),
        &[("fill", "#231f1a")],
    );
    rule(
        css,
        &format!("{selector} .ring-node"),
        &[("filter", "none")],
    );
    rule(
        css,
        &format!("{selector} .peer-node"),
        &[("fill", theme.panel), ("stroke", "rgba(122, 87, 46, 0.72)")],
    );
    rule(
        css,
        &format!("{selector} .peer-node.connected"),
        &[("fill", "#dff4ed"), ("stroke", "rgba(15, 118, 110, 0.78)")],
    );
    rule(
        css,
        &format!("{selector} .local-node"),
        &[("fill", "#f6d8d2"), ("stroke", "rgba(180, 35, 24, 0.72)")],
    );
    rule(css, &format!("{selector} .node-label,{selector} .peer-index,{selector} .local-id,{selector} .successor-label text"), &[("fill", theme.accent)]);
    rule(css, &format!("{selector} .node-id,{selector} .empty-node-label,{selector} .topology-count,{selector} .ring-zero,{selector} .core-sub,{selector} .core-hint"), &[("fill", theme.muted)]);
    rule(
        css,
        &format!("{selector} .topology-mode"),
        &[("fill", theme.ink)],
    );
    rule(
        css,
        &format!("{selector} .predecessor-label text"),
        &[("fill", theme.amber)],
    );
}

fn append_dialogs(css: &mut String, theme: Theme) {
    rule(css, ".topology-shell .dialog-backdrop", &[("background", "linear-gradient(90deg, rgba(122, 87, 46, 0.05) 1px, transparent 1px), linear-gradient(180deg, rgba(122, 87, 46, 0.045) 1px, transparent 1px), rgba(49, 39, 24, 0.36)")]);
    rule(
        css,
        ".topology-shell .link-dialog",
        &[
            ("border-color", theme.line_strong),
            ("background", theme.panel),
            ("box-shadow", "0 18px 54px rgba(112, 84, 48, 0.24)"),
        ],
    );
    rule(
        css,
        ".topology-shell .dialog-header h2",
        &[("color", theme.ink)],
    );
    rule(
        css,
        ".topology-shell .dialog-header .eyebrow,.topology-shell .dialog-close::before",
        &[("color", theme.accent)],
    );
    rule(
        css,
        ".topology-shell .dialog-tabs",
        &[
            ("border-color", theme.line),
            ("background", theme.panel_alt),
        ],
    );
    rule(css, ".topology-shell .dialog-tab", &[("color", "#5f5140")]);
    rule(
        css,
        ".topology-shell .dialog-tab:hover,.topology-shell .dialog-tab.active",
        &[
            ("border-color", "#111827"),
            ("background", "#111827"),
            ("color", "#fff"),
        ],
    );
    rule(
        css,
        ".topology-shell .dialog-pane",
        &[
            ("border-color", theme.line),
            ("background", theme.panel_alt),
        ],
    );
}

fn append_responsive(css: &mut String) {
    media(
        css,
        "(max-width: 1260px)",
        &[
            (
                ".landing-hero",
                &[
                    ("align-items", "start"),
                    ("min-height", "560px"),
                    ("padding", "56px clamp(24px, 5vw, 56px) 48px"),
                ][..],
            ),
            (
                ".landing-feature-grid",
                &[("grid-template-columns", "repeat(2, minmax(0, 1fr))")][..],
            ),
        ],
    );
    media(
        css,
        "(max-width: 1080px)",
        &[
            (
                ".landing-feature-card",
                &[("grid-template-columns", "184px minmax(0, 1fr)")][..],
            ),
            (
                ".landing-feature-illustration",
                &[("min-height", "178px")][..],
            ),
        ],
    );
    media(
        css,
        "(max-width: 860px)",
        &[
            (
                ".landing-hero,.landing-architecture,.landing-architecture-grid,.landing-runtime",
                &[("grid-template-columns", "1fr")][..],
            ),
            (
                ".landing-feature-grid,.landing-example-grid",
                &[("grid-template-columns", "repeat(2, minmax(0, 1fr))")][..],
            ),
            (
                ".landing-feature-card",
                &[("grid-template-columns", "168px minmax(0, 1fr)")][..],
            ),
            (
                ".landing-feature-illustration",
                &[("min-height", "162px")][..],
            ),
            (
                ".landing-layer-stack",
                &[
                    ("grid-auto-flow", "column"),
                    ("grid-auto-columns", "218px"),
                    ("overflow-x", "auto"),
                ][..],
            ),
            (
                ".landing-layer",
                &[("border-bottom", "0"), ("border-right", border("#ded0bd"))][..],
            ),
            (".landing-layer:last-child", &[("border-right", "0")][..]),
            (
                ".app-shell:not(.extension-mode) .landing-header .header-nav",
                &[
                    ("position", "fixed"),
                    ("left", "0"),
                    ("right", "0"),
                    ("bottom", "0"),
                    ("z-index", "60"),
                    ("display", "grid"),
                    ("grid-template-columns", "repeat(4, minmax(0, 1fr))"),
                    ("gap", "0"),
                    ("width", "auto"),
                    ("padding", "8px 10% calc(8px + env(safe-area-inset-bottom))"),
                    ("border", "0"),
                    ("border-top", "1px solid #d7c6aa"),
                    ("border-radius", "0"),
                    ("background", "#fbf4e6"),
                    ("box-shadow", "0 -1px 0 rgba(17, 24, 39, 0.04)"),
                ][..],
            ),
            (
                ".app-shell:not(.extension-mode) .landing-header .header-nav-button,.app-shell:not(.extension-mode) .landing-header .header-github-link",
                &[
                    ("width", "100%"),
                    ("min-width", "0"),
                    ("height", "42px"),
                    ("min-height", "42px"),
                    ("padding", "0 4px"),
                    ("border", "0"),
                    ("border-radius", "4px"),
                    ("background", "transparent"),
                    ("font-size", "11px"),
                ][..],
            ),
            (
                ".app-shell:not(.extension-mode) .landing-header .header-nav-button.active",
                &[
                    ("background", "#111827"),
                    ("color", "#fff"),
                ][..],
            ),
            (
                ".app-shell:not(.extension-mode) .landing-header .header-nav-button:hover,.app-shell:not(.extension-mode) .landing-header .header-github-link:hover",
                &[
                    ("background", "#111827"),
                    ("color", "#fff"),
                ][..],
            ),
            (
                ".topology-shell:not(.extension-mode) .landing-header",
                &[("backdrop-filter", "none")][..],
            ),
            (
                ".topology-shell:not(.extension-mode)",
                &[("padding-bottom", "calc(72px + env(safe-area-inset-bottom))")][..],
            ),
            (
                ".guide-page",
                &[("padding-bottom", "calc(96px + env(safe-area-inset-bottom))")][..],
            ),
        ],
    );
    media(
        css,
        "(max-width: 720px)",
        &[
            (
                ".app-header",
                &[
                    ("align-items", "stretch"),
                    ("flex-direction", "column"),
                    ("padding", "6px 8px"),
                ][..],
            ),
            (
                ".header-nav",
                &[
                    ("display", "grid"),
                    ("width", "100%"),
                    ("grid-template-columns", "repeat(3, minmax(0, 1fr))"),
                ][..],
            ),
            (
                ".header-nav-button,.header-github-link",
                &[("width", "100%"), ("min-width", "0")][..],
            ),
            (".guide-shell", &[("gap", "0"), ("padding", "0")][..]),
            (
                ".landing-header",
                &[
                    ("display", "grid"),
                    ("grid-template-columns", "1fr"),
                    ("gap", "10px"),
                    ("padding", "10px 10%"),
                ][..],
            ),
            (
                ".app-shell:not(.extension-mode) .landing-header .header-nav",
                &[
                    ("position", "fixed"),
                    ("left", "0"),
                    ("right", "0"),
                    ("bottom", "0"),
                    ("z-index", "60"),
                    ("display", "grid"),
                    ("grid-template-columns", "repeat(4, minmax(0, 1fr))"),
                    ("gap", "0"),
                    ("width", "auto"),
                    ("padding", "8px 10% calc(8px + env(safe-area-inset-bottom))"),
                    ("border", "0"),
                    ("border-top", "1px solid #d7c6aa"),
                    ("border-radius", "0"),
                    ("background", "#fbf4e6"),
                    ("box-shadow", "0 -1px 0 rgba(17, 24, 39, 0.04)"),
                ][..],
            ),
            (
                ".app-shell:not(.extension-mode) .landing-header .header-nav-button,.app-shell:not(.extension-mode) .landing-header .header-github-link",
                &[
                    ("height", "42px"),
                    ("min-height", "42px"),
                    ("padding", "0 4px"),
                    ("border", "0"),
                    ("border-radius", "4px"),
                    ("background", "transparent"),
                    ("font-size", "11px"),
                ][..],
            ),
            (
                ".app-shell:not(.extension-mode) .landing-header .header-nav-button:hover,.app-shell:not(.extension-mode) .landing-header .header-github-link:hover,.app-shell:not(.extension-mode) .landing-header .header-nav-button.active",
                &[
                    ("background", "#111827"),
                    ("color", "#fff"),
                ][..],
            ),
            (
                ".topology-shell:not(.extension-mode)",
                &[("padding-bottom", "calc(72px + env(safe-area-inset-bottom))")][..],
            ),
            (
                ".guide-page",
                &[
                    ("gap", "34px"),
                    ("padding", "0 10% calc(96px + env(safe-area-inset-bottom))"),
                ][..],
            ),
            (
                ".landing-hero",
                &[("min-height", "520px"), ("padding", "42px 24px 32px")][..],
            ),
            (
                ".landing-hero::before",
                &[("background", "linear-gradient(180deg, rgba(251, 244, 230, 0.96) 0%, rgba(251, 244, 230, 0.9) 48%, rgba(251, 244, 230, 0.38) 100%)")][..],
            ),
            (".landing-hero h2", &[("font-size", "2.55rem")][..]),
            (".landing-lede", &[("font-size", "1rem")][..]),
            (".landing-section", &[("padding", "32px 0")][..]),
            (
                ".landing-section-heading h2,.landing-final h2",
                &[("font-size", "1.55rem")][..],
            ),
            (
                ".landing-feature-grid",
                &[("grid-template-columns", "1fr")][..],
            ),
            (
                ".landing-feature-card",
                &[("grid-template-columns", "204px minmax(0, 1fr)")][..],
            ),
            (
                ".landing-feature-illustration",
                &[("min-height", "180px")][..],
            ),
            (
                ".landing-layer",
                &[("grid-template-columns", "34px minmax(0, 1fr)")][..],
            ),
            (
                ".landing-layer-detail-heading",
                &[("grid-template-columns", "42px minmax(0, 1fr)")][..],
            ),
            (
                ".landing-layer-detail",
                &[("min-height", "auto"), ("padding", "22px")][..],
            ),
            (
                ".landing-layer-detail-list",
                &[("grid-template-columns", "1fr")][..],
            ),
            (".landing-final", &[("grid-template-columns", "1fr")][..]),
        ],
    );
    media(
        css,
        "(max-width: 520px)",
        &[
            (
                ".landing-feature-grid,.landing-example-grid",
                &[("grid-template-columns", "1fr")][..],
            ),
            (
                ".landing-feature-card",
                &[("grid-template-columns", "1fr")][..],
            ),
            (".landing-feature-illustration", &[("height", "208px")][..]),
            (".landing-hero h2", &[("font-size", "2.05rem")][..]),
        ],
    );
}

fn themed_nav(css: &mut String, scope: &str, theme: Theme) {
    rule(
        css,
        &format!("{scope} .header-nav-button,{scope} .header-github-link"),
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
        &format!("{scope} .header-nav-button.active,{scope} .header-nav-button:hover,{scope} .header-github-link:hover"),
        &[
            ("border-color", "#111827"),
            ("background", "#111827"),
            ("color", "#fff"),
        ],
    );
}

fn border(color: &'static str) -> &'static str {
    match color {
        "#dfd0b7" => "1px solid #dfd0b7",
        "#d7c6aa" => "1px solid #d7c6aa",
        _ => "1px solid currentColor",
    }
}

fn rule(css: &mut String, selector: &str, declarations: &[(&str, &str)]) {
    css.push('\n');
    css.push_str(selector);
    css.push('{');
    for (name, value) in declarations {
        let _ = write!(css, "{name}:{value};");
    }
    css.push('}');
}

fn media(css: &mut String, query: &str, rules: &[(&str, &[(&'static str, &'static str)])]) {
    let _ = write!(css, "\n@media {query}{{");
    for (selector, declarations) in rules {
        rule(css, selector, declarations);
    }
    css.push('}');
}
