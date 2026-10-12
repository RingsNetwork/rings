//! This module provide basic mechanism.

mod dispatch;
pub mod ext;
pub mod protocols;
#[cfg(rings_native)]
pub mod transport;
use std::sync::Arc;

use futures::FutureExt;
use rings_core::dht::Did;
use rings_core::lifecycle::StopToken;
use rings_core::message::CustomMessage;
use rings_core::swarm::application::ApplicationData;
use rings_core::swarm::application::ApplicationDelivery;
use rings_core::swarm::application::ApplicationInbox;
use rings_core::swarm::application::ApplicationMessage;
use rings_core::swarm::application::SwarmEvent;

pub use crate::extension::dispatch::DispatchDropCounters;
pub(crate) use crate::extension::dispatch::DispatchDrops;
use crate::extension::ext::Extensions;
use crate::provider::Provider;

/// Observer of what the [`Backend`] reads from the application inbox but does not act on
/// itself: the peer facts, and the E2E messages it routes to no protocol.
///
/// The backend hands the observer each item in inbox order, before it reads the next. Facts are
/// the swarm's logical conclusions, never a physical connection state. The observer must not
/// block: it runs on the inbox's one reader.
///
/// A fact reaches the observer as soon as it is read, while the custom messages read before it
/// may still wait in their namespace's queue: facts keep their order among themselves and with
/// E2E messages, but may overtake custom messages.
pub trait BackendObserver: Send + Sync {
    /// `peer` was admitted: its transport is ready and it joined the local DHT.
    fn peer_admitted(&self, _peer: Did) {}

    /// `peer` left the local DHT, whichever physical event or local decision caused it.
    fn peer_retired(&self, _peer: Did) {}

    /// An E2E message (handshake or stream frame) addressed to this node.
    fn e2e_message(&self, _message: &ApplicationMessage) {}
}

/// Backend consumes the swarm's application inbox: it routes each inbound custom message,
/// decoded as an [`Envelope`](ext::Envelope), to its namespace's protocol via the
/// [`Extensions`] registry, and hands everything else to the optional [`BackendObserver`]. The
/// registry is shared with the [`Provider`], so protocols registered there are visible to
/// inbound dispatch here. Each protocol's interpreter does its IO through a namespace-scoped
/// [`Scope`](ext::Scope).
///
/// The swarm never waits on the backend (#935): it reads at its own pace, and a full inbox
/// drops further live messages. Each registered namespace is dispatched by its own task, in the
/// order its messages arrived, so a protocol waiting on a slow send holds back neither another
/// namespace nor the peer facts; a namespace whose queue is full drops further messages,
/// counted, as the inbox does. Facts and E2E messages go to the observer at once, so a fact can
/// overtake a custom message still queued for its namespace.
///
/// Every message the backend drops is counted in the provider's
/// [`dispatch_drop_counters`](Provider::dispatch_drop_counters).
pub struct Backend {
    extensions: Extensions,
    drops: Arc<DispatchDrops>,
    observer: Option<Arc<dyn BackendObserver>>,
}

impl Backend {
    /// Create a new backend over a provider, sharing its protocol registry.
    pub fn new(provider: Arc<Provider>) -> Self {
        Self {
            extensions: provider.extensions(),
            drops: provider.dispatch_drops(),
            observer: None,
        }
    }

    /// Attach the observer that receives peer facts and E2E messages.
    pub fn observed_by(mut self, observer: Arc<dyn BackendObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Consume `inbox` until the swarm is gone or `stop` is requested; the namespace workers it
    /// starts end with it once `stop` is requested.
    pub async fn serve(self, inbox: ApplicationInbox, stop: StopToken) {
        let mut namespaces = dispatch::NamespaceQueues::new(
            self.extensions.core(),
            Arc::clone(&self.drops),
            stop.clone(),
        );
        loop {
            let delivery = futures::select_biased! {
                () = stop.stopped().fuse() => return,
                delivery = inbox.next().fuse() => delivery,
            };
            let Some(delivery) = delivery else {
                return;
            };
            self.route(&mut namespaces, delivery);
        }
    }

    /// Route one item of the application inbox: a custom message to its namespace's queue,
    /// everything else to the observer. Never waits.
    fn route(&self, namespaces: &mut dispatch::NamespaceQueues, delivery: ApplicationDelivery) {
        match delivery {
            ApplicationDelivery::Message(ApplicationMessage {
                from,
                data: ApplicationData::Custom(CustomMessage(bytes)),
                ..
            }) => namespaces.submit(from, &bytes),
            ApplicationDelivery::Message(message) => {
                if let Some(observer) = self.observer.as_deref() {
                    observer.e2e_message(&message);
                }
            }
            ApplicationDelivery::Event(event) => {
                if let Some(observer) = self.observer.as_deref() {
                    match event {
                        SwarmEvent::PeerAdmitted { peer } => observer.peer_admitted(peer),
                        SwarmEvent::PeerRetired { peer } => observer.peer_retired(peer),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}
