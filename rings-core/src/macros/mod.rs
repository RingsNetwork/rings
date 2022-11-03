//! Macro for ring-core
//!
//! for impl recursion, we need:
//! `poll` macro help to implement await operation finish when feature using `wasm`
//!
//! # Example
//!
//! ```rust,ignore
//! # extern crate async_trait;
//! # extern crate futures;
//! # extern crate ring_core;
//! # extern crate log;
//!
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! # use async_trait::async_trait;
//! # use futures::future::FutureExt;
//! # use futures::pin_mut;
//! # use futures::select;
//! # use futures_timer::Delay;
//! # use ring_core::dht::Stabilization;
//! # use ring_core::dht::TStabilize;
//!
//! #[async_trait]
//! impl TStabilize for Stabilization {
//!     async fn wait(self: Arc<Self>) {
//!         loop {
//!             let timeout = Delay::new(Duration::from_secs(self.timeout as u64)).fuse();
//!             pin_mut!(timeout);
//!             select! {
//!                 _ = timeout => self
//!                     .stabilize()
//!                     .await
//!                     .unwrap_or_else(|e| log::error!("failed to stabilize {:?}", e)),
//!             }
//!         }
//!     }
//! }
//! ```
//!
//! Stabilize function using `futures::select` to await task is finish, but feature wasm not support
//! Using `poll` can fix this problem.
//!
//! ```rust,ignore
//! # extern crate async_trait;
//! # extern crate ring_core;
//! # extern crate wasm_bindgen_futures;
//! # extern crate log;
//!
//! # use std::sync::Arc;
//!
//! # use async_trait::async_trait;
//! # use ring_core::dht::Stabilization;
//! # use ring_core::dht::TStabilize;
//! # use ring_core::macros::poll;
//! # use wasm_bindgen_futures::spawn_local;
//!  #[async_trait(?Send)]
//!  impl TStabilize for Stabilization {
//!      async fn wait(self: Arc<Self>) {
//!          let caller = Arc::clone(&self);
//!          let func = move || {
//!             let caller = caller.clone();
//!             spawn_local(Box::pin(async move {
//!                 caller
//!                     .stabilize()
//!                     .await
//!                     .unwrap_or_else(|e| log::error!("failed to stabilize {:?}", e));
//!             }))
//!         };
//!         poll!(func, 25000);
//!     }
//!  }
//!  ```

/// poll macro use for wasm futures and wait, act like `async-await`.
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
