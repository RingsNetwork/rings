//! Procedural macros shared by Rings crates.

extern crate proc_macro;
use proc_macro::TokenStream;

/// If the feature is not "wasm", the macro does nothing; otherwise, it calls wasm_bindgen.
/// wasm_export does not work for Js Class. To export a class to js,
/// you should use wasm_bindgen or __wasm_bindgen_class_marker.
/// ref: <https://docs.rs/wasm-bindgen-macro/0.2.86/src/wasm_bindgen_macro/lib.rs.html#51>
#[proc_macro_attribute]
pub fn wasm_export(attr: TokenStream, input: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "wasm_export does not support attribute arguments",
        )
        .to_compile_error()
        .into();
    }
    #[cfg(feature = "wasm")]
    {
        let input: proc_macro2::TokenStream = input.into();
        quote::quote! {
            #[cfg_attr(target_family = "wasm", wasm_bindgen::prelude::wasm_bindgen)]
            #input
        }
        .into()
    }

    #[cfg(not(feature = "wasm"))]
    return input;
}
