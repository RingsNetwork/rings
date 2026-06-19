#![warn(missing_docs)]
//! This module provide basic mechanism.

pub mod ext;
pub mod protocols;
#[cfg(feature = "snark")]
pub mod snark;
pub mod transport;
pub mod types;
use std::result::Result;
use std::sync::Arc;

use async_trait::async_trait;
use rings_core::message::CustomMessage;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::message::MessageVerificationExt;
use rings_core::swarm::callback::SwarmCallback;

use crate::extension::ext::Envelope;
use crate::extension::ext::Extensions;
use crate::provider::Provider;

/// Backend handles inbound custom messages from the Swarm, routing each decoded
/// [`Envelope`] to its namespace's protocol via the [`Extensions`] registry. The
/// registry is shared with the [`Provider`], so protocols registered there are visible
/// to inbound dispatch here. Each protocol's interpreter does its IO through a
/// namespace-scoped [`Scope`](ext::Scope); the underlying router capability is internal.
pub struct Backend {
    extensions: Extensions,
}

impl Backend {
    /// Create a new backend over a provider, sharing its protocol registry.
    pub fn new(provider: Arc<Provider>) -> Self {
        Self {
            extensions: provider.extensions(),
        }
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl SwarmCallback for Backend {
    async fn on_inbound(&self, payload: &MessagePayload) -> Result<(), Box<dyn std::error::Error>> {
        let data: Message = payload.transaction.data()?;

        let Message::CustomMessage(CustomMessage(msg)) = data else {
            return Ok(());
        };

        let envelope = Envelope::decode(&msg)?;
        let from = payload.transaction.signer();
        self.extensions.dispatch(from, envelope).await?;

        Ok(())
    }
}
