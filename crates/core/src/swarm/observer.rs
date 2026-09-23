//! Bounded-observability boundary for swarm activity.
//!
//! The core reports semantic events through [`SwarmObserver`] without choosing a storage,
//! retention, or export format. Implementations must keep callbacks synchronous and bounded:
//! transport delivery and inbound dispatch invoke them on protocol paths that must not wait for
//! operator telemetry.

use std::sync::Arc;

use crate::dht::Did;
use crate::message::MessageCategory;

/// One operator-visible logical message action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageActivity {
    /// A locally originated logical message was delivered to its next hop.
    Sent,
    /// A logical message was accepted from a transport or the local relay inbox.
    Received,
    /// A message originated elsewhere and was delivered to its next hop.
    Forwarded,
    /// A message for an offline destination was retained in a relay inbox.
    Stored,
}

/// Final local outcome of an observed operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationOutcome {
    /// The local operation completed successfully.
    Succeeded,
    /// The local operation failed or was cancelled before success.
    Failed,
}

/// Privacy-safe description of one message operation.
///
/// The record deliberately excludes payload bytes, DIDs, transaction identifiers, delegation
/// material, and transport addresses. `message_class` is a compile-time protocol variant name,
/// so an observer cannot create attacker-controlled label cardinality from it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageObservation {
    /// Semantic operation performed by this node.
    pub activity: MessageActivity,
    /// Stable, finite scheduling category of the message.
    pub category: MessageCategory,
    /// Stable protocol variant name compiled into the node.
    pub message_class: &'static str,
    /// Local completion result.
    pub outcome: ObservationOutcome,
}

/// Kind of DHT lookup represented by one lifecycle event.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LookupKind {
    /// Chord successor lookup used by application or topology maintenance.
    Successor,
    /// DHT entry lookup across one or more storage placements.
    Storage,
}

/// Internal correlation key for a DHT lookup.
///
/// Correlation keys are passed only to the observer. Exporters must aggregate or redact them;
/// using them as metric labels would create unbounded cardinality.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum LookupCorrelation {
    /// Transaction identifier shared by a successor request and its report.
    Transaction(uuid::Uuid),
    /// Resource key shared by a storage lookup and its response.
    StorageResource(Did),
}

/// Terminal result of one DHT lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LookupOutcome {
    /// The lookup produced a local or authenticated remote answer.
    Succeeded,
    /// The lookup failed before an answer was accepted.
    Failed,
}

/// Non-blocking observation hooks for swarm activity.
///
/// Every method has a no-op default so embedders retain source compatibility. Implementations
/// must not perform IO, wait on asynchronous work, or retain unbounded identifiers.
pub trait SwarmObserver {
    /// Observe the completion of one logical message operation.
    fn observe_message(&self, _observation: MessageObservation) {}

    /// Observe the beginning of one lookup lifecycle.
    fn lookup_started(&self, _kind: LookupKind, _correlation: LookupCorrelation) {}

    /// Observe the terminal result of one lookup lifecycle.
    fn lookup_finished(
        &self,
        _kind: LookupKind,
        _correlation: LookupCorrelation,
        _outcome: LookupOutcome,
    ) {
    }
}

/// Shared observer accepted by native and browser swarm builders.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub type SharedSwarmObserver = Arc<dyn SwarmObserver>;

/// Shared observer accepted by native swarm builders.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub type SharedSwarmObserver = Arc<dyn SwarmObserver + Send + Sync>;

/// Observer used when an embedder does not configure operational telemetry.
pub(crate) struct NoopSwarmObserver;

impl SwarmObserver for NoopSwarmObserver {}
