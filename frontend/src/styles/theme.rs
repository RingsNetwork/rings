use std::fmt::Write as _;

pub(super) const THEME_CSS_CAPACITY: usize = 32_000;

mod console;
mod dialogs;
mod footer;
mod guide;
mod landing;
mod navigation;
mod responsive;
mod topology;

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
    navigation::append(css, WARM);
    landing::append(css, WARM);
    guide::append(css, WARM);
    footer::append(css, WARM);
    console::append(css, WARM);
    topology::append(css, WARM);
    dialogs::append(css, WARM);
    responsive::append(css);
    append_academic_landing(css);
}

/// Applies a restrained academic style to the public landing page, organizing its existing
/// content as a thesis followed by numbered sections, supporting structure, and examples.
fn append_academic_landing(css: &mut String) {
    rule(css, ".app-shell:not(.extension-mode) .landing-header", &[
        ("min-height", "76px"),
        ("border-bottom", "3px double #82796d"),
        ("background", "#f8f6f0"),
        ("box-shadow", "none"),
    ]);
    rule(css, ".app-shell:not(.extension-mode) .landing-header-mark", &[
        ("border", "0"),
        ("border-radius", "0"),
        ("background", "transparent"),
        ("padding", "0"),
    ]);
    rule(css, ".app-shell:not(.extension-mode) .landing-header-brand,.app-shell:not(.extension-mode) .landing-header-brand strong,.app-shell:not(.extension-mode) .landing-header-brand span:last-child", &[
        ("color", "#242a31"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
    ]);
    rule(css, ".app-shell:not(.extension-mode) .landing-header-brand strong", &[
        ("font-size", "1.05rem"),
        ("font-weight", "700"),
    ]);
    rule(css, ".app-shell:not(.extension-mode) .landing-header-brand span:last-child", &[
        ("font-size", ".78rem"),
    ]);
    rule(css, ".app-shell:not(.extension-mode) .landing-header .header-nav-button,.app-shell:not(.extension-mode) .landing-header .header-external-link", &[
        ("border", "0"),
        ("border-bottom", "1px solid transparent"),
        ("border-radius", "0"),
        ("background", "transparent"),
        ("box-shadow", "none"),
        ("color", "#3e4650"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("font-size", ".88rem"),
        ("font-weight", "500"),
        ("letter-spacing", ".03em"),
    ]);
    rule(css, ".app-shell:not(.extension-mode) .landing-header .header-nav-button.active,.app-shell:not(.extension-mode) .landing-header .header-nav-button:hover,.app-shell:not(.extension-mode) .landing-header .header-external-link:hover", &[
        ("border-bottom-color", "#853e48"),
        ("background", "transparent"),
        ("color", "#853e48"),
    ]);
    rule(css, ".landing-page", &[
        ("counter-reset", "academic-section"),
        ("background", "#f8f6f0"),
        ("color", "#252a30"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
    ]);
    rule(css, ".landing-hero", &[
        ("min-height", "590px"),
        ("align-content", "center"),
        ("border-bottom", "3px double #82796d"),
        ("background", "#f8f6f0"),
        ("padding-top", "78px"),
        ("padding-bottom", "78px"),
    ]);
    rule(css, ".landing-hero::before", &[
        ("display", "block"),
        ("content", "\"\""),
        ("position", "absolute"),
        ("inset", "0"),
        ("z-index", "1"),
        ("pointer-events", "none"),
        ("background", "linear-gradient(90deg, rgba(248,246,240,.88) 0%, rgba(248,246,240,.58) 36%, rgba(248,246,240,.08) 74%, rgba(248,246,240,.02) 100%), linear-gradient(180deg, rgba(248,246,240,.02), rgba(248,246,240,.16))"),
    ]);
    rule(css, ".landing-hero::after", &[
        ("content", "\"\""),
        ("position", "absolute"),
        ("inset", "0"),
        ("z-index", "0"),
        ("pointer-events", "none"),
        ("background-image", "url(\"assets/images/rings-market-hero.png\")"),
        ("background-position", "center"),
        ("background-size", "cover"),
        ("background-repeat", "no-repeat"),
        ("filter", "grayscale(.2) sepia(.08) saturate(.9) contrast(1.08)"),
        ("opacity", "1"),
    ]);
    rule(css, ".landing-hero-copy", &[
        ("max-width", "820px"),
        ("gap", "24px"),
    ]);
    rule(css, ".landing-kicker", &[
        ("width", "fit-content"),
        ("margin", "0"),
        ("border", "0"),
        ("border-bottom", "1px solid #853e48"),
        ("padding", "0 0 6px"),
        ("background", "transparent"),
        ("color", "#853e48"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("font-size", ".9rem"),
        ("font-weight", "600"),
        ("letter-spacing", ".12em"),
        ("transform", "none"),
    ]);
    rule(css, ".landing-hero h2", &[
        ("max-width", "900px"),
        ("color", "#1f2933"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("font-size", "clamp(3rem, 6vw, 5.4rem)"),
        ("font-weight", "600"),
        ("letter-spacing", "-.035em"),
        ("line-height", "1.08"),
        ("text-shadow", "none"),
        ("text-transform", "none"),
    ]);
    rule(css, ".landing-lede,.landing-section-lede,.landing-section-heading .landing-section-lede", &[
        ("max-width", "740px"),
        ("color", "#45494d"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("font-size", "1.14rem"),
        ("font-weight", "400"),
        ("line-height", "1.8"),
    ]);
    rule(css, ".landing-actions", &[
        ("gap", "14px"),
        ("margin-top", "8px"),
    ]);
    rule(css, ".landing-primary-action,.landing-secondary-action", &[
        ("min-height", "44px"),
        ("border", "1px solid #82796d"),
        ("border-radius", "2px"),
        ("padding", "10px 18px"),
        ("background", "transparent"),
        ("box-shadow", "none"),
        ("color", "#30363d"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("font-size", ".95rem"),
        ("font-weight", "500"),
        ("letter-spacing", ".02em"),
        ("text-transform", "none"),
        ("transition", "background 120ms ease, color 120ms ease"),
    ]);
    rule(css, ".landing-primary-action", &[
        ("border-color", "#853e48"),
        ("background", "#853e48"),
        ("color", "#fffdf8"),
    ]);
    rule(css, ".landing-primary-action:hover,.landing-secondary-action:hover", &[
        ("transform", "none"),
        ("border-color", "#652d36"),
        ("background", "#652d36"),
        ("box-shadow", "none"),
        ("color", "#fffdf8"),
    ]);
    rule(css, ".landing-section", &[
        ("counter-increment", "academic-section"),
        ("gap", "28px"),
        ("padding", "58px 0"),
        ("border-top", "1px solid #b8b0a3"),
        ("scroll-margin-top", "96px"),
    ]);
    rule(css, ".landing-section-heading", &[
        ("max-width", "850px"),
        ("gap", "14px"),
    ]);
    rule(css, ".landing-kicker,.landing-section-heading>p:not(.landing-section-lede),.landing-final>div>p", &[
        ("color", "#853e48"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("font-size", ".92rem"),
        ("font-weight", "600"),
        ("letter-spacing", ".12em"),
        ("text-transform", "uppercase"),
    ]);
    rule(css, ".landing-section-heading>p:not(.landing-section-lede)::before", &[
        ("content", "counter(academic-section, upper-roman) \". \""),
        ("color", "#82796d"),
        ("font-variant-numeric", "oldstyle-nums"),
    ]);
    rule(css, ".landing-section-heading h2,.landing-final h2", &[
        ("margin", "0"),
        ("color", "#1f2933"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("font-size", "clamp(2rem, 3.5vw, 3rem)"),
        ("font-weight", "600"),
        ("letter-spacing", "-.025em"),
        ("line-height", "1.18"),
        ("text-transform", "none"),
    ]);
    rule(css, ".landing-feature-grid", &[
        ("gap", "26px 34px"),
    ]);
    rule(css, ".landing-feature-card,.landing-example-card,.guide-card", &[
        ("border", "0"),
        ("border-top", "1px solid #b8b0a3"),
        ("border-radius", "0"),
        ("padding", "20px 0"),
        ("background", "transparent"),
        ("box-shadow", "none"),
        ("transform", "none"),
    ]);
    rule(css, ".landing-feature-card h3,.landing-example-card h3,.guide-card h3,.guide-step h3,.landing-layer h3,.landing-layer-detail h3", &[
        ("color", "#252a30"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("font-size", "1.22rem"),
        ("font-weight", "600"),
        ("line-height", "1.3"),
        ("text-transform", "none"),
    ]);
    rule(css, ".landing-feature-card p,.landing-example-card p,.guide-card p,.guide-step p,.landing-layer p,.landing-layer-detail>p", &[
        ("color", "#4a4d50"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("font-size", ".98rem"),
        ("line-height", "1.75"),
    ]);
    rule(css, ".landing-feature-illustration", &[
        ("border", "1px solid #d2ccbf"),
        ("border-radius", "0"),
        ("background", "transparent"),
        ("filter", "grayscale(.78) sepia(.22) saturate(.55) contrast(.92)"),
        ("image-rendering", "auto"),
        ("object-fit", "contain"),
    ]);
    rule(css, ".landing-feature-card:hover,.landing-example-card:hover", &[
        ("transform", "none"),
        ("border-color", "#853e48"),
        ("box-shadow", "none"),
    ]);
    rule(css, ".landing-architecture,.landing-runtime,.landing-examples", &[
        ("border", "0"),
        ("border-top", "1px solid #b8b0a3"),
        ("padding", "58px 0"),
        ("background", "transparent"),
        ("box-shadow", "none"),
    ]);
    rule(css, ".landing-layer-stack,.landing-layer-detail", &[
        ("border", "1px solid #b8b0a3"),
        ("border-radius", "0"),
        ("background", "#f1eee7"),
        ("box-shadow", "none"),
    ]);
    rule(css, ".landing-layer", &[
        ("border-left", "2px solid transparent"),
        ("border-bottom", "1px solid #d1cabd"),
        ("background", "transparent"),
        ("color", "#353a40"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("box-shadow", "none"),
        ("text-transform", "none"),
    ]);
    rule(css, ".landing-layer:nth-child(n)", &[("background", "transparent")]);
    rule(css, ".landing-layer:hover,.landing-layer.active", &[
        ("border-left-color", "#853e48"),
        ("background", "#e8e2d7"),
        ("color", "#252a30"),
    ]);
    rule(css, ".landing-layer-index,.landing-layer-detail-index", &[
        ("border", "1px solid #b8b0a3"),
        ("border-radius", "0"),
        ("background", "#f8f6f0"),
        ("color", "#853e48"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
    ]);
    rule(css, ".landing-runtime .landing-section-heading h2,.landing-runtime .landing-section-heading .landing-section-lede", &[
        ("color", "#1f2933"),
    ]);
    rule(css, ".landing-code", &[
        ("border", "1px solid #82796d"),
        ("border-radius", "0"),
        ("padding", "22px"),
        ("background", "#efebe2"),
        ("box-shadow", "none"),
        ("color", "#2e3338"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("font-size", ".94rem"),
        ("line-height", "1.8"),
    ]);
    rule(css, ".landing-code code", &[
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
    ]);
    rule(css, ".landing-final", &[
        ("border", "0"),
        ("border-top", "3px double #82796d"),
        ("border-bottom", "1px solid #b8b0a3"),
        ("border-radius", "0"),
        ("padding", "34px 0"),
        ("background", "transparent"),
        ("box-shadow", "none"),
    ]);
    rule(css, ".landing-final span", &[
        ("color", "#45494d"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
        ("font-size", "1.02rem"),
        ("line-height", "1.75"),
    ]);
    rule(css, ".site-footer", &[
        ("border-top", "1px solid #dfd0b7"),
        ("background", "#101828"),
        ("color", "#d0d5dd"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
    ]);
    rule(css, ".site-footer-logo", &[
        ("background", "#1f2937"),
    ]);
    rule(css, ".site-footer-brand strong,.site-footer-column h3,.site-footer-notice dt", &[
        ("color", "#fff"),
    ]);
    rule(css, ".site-footer-brand p,.site-footer-notice dd", &[
        ("color", "#98a2b3"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
    ]);
    rule(css, ".site-footer-link", &[
        ("color", "#d0d5dd"),
        ("font-family", "'Iowan Old Style', Baskerville, 'Palatino Linotype', 'Noto Serif CJK SC', 'Songti SC', 'Times New Roman', serif"),
    ]);
    rule(css, ".site-footer-link:hover", &[
        ("color", "#fff"),
    ]);
    rule(css, ".site-footer-notices,.site-footer-bottom", &[
        ("border-color", "rgba(255,255,255,.1)"),
    ]);
    rule(css, ".site-footer-bottom", &[
        ("color", "#667085"),
    ]);
}
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
