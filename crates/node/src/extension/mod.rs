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
use rings_core::message::PacedLane;
use rings_core::swarm::callback::PeerTransition;
use rings_core::swarm::callback::SwarmCallback;
use rings_core::swarm::callback::SwarmEvent;
use rings_runtime::run_detached;

use crate::extension::ext::Envelope;
use crate::extension::ext::Extensions;
use crate::provider::Provider;

/// Observer of swarm facts the [`Backend`] decodes or receives but does not act on itself.
///
/// The backend hands the observer each fact synchronously, before any await, so a later
/// callback for the same peer observes it. Facts are the swarm's logical conclusions, never a
/// physical connection state: the backend is the one place that translates.
pub trait BackendObserver: Send + Sync {
    /// A successor lookup report addressed to this node for a lookup that requested no core
    /// action (`FindSuccessorReportHandler::None`), i.e. an application-issued lookup:
    /// `successor` is the reported successor of the key queried under transaction `tx_id`.
    fn lookup_report(&self, tx_id: uuid::Uuid, successor: Did);

    /// `peer` was admitted: its transport is ready and it joined the local DHT.
    fn peer_admitted(&self, peer: Did);

    /// `peer` left the local DHT, whichever physical event or local decision caused it.
    fn peer_retired(&self, peer: Did);
}

/// Backend handles inbound custom messages from the Swarm, routing each decoded
/// [`Envelope`] to its namespace's protocol via the [`Extensions`] registry. The
/// registry is shared with the [`Provider`], so protocols registered there are visible
/// to inbound dispatch here. Each protocol's interpreter does its IO through a
/// namespace-scoped [`Scope`](ext::Scope); the underlying router capability is internal.
/// Dispatch owns a detached task so a swarm callback deadline stops waiting without
/// cancelling an already committed protocol transition or its ordered effect trace.
/// Core lookup reports, admissions and retirements are not dispatched; they are handed to the
/// optional [`BackendObserver`].
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

    /// Attach the observer that receives application lookup reports, admissions and retirements.
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
            Message::FindSuccessorReport(report) if report.is_application_lookup() => {
                if let Some(observer) = self.observer.as_deref() {
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

    /// Name the paced direct-edge lane of an inbound envelope from the registry, so a
    /// protocol's declared rate (see [`ext::Protocol::paced_direct_rate`]) reaches core's
    /// admission. Core alone decides whether the lane applies to the delivering edge.
    fn paced_lane(&self, application_payload: &[u8]) -> Option<PacedLane> {
        self.extensions.paced_lane(application_payload)
    }

    /// Translate the swarm's events into the observer's facts: an admission and a retirement.
    async fn on_event(&self, event: &SwarmEvent) -> Result<(), rings_core::error::CallbackError> {
        let Some(observer) = self.observer.as_deref() else {
            return Ok(());
        };
        match event.peer_transition() {
            Some((peer, PeerTransition::Admitted)) => observer.peer_admitted(peer),
            Some((peer, PeerTransition::Retired)) => observer.peer_retired(peer),
            None => {}
        }
        Ok(())
    }
}
