#![warn(missing_docs)]
//! Plain-text protocol extension.
//!
//! Reference port of the old `BackendMessage::PlainText` variant onto the unified
//! [`Extension`] abstraction. Payload is the UTF-8 message bytes.

use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;

use crate::backend::ext::Ctx;
use crate::backend::ext::DynExtension;
use crate::backend::ext::Extension;
use crate::error::Result;

/// Namespace for the plain-text protocol.
pub const NAMESPACE: &str = "plaintext";

/// Plain-text protocol extension.
#[derive(Default)]
pub struct PlainText;

impl PlainText {
    /// Build the extension as a registry-ready trait object.
    pub fn new() -> Arc<DynExtension> {
        Arc::new(Self)
    }
}

#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
impl Extension for PlainText {
    fn namespace(&self) -> &str {
        NAMESPACE
    }

    async fn on_message(&self, _ctx: &Ctx, from: Did, payload: Bytes) -> Result<()> {
        let text = String::from_utf8_lossy(&payload);
        tracing::info!("plaintext from {from:?}: {text}");
        Ok(())
    }
}
