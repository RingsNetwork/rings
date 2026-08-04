use super::border;
use super::rule;
use super::themed_nav;
use super::Theme;

pub(super) fn append(css: &mut String, theme: Theme) {
    append_console_shell(css, theme);
    append_console_header(css, theme);
    append_console_stage(css);
    append_console_controls(css, theme);
}

fn append_console_shell(css: &mut String, theme: Theme) {
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
    rule(css, ".topology-shell::before", &[
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
    ]);
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
}

fn append_console_header(css: &mut String, theme: Theme) {
    rule(css, ".topology-shell .landing-header", &[
        ("width", "calc(100% + 24px)"),
        ("margin", "-12px -12px 0"),
        ("background", "rgba(251, 244, 230, 0.88)"),
        ("backdrop-filter", "blur(10px)"),
    ]);
    rule(
        css,
        ".topology-shell .app-header:not(.landing-header),.topology-shell .node-band,.topology-shell .network-stage",
        &[
            ("border-color", theme.line),
            ("background", theme.panel),
            ("box-shadow", "0 1px 2px rgba(112, 84, 48, 0.08)"),
        ],
    );
    rule(css, ".topology-shell .app-header:not(.landing-header)", &[
        ("min-height", "64px"),
        ("padding", "10px 12px"),
        ("border-radius", "8px"),
    ]);
    rule(css, ".topology-shell .app-header h1,.topology-shell .section-heading h2,.topology-shell .section-heading h3,.topology-shell .tool-header h3,.topology-shell .command-panel-header h3", &[("color", theme.ink)]);
    rule(css, ".topology-shell .tool-block h3", &[(
        "color",
        theme.accent,
    )]);
    rule(css, ".topology-shell .app-header .eyebrow", &[(
        "color",
        theme.accent,
    )]);
    themed_nav(css, ".topology-shell", theme);
}

fn append_console_stage(css: &mut String) {
    rule(css, ".topology-shell .node-band", &[("background", "linear-gradient(90deg, rgba(180, 35, 24, 0.06), transparent 42%, rgba(15, 118, 110, 0.05)), rgba(255, 250, 240, 0.9)")]);
    rule(css, ".topology-shell .network-stage", &[("background", "linear-gradient(90deg, rgba(243, 234, 216, 0.44) 0%, rgba(243, 234, 216, 0.28) 46%, rgba(243, 234, 216, 0.08) 100%), linear-gradient(180deg, rgba(255, 250, 240, 0.3), rgba(255, 246, 230, 0.18)), url(\"assets/images/rings-market-hero.png\") center / cover no-repeat"), ("backdrop-filter", "blur(2px)")]);
    rule(css, ".topology-shell .network-stage:has(.modal-shell)", &[
        ("backdrop-filter", "none"),
    ]);
    rule(css, ".topology-shell.extension-mode", &[(
        "padding-bottom",
        "calc(84px + env(safe-area-inset-bottom))",
    )]);
    rule(css, ".topology-shell.extension-mode .topology-layout", &[(
        "grid-template-columns",
        "minmax(0, 1fr)",
    )]);
    rule(css, ".topology-shell.extension-mode .network-stage", &[(
        "backdrop-filter",
        "none",
    )]);
}

fn append_console_controls(css: &mut String, theme: Theme) {
    append_form_controls(css, theme);
    append_surface_controls(css, theme);
    append_workspace_controls(css, theme);
    append_extension_tabs(css, theme);
}

fn append_form_controls(css: &mut String, theme: Theme) {
    rule(
        css,
        ".topology-shell button:not(.header-nav-button):not(.dialog-backdrop)",
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
        ".topology-shell button:not(.header-nav-button):not(.dialog-backdrop):hover",
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
}

fn append_surface_controls(css: &mut String, theme: Theme) {
    rule(css, ".topology-shell .surface,.topology-shell .metric,.topology-shell .tool-block,.topology-shell .advanced-settings,.topology-shell .control-sidebar,.topology-shell .sidebar-command-panel,.topology-shell .rail-card,.topology-shell .sidebar-section,.topology-shell .empty-state,.topology-shell .list-item,.topology-shell .segmented,.topology-shell .sdp-step", &[("border-color", theme.line), ("background", theme.panel), ("box-shadow", "none")]);
    rule(css, ".topology-shell .control-sidebar", &[(
        "background",
        "linear-gradient(180deg, rgba(255, 250, 240, 0.98), rgba(255, 246, 230, 0.96)), #fffaf0",
    )]);
    rule(css, ".topology-shell .control-sidebar.collapsed", &[
        ("border-color", theme.line_strong),
        ("background", theme.panel_alt),
    ]);
    rule(css, ".topology-shell .sidebar-toggle,.topology-shell .control-sidebar.collapsed .sidebar-toggle", &[("border-color", theme.line_strong), ("background", theme.panel_alt), ("color", "#5f5140"), ("box-shadow", "none")]);
    rule(css, ".topology-shell .sidebar-toggle:hover", &[
        ("border-color", theme.accent),
        ("background", "#fff5f4"),
        ("color", theme.accent),
    ]);
    rule(css, ".topology-shell .sidebar-toggle-icon,.topology-shell .control-sidebar.collapsed .sidebar-toggle-label,.topology-shell .advanced-settings summary,.topology-shell .copy-button,.topology-shell .sdp-copy,.topology-shell .rail-copy,.topology-shell .rail-did", &[("color", theme.accent)]);
    rule(css, ".topology-shell .field span,.topology-shell .eyebrow,.topology-shell .metric span,.topology-shell .identity-value span,.topology-shell .debug-actions>span,.topology-shell .link-meta,.topology-shell .node-status-line span,.topology-shell .rail-card-header span,.topology-shell .rail-row span,.topology-shell .muted", &[("color", theme.muted)]);
    rule(css, ".topology-shell .eyebrow,.topology-shell .rail-card-header>span,.topology-shell .command-panel-header span", &[("color", theme.accent)]);
    rule(
        css,
        ".topology-shell .status,.topology-shell .signal-card p",
        &[("color", theme.amber)],
    );
    rule(css, ".topology-shell .metric strong,.topology-shell .identity-value code,.topology-shell .link-meta strong,.topology-shell .rail-row strong,.topology-shell .rail-did,.topology-shell .signal-card p", &[("color", "#111827")]);
}

fn append_workspace_controls(css: &mut String, theme: Theme) {
    rule(css, ".topology-shell .workspace-tab", &[
        ("border-color", theme.line_strong),
        ("border-radius", "8px"),
        ("background", theme.panel),
        ("color", "#5f5140"),
    ]);
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
    rule(css, ".topology-shell .rail-state.ready", &[
        ("border-color", "rgba(15, 118, 110, 0.24)"),
        ("background", "#dff4ed"),
        ("color", theme.teal),
    ]);
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
    rule(css, ".topology-shell .payload-output textarea", &[
        ("background", theme.panel),
        ("color", "#111827"),
    ]);
    rule(css, ".topology-shell .topology-add-button", &[("border-color", theme.line_strong), ("background", "radial-gradient(circle at 50% 42%, rgba(180, 35, 24, 0.12), rgba(15, 118, 110, 0.08) 58%, rgba(255, 250, 240, 0.92)), #fff6e6"), ("box-shadow", "0 12px 24px rgba(112, 84, 48, 0.14)"), ("color", theme.accent)]);
    rule(css, ".topology-shell .topology-add-button:hover", &[("border-color", theme.accent), ("background", "radial-gradient(circle at 50% 42%, rgba(180, 35, 24, 0.16), rgba(15, 118, 110, 0.1) 58%, rgba(255, 250, 240, 0.94)), #fff6e6"), ("color", theme.accent)]);
}

fn append_extension_tabs(css: &mut String, theme: Theme) {
    rule(css, ".topology-shell .extension-action-tabs", &[
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
    ]);
    append_extension_modal(css);
    append_extension_tab_content(css);
}

fn append_extension_modal(css: &mut String) {
    rule(css, ".topology-shell.extension-mode .modal-shell", &[
        ("position", "fixed"),
        ("inset", "0 0 calc(84px + env(safe-area-inset-bottom)) 0"),
        ("z-index", "120"),
        ("display", "grid"),
        ("place-items", "stretch"),
        ("padding", "0"),
        ("overflow", "hidden"),
    ]);
    rule(css, ".topology-shell.extension-mode .dialog-backdrop", &[(
        "display", "none",
    )]);
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
}

fn append_extension_tab_content(css: &mut String) {
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
