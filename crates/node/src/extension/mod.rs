//! This module provide basic mechanism.

pub mod ext;
pub mod protocols;
pub mod transport;
use std::result::Result;
use std::sync::Arc;

use async_trait::async_trait;
use rings_core::dht::Did;
use rings_core::message::CustomMessage;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::message::MessageVerificationExt;
use rings_core::swarm::callback::SwarmCallback;
use rings_core::swarm::callback::SwarmEvent;
use rings_transport::core::transport::WebrtcConnectionState;

use crate::extension::ext::Envelope;
use crate::extension::ext::Extensions;
use crate::extension::transport::platform::run_detached;
use crate::provider::Provider;

/// Observer of swarm facts the [`Backend`] decodes or receives but does not act on itself.
///
/// The backend decodes each inbound payload exactly once and hands the observer the decoded
/// fact synchronously, before any await, so a later callback for the same peer observes it.
pub trait BackendObserver: Send + Sync {
    /// A Chord successor lookup report addressed to this node: `successor` is the reported
    /// successor of the key queried under transaction `tx_id`.
    fn lookup_report(&self, tx_id: uuid::Uuid, successor: Did);

    /// The direct transport to `peer` reached `state`.
    fn connection_state(&self, peer: Did, state: WebrtcConnectionState);
}

/// Backend handles inbound custom messages from the Swarm, routing each decoded
/// [`Envelope`] to its namespace's protocol via the [`Extensions`] registry. The
/// registry is shared with the [`Provider`], so protocols registered there are visible
/// to inbound dispatch here. Each protocol's interpreter does its IO through a
/// namespace-scoped [`Scope`](ext::Scope); the underlying router capability is internal.
/// Dispatch owns a detached task so a swarm callback deadline stops waiting without
/// cancelling an already committed protocol transition or its ordered effect trace.
/// Core lookup reports and connection state changes are not dispatched; they are handed to
/// the optional [`BackendObserver`].
pub struct Backend {
    extensions: Extensions,
    observer: Option<Arc<dyn BackendObserver>>,
}

impl Backend {
    /// Create a new backend over a provider, sharing its protocol registry.
    pub fn new(provider: Arc<Provider>) -> Self {
        Self {
            extensions: provider.extensions(),
            observer: None,
        }
    }

    /// Attach the observer that receives decoded lookup reports and connection state changes.
    pub fn observed_by(mut self, observer: Arc<dyn BackendObserver>) -> Self {
        self.observer = Some(observer);
        self
    }
}

#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
impl SwarmCallback for Backend {
    async fn on_inbound(
        &self,
        payload: &MessagePayload,
    ) -> Result<(), rings_core::error::CallbackError> {
        let data: Message = payload.transaction.data()?;

        let msg = match data {
            Message::CustomMessage(CustomMessage(msg)) => msg,
            Message::FindSuccessorReport(report) => {
                if let Some(observer) = &self.observer {
                    observer.lookup_report(payload.transaction.tx_id, report.did);
                }
                return Ok(());
            }
            _ => return Ok(()),
        };

        let envelope = Envelope::decode(&msg)?;
        let from = payload.transaction.signer();
        let extensions = self.extensions.clone();
        let dispatch =
            run_detached(async move { extensions.dispatch(from, envelope).await }).await?;
        dispatch?;

        Ok(())
    }

    async fn on_event(&self, event: &SwarmEvent) -> Result<(), rings_core::error::CallbackError> {
        let Some(observer) = &self.observer else {
            return Ok(());
        };
        if let SwarmEvent::ConnectionStateChange { peer, state } = event {
            observer.connection_state(*peer, *state);
        }
        Ok(())
    }
}
