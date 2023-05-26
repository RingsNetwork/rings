extern crate proc_macro;
use proc_macro::TokenStream;

/// If the feature is not "wasm", the macro does nothing; otherwise, it calls wasm_bindgen.
/// wasm_export does not work for Js Class. To export a class to js,
/// you should use wasm_bindgen or __wasm_bindgen_class_marker.
/// ref: https://docs.rs/wasm-bindgen-macro/0.2.86/src/wasm_bindgen_macro/lib.rs.html#51
#[proc_macro_attribute]
pub fn wasm_export(_attr: TokenStream, input: TokenStream) -> proc_macro::TokenStream {
    #[cfg(features = "wasm")]
    return rings_core::wasm_bindgen::prelude::wasm_bindgen(_attr, input);
    #[cfg(not(features = "wasm"))]
    return input;
}
