//! Activity sources and the quiescence predicate of core's test nodes, native and browser.
//!
//! The activity cell and the activity-woken probe live in [`rings_test_support::activity`];
//! this module wires core's test nodes to it. A node's observer ([`LedgerObserver`]) and callback
//! ([`ActivityCallback`]) record activity on every message delivered, received, handled or
//! stored, on every lookup event and connection event, and count wire messages for the
//! conservation clause of [`swarms_quiescent`].

use std::future::Future;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
pub(crate) use rings_test_support::activity::activity_after;
pub(crate) use rings_test_support::activity::activity_mark;
pub(crate) use rings_test_support::activity::record_activity;

use crate::dht::Did;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::callback::SwarmEvent;
use crate::swarm::observer::LookupCorrelation;
use crate::swarm::observer::LookupKind;
use crate::swarm::observer::LookupOutcome;
use crate::swarm::observer::MessageActivity;
use crate::swarm::observer::MessageObservation;
use crate::swarm::observer::ObservationOutcome;
use crate::swarm::observer::SwarmObserver;
use crate::swarm::Swarm;

/// [`rings_test_support::activity::probe_on_activity`] with core's error type.
pub(crate) async fn probe_on_activity<T, F>(
    label: &str,
    hang_guard: Duration,
    probe: impl FnMut() -> F,
) -> crate::error::Result<T>
where
    F: Future<Output = crate::error::Result<Option<T>>>,
{
    rings_test_support::activity::probe_on_activity(label, hang_guard, probe).await
}

/// Wire-message conservation counts of one test node.
///
/// `delivered` counts logical messages this node delivered to a next hop (sent or forwarded,
/// successfully). `received` counts logical messages that reached this node from another node;
/// the node's test callback records them. Local self-deliveries appear in neither. Summed over
/// every node of a test, `Σ delivered − Σ received` is the number of messages still between
/// nodes.
#[derive(Default)]
pub struct MessageLedger {
    delivered: AtomicU64,
    received: AtomicU64,
}

impl MessageLedger {
    /// Messages this node delivered to a next hop.
    pub(crate) fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::Acquire)
    }

    /// Messages that reached this node from another node.
    pub(crate) fn received(&self) -> u64 {
        self.received.load(Ordering::Acquire)
    }

    /// Count one message that reached this node from another node.
    pub(crate) fn record_received(&self) {
        self.received.fetch_add(1, Ordering::AcqRel);
    }
}

/// Observer of one test node: counts delivered messages and records activity.
pub(crate) struct LedgerObserver {
    ledger: Arc<MessageLedger>,
}

impl LedgerObserver {
    /// An observer counting into `ledger`.
    pub(crate) fn new(ledger: Arc<MessageLedger>) -> Self {
        Self { ledger }
    }
}

impl SwarmObserver for LedgerObserver {
    fn observe_message(&self, observation: MessageObservation) {
        if matches!(
            (observation.activity, observation.outcome),
            (
                MessageActivity::Sent | MessageActivity::Forwarded,
                ObservationOutcome::Succeeded
            )
        ) {
            self.ledger.delivered.fetch_add(1, Ordering::AcqRel);
        }
        record_activity();
    }

    fn lookup_started(&self, _kind: LookupKind, _correlation: LookupCorrelation) {
        record_activity();
    }

    fn lookup_finished(
        &self,
        _kind: LookupKind,
        _correlation: LookupCorrelation,
        _outcome: LookupOutcome,
    ) {
        record_activity();
    }
}

/// Callback of a test swarm without an inbox: counts wire receptions and records activity.
pub(crate) struct ActivityCallback {
    /// This swarm's DID, to tell a message from another node from a local self-delivery.
    local: Did,
    ledger: Arc<MessageLedger>,
}

impl ActivityCallback {
    /// A callback for the swarm `local`, counting into `ledger`.
    pub(crate) fn new(local: Did, ledger: Arc<MessageLedger>) -> Self {
        Self { local, ledger }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl SwarmCallback for ActivityCallback {
    async fn on_validate(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        if payload.signer() != self.local {
            self.ledger.record_received();
        }
        record_activity();
        Ok(())
    }

    async fn on_inbound(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        // The message has been handled; the state it changed is now observable.
        record_activity();
        Ok(())
    }

    async fn on_event(
        &self,
        _event: &SwarmEvent,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        record_activity();
        Ok(())
    }
}

/// Whether `swarm` has work in flight: a handshake, an admitted inbound message not yet
/// handled, or an outbound transfer not yet completed.
pub(crate) fn swarm_in_flight(swarm: &Swarm) -> bool {
    swarm
        .transport
        .pending_connection_count()
        .unwrap_or_default()
        > 0
        || swarm.transport.inbound_admitted_count_for_test() > 0
        || swarm.transport.outbound_admitted_transfer_total_for_test() > 0
}

/// Whether `nodes` are quiescent: nothing in flight on any of them, and nothing between them.
///
/// ```text
/// quiescent ≡ ∀ n. ¬in_flight(n)  ∧  Σ delivered(n) = Σ received(n)
/// ```
///
/// The second clause is message conservation. A message a node delivered that no node has yet
/// received (on the wire, or in a transport's delay) makes `Σ delivered > Σ received`, so
/// quiescence is decided by counts, never by a silence window. Every clause changes only
/// together with an activity (a delivery, a reception, a handled message, a released transfer),
/// so probing it on activity observes the quiescent state as soon as it holds. Pre: `nodes` are
/// all the nodes that exchange messages in the test.
pub(crate) fn swarms_quiescent<'a>(
    nodes: impl IntoIterator<Item = (&'a Swarm, &'a MessageLedger)>,
) -> bool {
    let (idle, delivered, received) = nodes.into_iter().fold(
        (true, 0_u64, 0_u64),
        |(idle, delivered, received), (swarm, ledger)| {
            (
                idle && !swarm_in_flight(swarm),
                delivered + ledger.delivered(),
                received + ledger.received(),
            )
        },
    );
    idle && delivered == received
}
