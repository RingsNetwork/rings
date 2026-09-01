pub fn impl_judge_connection_traits(ast: &syn::DeriveInput) -> proc_macro2::TokenStream {
    let name = &ast.ident;
    #[cfg(feature = "core_crate")]
    quote! {
    use crate::swarm::impls::JudgeConnection;

    #[cfg_attr(feature = "node", async_trait)]
    #[cfg_attr(feature = "browser", async_trait(?Send))]
    impl JudgeConnection for #name {}
    }
    #[cfg(not(feature = "core_crate"))]
    quote! {
    use rings_core::swarm::impls::JudgeConnection;

    #[cfg_attr(feature = "node", async_trait)]
    #[cfg_attr(feature = "browser", async_trait(?Send))]
    impl JudgeConnection for #name {}
    }
}
