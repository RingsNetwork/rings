// pub mod browser;
pub mod processor;

use wasm_bindgen_test::wasm_bindgen_test_configure;

wasm_bindgen_test_configure!(run_in_browser);

pub fn setup_log() {
    console_log::init_with_level(log::Level::Trace).unwrap();
    log::debug!("test")
}
