//! CSS bundle for the Yew app.
//!
//! Styles stay in CSS modules, while Rust only owns deterministic inclusion.

pub const APP_CSS: &str = concat!(
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
