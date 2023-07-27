pub fn impl_measure_behaviour_traits(ast: &syn::DeriveInput) -> proc_macro2::TokenStream {
    let name = &ast.ident;

    let impl_token = quote! {
    #[cfg_attr(feature = "node", async_trait)]
    #[cfg_attr(feature = "browser", async_trait(?Send))]
    impl<const T: i16> MessageSendBehaviour<T> for #name {}
    #[cfg_attr(feature = "node", async_trait)]
    #[cfg_attr(feature = "browser", async_trait(?Send))]
    impl<const T: i16> MessageRecvBehaviour<T> for #name {}
    #[cfg_attr(feature = "node", async_trait)]
    #[cfg_attr(feature = "browser", async_trait(?Send))]
    impl<const T: i16> ConnectBehaviour<T> for #name {}
    };

    #[cfg(not(feature = "core_crate"))]
    quote! {
    use rings_core::measure::measure::MessageRecvBehaviour;
    use rings_core::measure::measure::MessageSendehaviour;
    use rings_core::measure::measure::ConnectBehaviour;

    #impl_token
    }

    #[cfg(feature = "core_crate")]
    quote! {
    use crate::measure::measure::MessageRecvBehaviour;
    use crate::measure::measure::MessageSendBehaviour;
    use crate::measure::measure::ConnectBehaviour;

    #impl_token
    }
}

pub fn impl_judge_connection_traits(ast: &syn::DeriveInput) -> proc_macro2::TokenStream {
    let name = &ast.ident;
    #[cfg(feature = "core_crate")]
    quote! {
    use crate::transports::manager::JudgeConnection;

    #[cfg_attr(feature = "node", async_trait)]
    #[cfg_attr(feature = "browser", async_trait(?Send))]
    impl JudgeConnection for #name {}
    }
    #[cfg(not(feature = "core_crate"))]
    quote! {
    use rings_core::transports::manager::JudgeConnection;

    #[cfg_attr(feature = "node", async_trait)]
    #[cfg_attr(feature = "browser", async_trait(?Send))]
    impl JudgeConnection for #name {}
    }
}
