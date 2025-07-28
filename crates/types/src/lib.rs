//! Lib for common types

/// Common trait of async provider
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait AsyncProvider<E>: Clone {
    /// handle message
    async fn request<T>(
        &self,
        method: rings_rpc::method::Method,
        params: T,
    ) -> Result<serde_json::Value, E>
    where
        T: serde::Serialize + Send;

    /// Listen messages
    async fn listen(&self);
}

#[cfg(feature = "websys")]
pub mod websys {
    use wasm_bindgen::JsValue;
    /// Async provider trait
    pub trait JsProvider {
        /// Request local interface
        fn request(&self, method: String, params: JsValue) -> js_sys::Promise;

        /// Listen messages
        fn listen(&self) -> js_sys::Promise;
    }
}
