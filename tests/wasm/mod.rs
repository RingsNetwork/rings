pub mod browser;
pub mod processor;

use rings_node::logging::browser::init_logging;
use wasm_bindgen_test::wasm_bindgen_test_configure;

wasm_bindgen_test_configure!(run_in_browser);

pub fn setup_log() {
    init_logging(tracing::Level::TRACE);
    tracing::debug!("test")
}
