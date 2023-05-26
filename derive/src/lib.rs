extern crate proc_macro;
use proc_macro::TokenStream;

/// If the feature is "wasm", the macro does nothing; otherwise, it calls `wasm_bindgen`.
#[proc_macro_attribute]
pub fn wasm_export(_attr: TokenStream, _input: TokenStream) -> TokenStream {
    #[cfg(features = "wasm")]
    {
	use rings_core::wasm_bindgen::prelude::*;
        return wasm_bindgen(_attr, _input);
    }

    {
        return _input;
    }
}
