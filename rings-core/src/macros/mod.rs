//! Macro for ring-core
//!
//! for impl recursion, we need:
//! ```
//! func = fn(func: Function) {
//!     poll();
//!     set_timeout(func, timeout, func);
//! }
//! set_timeout(func, timeout, func)
//! ```

/// `poll` macro help to implement await operation finish when feature using `wasm`
///
/// # Example
///
/// ```rust,no_run
/// 
/// use std::sync::Arc;
/// use std::time::Duration;

/// use async_trait::async_trait;
/// use futures::future::FutureExt;
/// use futures::pin_mut;
/// use futures::select;
/// use futures_timer::Delay;
/// use crate::dht::Stabilization;
/// use crate::dht::TStabilize;
///
/// #[async_trait]
/// impl TStabilize for Stabilization {
///     async fn wait(self: Arc<Self>) {
///         loop {
///             let timeout = Delay::new(Duration::from_secs(self.timeout as u64)).fuse();
///             pin_mut!(timeout);
///             select! {
///                 _ = timeout => self
///                     .stabilize()
///                     .await
///                     .unwrap_or_else(|e| log::error!("failed to stabilize {:?}", e)),
///             }
///         }
///     }
/// }
///
/// ```
/// Stabilize function using `futures::select` to await task is finish, but feature wasm not support
/// Using `poll` can fix this problem.
/// ```rust,no_run
/// use std::sync::Arc;

/// use async_trait::async_trait;
/// use wasm_bindgen_futures::spawn_local;

/// use super::Stabilization;
/// use super::TStabilize;
/// use crate::poll;

/// #[async_trait(?Send)]
/// impl TStabilize for Stabilization {
///     async fn wait(self: Arc<Self>) {
///         let caller = Arc::clone(&self);
///         let func = move || {
///             let caller = caller.clone();
///             spawn_local(Box::pin(async move {
///                 caller
///                     .stabilize()
///                     .await
///                     .unwrap_or_else(|e| log::error!("failed to stabilize {:?}", e));
///             }))
///         };
///         poll!(func, 25000);
///     }
/// }
/// ```

#[macro_export]
macro_rules! poll {
    ( $func:expr, $ttl:expr ) => {{
        use wasm_bindgen::JsCast;
        let window = web_sys::window().unwrap();
        let func = wasm_bindgen::prelude::Closure::wrap(
            (box move |func: js_sys::Function| {
                $func();
                let window = web_sys::window().unwrap();
                window
                    .set_timeout_with_callback_and_timeout_and_arguments(
                        func.unchecked_ref(),
                        $ttl,
                        &js_sys::Array::of1(&func),
                    )
                    .unwrap();
            }) as Box<dyn FnMut(js_sys::Function)>,
        );
        window
            .set_timeout_with_callback_and_timeout_and_arguments(
                &func.as_ref().unchecked_ref(),
                $ttl,
                &js_sys::Array::of1(&func.as_ref().unchecked_ref()),
            )
            .unwrap();
        func.forget();
    }};
}
