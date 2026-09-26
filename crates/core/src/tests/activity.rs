//! Activity sources and the quiescence predicate of core's test nodes, native and browser.
//!
//! The activity cell and the activity-woken probe live in [`rings_test_support::activity`];
//! this module wires core's test nodes to it. A node's observer ([`ActivityObserver`]) and
//! callback ([`ActivityCallback`]) record activity on every message delivered, received,
//! handled or stored and on every lookup event and connection event; the transport's frame
//! ledger records it on every frame sent, arrived or handed off, and feeds the conservation
//! clause of [`swarms_quiescent`].

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
pub(crate) use rings_test_support::activity::activity_after;
pub(crate) use rings_test_support::activity::activity_mark;
pub(crate) use rings_test_support::activity::record_activity;

use crate::message::MessagePayload;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::callback::SwarmEvent;
use crate::swarm::observer::LookupCorrelation;
use crate::swarm::observer::LookupKind;
use crate::swarm::observer::LookupOutcome;
use crate::swarm::observer::MessageObservation;
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

/// Observer of a test swarm: records activity on every message and lookup observation.
pub(crate) struct ActivityObserver;

impl SwarmObserver for ActivityObserver {
    fn observe_message(&self, _observation: MessageObservation) {
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

/// Callback of a test swarm: records activity on every validated or handled message and on
/// every connection event.
pub(crate) struct ActivityCallback;

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl SwarmCallback for ActivityCallback {
    async fn on_validate(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
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

/// Whether `swarm` has work in flight: a handshake, a frame being sent or not yet handed off to
/// the inbound actor, an admitted inbound message not yet handled, or an outbound transfer not
/// yet completed.
pub(crate) fn swarm_in_flight(swarm: &Swarm) -> bool {
    swarm
        .transport
        .pending_connection_count()
        .unwrap_or_default()
        > 0
        || swarm.transport.frames_for_test().busy()
        || swarm.transport.inbound_admitted_count_for_test() > 0
        || swarm.transport.outbound_admitted_transfer_total_for_test() > 0
}

/// Whether `swarms` are quiescent: nothing in flight on any of them, and nothing between them.
///
/// ```text
/// quiescent ≡ ∀ n. ¬in_flight(n)  ∧  Σ sent(n) = Σ arrived(n)
///             ∧ no activity during the evaluation
/// ```
///
/// The second clause is frame conservation (see
/// [`FrameLedger`](crate::swarm::transport::frame_ledger)): a frame one swarm sent that no swarm
/// has yet received (on the wire, or in a transport's delay) makes `Σ sent > Σ arrived`, so
/// quiescence is decided by counts, never by a silence window. Frames are counted at the
/// swarm's two chokepoints, the transport's acceptance of a send and the link stage's entry
/// before decoding, so a frame a receiver drops anywhere, malformed, refused or swept from a
/// hold, still balances its send; and an arrived frame stays in flight until the inbound actor's
/// capacity permit covers it, so no frame falls between two witnesses of the first clause.
///
/// The third clause makes the fold safe on a multi-thread runtime: swarms are sampled one after
/// another, so a frame moving between two samples could balance the counts falsely. Every
/// counted transition records activity, so the evaluation holds only if the activity generation
/// did not change while the swarms were sampled.
///
/// Preconditions: `swarms` are all the swarms that exchange frames in the test, and no frame is
/// lost below the swarm (a link retired with frames still on the wire); a violation makes the
/// wait fail at its hang guard rather than pass falsely.
pub(crate) fn swarms_quiescent<'a>(swarms: impl IntoIterator<Item = &'a Swarm>) -> bool {
    let mark = activity_mark();
    let (idle, sent, arrived) =
        swarms
            .into_iter()
            .fold((true, 0_u64, 0_u64), |(idle, sent, arrived), swarm| {
                let frames = swarm.transport.frames_for_test();
                (
                    idle && !swarm_in_flight(swarm),
                    sent + frames.sent(),
                    arrived + frames.arrived(),
                )
            });
    idle && sent == arrived && activity_mark() == mark
}
