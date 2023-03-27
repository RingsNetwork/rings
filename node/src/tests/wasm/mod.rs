pub mod browser;
pub mod processor;

use wasm_bindgen_test::wasm_bindgen_test_configure;

use crate::logging::browser::init_logging;

wasm_bindgen_test_configure!(run_in_browser);

pub fn setup_log() {
    init_logging(tracing::Level::TRACE);
    tracing::debug!("test")
}
