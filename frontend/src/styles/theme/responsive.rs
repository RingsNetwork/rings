use super::{border, media};

pub(super) fn append(css: &mut String) {
    append_wide_breakpoints(css);
    append_mid_breakpoint(css);
    append_mobile_breakpoint(css);
    append_small_breakpoint(css);
}

fn append_wide_breakpoints(css: &mut String) {
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
}

fn append_mid_breakpoint(css: &mut String) {
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
}

fn append_mobile_breakpoint(css: &mut String) {
    append_mobile_navigation(css);
    append_mobile_landing(css);
}

fn append_mobile_navigation(css: &mut String) {
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
        ],
    );
}

fn append_mobile_landing(css: &mut String) {
    media(
        css,
        "(max-width: 720px)",
        &[
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
}

fn append_small_breakpoint(css: &mut String) {
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
