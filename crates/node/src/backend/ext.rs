#![warn(missing_docs)]
//! Unified extension/protocol abstraction shared by `native` and `browser`.
//!
//! This is the seam that replaces the closed `BackendMessage` enum and the two
//! divergent dispatch paradigms. A protocol is an [`Extension`]; the node routes
//! inbound [`Envelope`]s to extensions by `namespace`; an extension talks back to the
//! node only through the [`Ctx`] capability handle.
//!
//! The abstraction, structure and constraints are identical on both targets; the only
//! divergence is the `Send` / `?Send` bound on async trait methods (browser futures
//! are not `Send`), expressed with the same `cfg_attr` pair used throughout the crate.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;

/// Namespaced message envelope carried over the P2P transport (bincode), in place of
/// the old closed `BackendMessage` enum.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    /// Protocol namespace this payload belongs to.
    pub namespace: String,
    /// Opaque protocol payload; the inner codec is the extension's own business.
    pub payload: Bytes,
}

impl Envelope {
    /// Build an envelope from a namespace and payload.
    pub fn new(namespace: impl Into<String>, payload: Bytes) -> Self {
        Self {
            namespace: namespace.into(),
            payload,
        }
    }

    /// Encode the envelope for the P2P transport.
    pub fn encode(&self) -> Result<Vec<u8>> {
        bincode::serialize(self).map_err(|_| Error::EncodeError)
    }

    /// Decode an envelope received from the P2P transport.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bincode::deserialize(bytes).map_err(|_| Error::DecodeError)
    }
}

/// Capability handle handed to an extension: the bounded set of node operations an
/// extension may perform. Extensions never see `Provider`/`Processor` directly.
#[derive(Clone)]
pub struct Ctx {
    processor: Arc<Processor>,
}

impl Ctx {
    /// Build a ctx over a processor.
    pub fn new(processor: Arc<Processor>) -> Self {
        Self { processor }
    }

    /// This node's DID.
    pub fn did(&self) -> Did {
        self.processor.did()
    }

    /// Send a namespaced payload to a peer.
    pub async fn send(&self, to: Did, namespace: &str, payload: Bytes) -> Result<()> {
        let bytes = Envelope::new(namespace, payload).encode()?;
        self.processor.send_message(to, &bytes).await?;
        Ok(())
    }
}

/// `Arc<dyn Extension>` with the target-appropriate auto-trait bounds.
#[cfg(not(feature = "browser"))]
pub type DynExtension = dyn Extension + Send + Sync;
/// `Arc<dyn Extension>` with the target-appropriate auto-trait bounds.
#[cfg(feature = "browser")]
pub type DynExtension = dyn Extension;

/// A protocol extension. Same trait and bounds on both targets.
#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
pub trait Extension {
    /// The protocol namespace this extension handles.
    fn namespace(&self) -> &str;

    /// Handle an inbound payload addressed to this extension's namespace.
    async fn on_message(&self, ctx: &Ctx, from: Did, payload: Bytes) -> Result<()>;
}

/// Registry that routes inbound envelopes to extensions by namespace.
/// Registry that routes inbound envelopes to extensions by namespace.
///
/// Cheaply cloneable and shared (interior mutability): the [`Provider`] owns one and
/// hands clones to the inbound callback, so registration and dispatch see the same
/// table.
///
/// [`Provider`]: crate::provider::Provider
#[derive(Default, Clone)]
pub struct Extensions {
    handlers: Arc<RwLock<HashMap<String, Arc<DynExtension>>>>,
}

impl Extensions {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an extension under its declared namespace.
    pub fn register(&self, ext: Arc<DynExtension>) {
        if let Ok(mut handlers) = self.handlers.write() {
            handlers.insert(ext.namespace().to_string(), ext);
        }
    }

    /// Whether a namespace has a registered extension.
    pub fn contains(&self, namespace: &str) -> bool {
        self.handlers
            .read()
            .map(|h| h.contains_key(namespace))
            .unwrap_or(false)
    }

    /// Look up an extension by namespace (clones the `Arc` out so the lock is not
    /// held across the async handler).
    fn get(&self, namespace: &str) -> Option<Arc<DynExtension>> {
        self.handlers.read().ok()?.get(namespace).cloned()
    }

    /// Route a decoded envelope to its namespace's extension.
    ///
    /// Unknown namespaces are logged and dropped (non-fatal): a peer speaking a
    /// protocol this node does not have is expected.
    pub async fn dispatch(&self, ctx: &Ctx, from: Did, envelope: Envelope) -> Result<()> {
        match self.get(&envelope.namespace) {
            Some(ext) => ext.on_message(ctx, from, envelope.payload).await,
            None => {
                tracing::debug!(
                    "no extension registered for namespace {:?}, dropping",
                    envelope.namespace
                );
                Ok(())
            }
        }
    }
}
