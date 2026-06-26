//! Macro for ring-core
//!
//! `poll` schedules a future expression repeatedly on wasm runtimes.
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
//! # extern crate log;
//!
//! # use std::sync::Arc;
//!
//! # use async_trait::async_trait;
//! # use ring_core::dht::Stabilization;
//! # use ring_core::dht::TStabilize;
//! # use ring_core::poll;
//!  #[async_trait(?Send)]
//!  impl TStabilize for Stabilization {
//!      async fn wait(self: Arc<Self>) {
//!          let caller = Arc::clone(&self);
//!          poll!(
//!              {
//!                  let caller = Arc::clone(&caller);
//!                  async move {
//!                      caller
//!                          .stabilize()
//!                          .await
//!                          .unwrap_or_else(|e| log::error!("failed to stabilize {:?}", e));
//!                  }
//!              },
//!              25000
//!          );
//!     }
//!  }
//!  ```

/// Schedule a future expression repeatedly with a wasm timeout.
#[macro_export]
macro_rules! poll {
    ( $future:expr, $ttl:expr ) => {{
        $crate::utils::js_utils::spawn_interval($ttl, move || $future);
    }};
}
