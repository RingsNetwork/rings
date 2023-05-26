extern crate proc_macro;
use proc_macro::TokenStream;

/// A proc macro that do nothing
#[proc_macro_attribute]
pub fn wasm_export(_attr: TokenStream, input: TokenStream) -> TokenStream {

    #[cfg(features="wasm")]
    {
	return rings_core::prelude::wasm_bindgen::wasm_bindgen(_attr, input);
    }

    {
	return input;
    }
}
