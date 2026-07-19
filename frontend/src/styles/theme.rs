use std::fmt::Write as _;

pub(super) const THEME_CSS_CAPACITY: usize = 24_000;

mod console;
mod dialogs;
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
    console::append(css, WARM);
    topology::append(css, WARM);
    dialogs::append(css, WARM);
    responsive::append(css);
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
