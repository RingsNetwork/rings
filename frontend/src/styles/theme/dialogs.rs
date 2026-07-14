use super::{rule, Theme};

pub(super) fn append(css: &mut String, theme: Theme) {
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
