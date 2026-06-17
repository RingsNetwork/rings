#![warn(missing_docs)]
//! Compute services — the effectful escape hatch: registered impure jobs run by the
//! shell when an [`Effect::Compute`](super::Effect::Compute) is interpreted.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use bytes::Bytes;
#[cfg(not(feature = "browser"))]
use futures::future::BoxFuture;
#[cfg(feature = "browser")]
use futures::future::LocalBoxFuture;

use crate::error::Error;
use crate::error::Result;

/// An impure compute job for a namespace: `input ↦ IO output`. Runs in the shell
/// when an [`Effect::Compute`](super::Effect::Compute) is interpreted; its `output` is
/// re-injected as a self-event. Used by protocols whose work is genuinely non-pure (e.g.
/// SNARK proving), keeping their `step` pure. `Send + Sync` natively, neither in browser.
#[cfg(not(feature = "browser"))]
pub type ComputeFn = Arc<dyn Fn(Bytes) -> BoxFuture<'static, Result<Bytes>> + Send + Sync>;
/// An impure compute job for a namespace: `input ↦ IO output`.
#[cfg(feature = "browser")]
pub type ComputeFn = Arc<dyn Fn(Bytes) -> LocalBoxFuture<'static, Result<Bytes>>>;

/// Registry of [`ComputeFn`]s keyed by namespace. Shared (interior mutability) so a
/// protocol registered on the [`Provider`](crate::provider::Provider) and the
/// [`Interpreter`](super::Interpreter) that runs its effects see the same jobs.
#[derive(Default, Clone)]
pub struct ComputeServices {
    jobs: Arc<RwLock<HashMap<String, ComputeFn>>>,
}

impl ComputeServices {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the compute job for `namespace`. Fails if the lock is poisoned.
    pub fn register(&self, namespace: impl Into<String>, job: ComputeFn) -> Result<()> {
        let mut jobs = self.jobs.write().map_err(|_| Error::Lock)?;
        jobs.insert(namespace.into(), job);
        Ok(())
    }

    pub(super) fn get(&self, namespace: &str) -> Option<ComputeFn> {
        self.jobs.read().ok()?.get(namespace).map(Arc::clone)
    }
}
