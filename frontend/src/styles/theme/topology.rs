use super::rule;
use super::Theme;

pub(super) fn append(css: &mut String, theme: Theme) {
    append_warm_topology_rules(css, ".topology-shell .topology", theme);
    rule(css, ".topology-shell .topology,.topology-shell .chord-topology", &[("border-color", theme.line_strong), ("background", "radial-gradient(circle at 50% 50%, rgba(180, 35, 24, 0.05), transparent 18%), linear-gradient(90deg, rgba(122, 87, 46, 0.055) 1px, transparent 1px), linear-gradient(180deg, rgba(122, 87, 46, 0.045) 1px, transparent 1px), rgba(255, 246, 230, 0.34)"), ("background-size", "auto, 28px 28px, 28px 28px, auto"), ("box-shadow", "inset 0 0 34px rgba(112, 84, 48, 0.04)")]);
    rule(css, ".topology-shell .topology .ring-peer-label text", &[(
        "stroke",
        "rgba(255, 250, 240, 0.92)",
    )]);
}

fn append_warm_topology_rules(css: &mut String, selector: &str, theme: Theme) {
    rule(css, &format!("{selector} .orbit"), &[(
        "stroke",
        "rgba(122, 87, 46, 0.42)",
    )]);
    rule(
        css,
        &format!("{selector} .orbit.outer,{selector} .orbit.inner"),
        &[("stroke", "rgba(122, 87, 46, 0.22)")],
    );
    rule(css, &format!("{selector} .scan"), &[(
        "stroke",
        "rgba(180, 35, 24, 0.34)",
    )]);
    rule(
        css,
        &format!("{selector} .ring-edge,{selector} .finger-link"),
        &[("stroke", "rgba(122, 87, 46, 0.42)")],
    );
    rule(css, &format!("{selector} .ring-flow"), &[(
        "stroke",
        "rgba(15, 118, 110, 0.58)",
    )]);
    rule(css, &format!("{selector} .finger-flow"), &[(
        "stroke",
        "rgba(180, 35, 24, 0.48)",
    )]);
    rule(css, &format!("{selector} .id-space-core"), &[
        ("fill", theme.page_alt),
        ("stroke", "rgba(122, 87, 46, 0.42)"),
    ]);
    rule(css, &format!("{selector} .core-label"), &[(
        "fill", "#231f1a",
    )]);
    rule(css, &format!("{selector} .ring-node"), &[(
        "filter", "none",
    )]);
    rule(css, &format!("{selector} .peer-node"), &[
        ("fill", theme.panel),
        ("stroke", "rgba(122, 87, 46, 0.72)"),
    ]);
    rule(css, &format!("{selector} .peer-node.connected"), &[
        ("fill", "#dff4ed"),
        ("stroke", "rgba(15, 118, 110, 0.78)"),
    ]);
    rule(css, &format!("{selector} .local-node"), &[
        ("fill", "#f6d8d2"),
        ("stroke", "rgba(180, 35, 24, 0.72)"),
    ]);
    rule(css, &format!("{selector} .active-node-halo"), &[(
        "stroke",
        theme.accent,
    )]);
    rule(css, &format!("{selector} .active-node-pointer"), &[(
        "stroke",
        theme.accent,
    )]);
    rule(css, &format!("{selector} .active-node-frame"), &[
        ("fill", theme.panel),
        ("stroke", theme.line_strong),
    ]);
    rule(css, &format!("{selector} .active-node-caption"), &[(
        "fill",
        theme.accent,
    )]);
    rule(css, &format!("{selector} .active-node-id"), &[(
        "fill", theme.ink,
    )]);
    rule(css, &format!("{selector} .node-label,{selector} .peer-index,{selector} .local-id,{selector} .successor-label text"), &[("fill", theme.accent)]);
    rule(css, &format!("{selector} .node-id,{selector} .empty-node-label,{selector} .topology-count,{selector} .ring-zero,{selector} .core-sub,{selector} .core-hint"), &[("fill", theme.muted)]);
    rule(css, &format!("{selector} .topology-mode"), &[(
        "fill", theme.ink,
    )]);
    rule(css, &format!("{selector} .predecessor-label text"), &[(
        "fill",
        theme.amber,
    )]);
}
