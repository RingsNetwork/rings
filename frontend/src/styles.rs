//! CSS bundle for the Yew app.

use std::sync::OnceLock;

mod theme;

const STRUCTURAL_CSS: &str = concat!(
    include_str!("styles/base.css"),
    "\n",
    include_str!("styles/layout.css"),
    "\n",
    include_str!("styles/components.css"),
    "\n",
    include_str!("styles/dialogs.css"),
    "\n",
    include_str!("styles/features.css"),
    "\n",
    include_str!("styles/responsive.css"),
);

/// Returns the complete stylesheet.
///
/// Static CSS files keep layout mechanics close to the DOM they style. Theme-level
/// color, surface, and state rules are generated from Rust so landing and console
/// stay visually aligned without scattering long override blocks across CSS files.
pub fn app_css() -> &'static str {
    static APP_CSS: OnceLock<String> = OnceLock::new();
    APP_CSS
        .get_or_init(|| {
            let mut css = String::with_capacity(STRUCTURAL_CSS.len() + theme::THEME_CSS_CAPACITY);
            css.push_str(STRUCTURAL_CSS);
            theme::append(&mut css);
            css
        })
        .as_str()
}
