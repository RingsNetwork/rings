use wasm_bindgen::prelude::*;

/// Helper macro which acts like `println!` only routes to `console.log`
/// instead.
#[macro_export]
macro_rules! console_log {
    ($($t:tt)*) => ($crate::console::log(&format_args!($($t)*).to_string()))
}

#[macro_export]
macro_rules! console_err {
    ($($t:tt)*) => ($crate::console::error(&format_args!($($t)*).to_string()))
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    pub fn log(a: &str);

    #[wasm_bindgen(js_namespace = console)]
    pub fn error(a: &str);
}
