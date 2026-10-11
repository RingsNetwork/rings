//! This module provide basic mechanism.

pub mod ext;
pub mod protocols;
#[cfg(rings_native)]
pub mod transport;
use std::sync::Arc;

use rings_core::dht::Did;
use rings_core::message::CustomMessage;
use rings_core::swarm::application::ApplicationData;
use rings_core::swarm::application::ApplicationDelivery;
use rings_core::swarm::application::ApplicationInbox;
use rings_core::swarm::application::ApplicationMessage;
use rings_core::swarm::application::SwarmEvent;

use crate::error::Result;
use crate::extension::ext::Envelope;
use crate::extension::ext::Extensions;
use crate::provider::Provider;

/// Observer of the peer facts the [`Backend`] reads from the application inbox but does not act
/// on itself.
///
/// The backend hands the observer each fact in inbox order, before it reads the next item.
/// Facts are the swarm's logical conclusions, never a physical connection state.
pub trait BackendObserver: Send + Sync {
    /// `peer` was admitted: its transport is ready and it joined the local DHT.
    fn peer_admitted(&self, peer: Did);

    /// `peer` left the local DHT, whichever physical event or local decision caused it.
    fn peer_retired(&self, peer: Did);
}

/// Backend consumes the swarm's application inbox, routing each inbound custom message, decoded
/// as an [`Envelope`], to its namespace's protocol via the [`Extensions`] registry. The registry
/// is shared with the [`Provider`], so protocols registered there are visible to inbound dispatch
/// here. Each protocol's interpreter does its IO through a namespace-scoped
/// [`Scope`](ext::Scope); the underlying router capability is internal.
///
/// The swarm never waits on the backend: it reads at its own pace, and a slow protocol fills
/// only the inbox, which backpressures only the application data lanes (#935). Admissions and
/// retirements are handed to the optional [`BackendObserver`]; E2E messages are not routed.
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

    /// Attach the observer that receives admissions and retirements.
    pub fn observed_by(mut self, observer: Arc<dyn BackendObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Consume `inbox` until the swarm is gone. Each item is handled to completion before the
    /// next is read, so the protocols see the inbox order; a failed item is logged and skipped.
    pub async fn serve(self, inbox: ApplicationInbox) {
        while let Some(delivery) = inbox.next().await {
            if let Err(error) = self.handle(delivery).await {
                tracing::warn!(?error, "application delivery failed");
            }
        }
    }

    /// Handle one item of the application inbox.
    pub async fn handle(&self, delivery: ApplicationDelivery) -> Result<()> {
        match delivery {
            ApplicationDelivery::Message(ApplicationMessage {
                from,
                data: ApplicationData::Custom(CustomMessage(bytes)),
                ..
            }) => {
                let envelope = Envelope::decode(&bytes)?;
                self.extensions.dispatch(from, envelope).await
            }
            ApplicationDelivery::Event(event) => {
                if let Some(observer) = self.observer.as_deref() {
                    match event {
                        SwarmEvent::PeerAdmitted { peer } => observer.peer_admitted(peer),
                        SwarmEvent::PeerRetired { peer } => observer.peer_retired(peer),
                        _ => {}
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
