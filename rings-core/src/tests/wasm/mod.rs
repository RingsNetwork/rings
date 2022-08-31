mod test_channel;
mod test_ice_servers;
mod test_idb_storage;
mod test_wasm_transport;

use wasm_bindgen_test::wasm_bindgen_test_configure;

wasm_bindgen_test_configure!(run_in_browser);

pub fn setup_log() {
    console_log::init_with_level(log::Level::Trace).unwrap();
    log::debug!("test")
}
