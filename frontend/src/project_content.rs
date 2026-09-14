//! Project copy shared by the initial document and the interactive landing page.
//!
//! The initial HTML is the single source of truth. Cargo generates these constants from its
//! marked, visible text before compiling the Yew application.

include!(concat!(env!("OUT_DIR"), "/project_content.rs"));
