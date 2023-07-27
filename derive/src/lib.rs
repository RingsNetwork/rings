extern crate proc_macro;
#[macro_use]
extern crate quote;
use syn::parse_macro_input;
use syn::DeriveInput;

mod derives;

use proc_macro::TokenStream;
#[cfg(feature = "wasm")]
use quote::quote;

/// If the feature is not "wasm", the macro does nothing; otherwise, it calls wasm_bindgen.
/// wasm_export does not work for Js Class. To export a class to js,
/// you should use wasm_bindgen or __wasm_bindgen_class_marker.
/// ref: <https://docs.rs/wasm-bindgen-macro/0.2.86/src/wasm_bindgen_macro/lib.rs.html#51>
#[proc_macro_attribute]
pub fn wasm_export(attr: TokenStream, input: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        std::unimplemented!("wasm_export is not ready for attribute case");
    }
    #[cfg(feature = "wasm")]
    return match wasm_bindgen_macro_support::expand(attr.into(), input.into()) {
        Ok(tokens) => tokens.into(),
        Err(diagnostic) => (quote! { #diagnostic }).into(),
    };

    #[cfg(not(feature = "wasm"))]
    return input;
}

#[proc_macro_derive(MeasureBehaviour)]
pub fn impl_measure_behaviour(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);
    crate::derives::impl_measure_behaviour_traits(&ast).into()
}

#[proc_macro_derive(JudgeConnection)]
pub fn impl_judege_connection(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);
    crate::derives::impl_judge_connection_traits(&ast).into()
}
