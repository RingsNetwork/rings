//! Conformance of the model's composed steps with the production shell.
//!
//! The model composes registry and topology transitions itself (`node`),
//! because `SwarmTransport` performs that composition behind async transport
//! effects a checker cannot expand. These tests close the gap on the
//! acceptance trace: disconnect, removal, rejoin under a newer generation,
//! and the retired generation's late events are driven through a real
//! `SwarmTransport` and through the model, and after every step the
//! observer's production state must equal the model's:
//!
//! ```text
//! π(transport) = (successors, predecessor, fingers, admitted generation of the peer,
//!                 storage repair requested since the previous observation)
//! ∀ step i of the trace.  π(transport_i) = π(model_i)
//! ```
//!
//! The repair component is read at the consumer of the removal outcome, so
//! each trace drives the production layer that consumes it: the close path
//! through `MessageHandler::leave_dht_attempt`, the send-terminal path
//! through the stabilizer's `clean_unavailable_connections`. The model's
//! component is the `StorageRepair` effect of its `Observe` step.
//!
//! The transport carries the production table sizes (`K = 3`, 160 finger
//! slots), so the finger projection compared here is not degenerate. The
//! first test removes on the close path (`Ordinary`); the second, under the
//! dummy transport whose data channels are routable, removes the head on the
//! send-terminal path (`Unavailable`), the composition whose argument shape
//! differs from production's; the third closes an admitted peer that no slot
//! references, the removal that must request no repair round.

use std::num::NonZeroU32;
use std::sync::Arc;

use super::enabled_step;
use super::model_rejoin;
use super::node::Effect;
use super::node::LifecycleEvent;
use super::observed_at;
use super::overlay::Bootstrap;
use super::overlay::Budget;
use super::overlay::Overlay;
use super::overlay::OverlayAction;
use super::overlay::OverlayState;
use super::overlay::ShellMutation;
use crate::dht::topology::DEFAULT_SUCCESSOR_CAPACITY;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::message::MessageHandler;
use crate::swarm::transport::pending::PendingConnectionAttempt;
use crate::swarm::transport::tests::transport_with_key;
use crate::swarm::transport::tests::NoopSwarmCallback;
use crate::swarm::transport::SwarmTransport;

/// Fixed observer key, so the compared ring is the same in every run.
const OBSERVER_KEY: &str = "65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0";

/// `π`: the observable the two sides must agree on.
#[derive(Debug, PartialEq, Eq)]
struct Projection {
    /// Successor sequence.
    successors: Vec<Did>,
    /// Predecessor.
    predecessor: Option<Did>,
    /// Finger table.
    fingers: Vec<Option<Did>>,
    /// Admitted generation of the peer under observation.
    admitted: Option<PendingConnectionAttempt>,
    /// Whether a storage repair round was requested since the previous
    /// observation.
    storage_repair: bool,
}

/// `π(model)` at `observer`, about `peer`, after a step whose `StorageRepair`
/// effect is `storage_repair`.
fn model_projection(
    state: &OverlayState,
    observer: Did,
    peer: Did,
    storage_repair: bool,
) -> Projection {
    let node = &state.nodes[&observer];
    Projection {
        successors: node.topology.successors.clone(),
        predecessor: node.topology.predecessor,
        fingers: node.topology.fingers.clone(),
        admitted: node.lifecycles.active_attempt(peer),
        storage_repair,
    }
}

/// `π(transport)`, about `peer`. Reading the repair component claims the
/// request, so the next observation sees only what the next step requests.
fn production_projection(transport: &SwarmTransport, peer: Did) -> Result<Projection> {
    let topology = transport.dht.topology_state()?;
    Ok(Projection {
        successors: topology.successors,
        predecessor: topology.predecessor,
        fingers: topology.fingers,
        admitted: transport.active_attempt(peer)?,
        storage_repair: transport.claim_storage_repair(),
    })
}

/// The model's `Observe(observer, event)`: the next state and whether the
/// step requested a storage repair round.
fn observed_step(
    overlay: &Overlay,
    state: &OverlayState,
    observer: Did,
    event: LifecycleEvent,
) -> (OverlayState, bool) {
    let requested = state.nodes[&observer]
        .as_ref()
        .clone()
        .observe(event, overlay)
        .effects
        .contains(&Effect::StorageRepair);
    (
        enabled_step(overlay, state, observed_at(observer)(event)),
        requested,
    )
}

/// The production consumer of the close path: `leave_dht_attempt` removes
/// the closed generation's peer and requests the repair round from the
/// removal outcome.
fn handler_for(transport: &Arc<SwarmTransport>) -> MessageHandler {
    MessageHandler::new(Arc::clone(transport), Arc::new(NoopSwarmCallback))
}

/// A transport at the fixed observer identity, the model ring of `PEERS`
/// evenly spaced peers around it with the production table sizes and the
/// restart budget, and the ring's identities.
fn observed_ring<const PEERS: usize>() -> Result<(Arc<SwarmTransport>, Overlay, [Did; PEERS])> {
    let transport = transport_with_key(&SecretKey::try_from(OBSERVER_KEY)?)?;
    let budget = Budget {
        departures: 1,
        rejoins: 1,
        ..super::QUIET
    };
    let overlay = Overlay::new(
        transport.dht.did,
        NonZeroU32::new(PEERS as u32).unwrap_or(NonZeroU32::MIN),
        DEFAULT_SUCCESSOR_CAPACITY,
        DEFAULT_FINGER_TABLE_SIZE,
        budget,
        Bootstrap::ConvergedMesh,
        ShellMutation::Faithful,
    );
    let ring = <[Did; PEERS]>::try_from(overlay.ring()).unwrap();
    Ok((Arc::new(transport), overlay, ring))
}

/// Production admission of `peer` without a transport object: reservation,
/// the topology `Admit`, and activation, as `commit_connection_admission`
/// orders them.
async fn admit_unlinked(transport: &SwarmTransport, peer: Did) -> Result<PendingConnectionAttempt> {
    let attempt = transport.reserve_pending_connection(peer).await?;
    transport.dht.admit_connected(peer, None)?;
    assert!(transport.activate_connection_for_test(attempt)?);
    Ok(attempt)
}

/// Law: the retired generation's late events are inert on both sides, whose
/// projections about `peer` stay `admitted`, and request no repair round.
async fn assert_retired_generation_is_inert(
    transport: &Arc<SwarmTransport>,
    retired: PendingConnectionAttempt,
    model: &OverlayState,
    observer: Did,
    peer: Did,
    admitted: &Projection,
) -> Result<()> {
    assert!(transport.disconnect_unavailable(retired).await?.is_none());
    assert!(transport.disconnect_attempt(retired).await?.is_none());
    assert!(transport
        .remove_retired_attempt_topology(retired)?
        .is_none());
    handler_for(transport).leave_dht_attempt(retired).await?;
    assert!(!admitted.storage_repair);
    assert_eq!(production_projection(transport, peer)?, *admitted);
    assert_eq!(model_projection(model, observer, peer, false), *admitted);
    Ok(())
}

/// Law: on the close path, the model's composed steps and the production
/// shell agree on `π` after every step, and the retired generation's late
/// events are inert in both.
#[tokio::test]
async fn test_model_agrees_with_the_production_shell_on_the_close_path() -> Result<()> {
    let (transport, overlay, [observer, successor, departed]) = observed_ring::<3>()?;
    let handler = handler_for(&transport);

    // Init: both peers admitted in ring order, predecessor notified.
    let model = overlay.init();
    admit_unlinked(&transport, successor).await?;
    let retired = admit_unlinked(&transport, departed).await?;
    transport.dht.notify(departed)?;
    assert_eq!(
        production_projection(&transport, departed)?,
        model_projection(&model, observer, departed, false)
    );

    // Disconnect and removal: the admitted generation closes; the departed
    // peer held a successor and the predecessor slot, so the removal vacates
    // slots and the repair round is requested.
    let model = enabled_step(&overlay, &model, OverlayAction::Depart(departed));
    let (model, requested) =
        observed_step(&overlay, &model, observer, LifecycleEvent::Closed(retired));
    assert!(requested);
    handler.leave_dht_attempt(retired).await?;
    assert_eq!(
        production_projection(&transport, departed)?,
        model_projection(&model, observer, departed, requested)
    );

    // Rejoin: the same peer is admitted under a newer generation.
    let (model, newer) = model_rejoin(&overlay, &model, observer, departed);
    assert_eq!(newer, admit_unlinked(&transport, departed).await?);
    assert!(newer.generation() > retired.generation());
    let admitted = production_projection(&transport, departed)?;
    assert_eq!(
        admitted,
        model_projection(&model, observer, departed, false)
    );

    // The retired generation's remaining event arrives.
    let (model, requested) = observed_step(
        &overlay,
        &model,
        observer,
        LifecycleEvent::SendTerminal(retired),
    );
    assert!(!requested);
    assert_retired_generation_is_inert(&transport, retired, &model, observer, departed, &admitted)
        .await
}

/// Law: on the close path of an admitted peer that no slot references, the
/// removal is the identity on the topology in both the model and the
/// production shell, and neither requests a repair round. In a ring of six,
/// the fifth peer is behind the `K = 3` successors, ahead of the predecessor,
/// and the fixpoint of no finger slot.
#[tokio::test]
async fn test_model_agrees_with_the_production_shell_on_an_unreferenced_close() -> Result<()> {
    let (transport, overlay, [observer, first, second, third, unreferenced, last]) =
        observed_ring::<6>()?;
    let handler = handler_for(&transport);

    // Init: every peer admitted in ring order, predecessor notified.
    let model = overlay.init();
    for peer in [first, second, third] {
        admit_unlinked(&transport, peer).await?;
    }
    let retired = admit_unlinked(&transport, unreferenced).await?;
    admit_unlinked(&transport, last).await?;
    transport.dht.notify(last)?;
    let before = production_projection(&transport, unreferenced)?;
    assert_eq!(
        before,
        model_projection(&model, observer, unreferenced, false)
    );
    assert_eq!(before.admitted, Some(retired));
    assert!(!transport.dht.topology_state()?.references(unreferenced));

    // Disconnect and removal: no slot is vacated, no repair round is requested.
    let model = enabled_step(&overlay, &model, OverlayAction::Depart(unreferenced));
    let (model, requested) =
        observed_step(&overlay, &model, observer, LifecycleEvent::Closed(retired));
    assert!(!requested);
    handler.leave_dht_attempt(retired).await?;
    let after = production_projection(&transport, unreferenced)?;
    assert_eq!(
        after,
        model_projection(&model, observer, unreferenced, requested)
    );
    assert_eq!(
        (after.successors, after.predecessor, after.fingers),
        (before.successors, before.predecessor, before.fingers)
    );
    assert_eq!(after.admitted, None);
    Ok(())
}

/// Law: on the send-terminal path, the unavailable head is replaced by the
/// routable admitted successors in both the model and the production shell,
/// the rejoined head is re-admitted under the same newer generation, and the
/// retired generation's late events are inert in both.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_model_agrees_with_the_production_shell_on_the_unavailable_path() -> Result<()> {
    use crate::dht::Stabilizer;
    use crate::swarm::callback::InnerSwarmCallback;
    use crate::swarm::inbox::SwarmInboxDelivery;
    use crate::swarm::transport::tests::open_dummy_data_channel_before_ice_connected;

    /// Production admission of `peer` over a routable dummy transport.
    async fn admit_routable(
        transport: &Arc<SwarmTransport>,
        peer: Did,
    ) -> Result<PendingConnectionAttempt> {
        let callback = InnerSwarmCallback::new(Arc::clone(transport), Arc::new(NoopSwarmCallback));
        let (attempt, _offer) = transport
            .prepare_connection_offer_with_attempt(peer, callback)
            .await?;
        open_dummy_data_channel_before_ice_connected(transport, peer).await?;
        transport.dht.admit_connected(peer, None)?;
        assert!(transport.activate_connection_for_test(attempt)?);
        Ok(attempt)
    }

    let (transport, overlay, [observer, head, other]) = observed_ring::<3>()?;
    let stabilizer = Stabilizer::new(
        Arc::clone(&transport),
        Arc::new(SwarmInboxDelivery::new(Arc::clone(&transport))),
    );

    // Init: both peers admitted and routable; the head is the departing peer.
    let model = overlay.init();
    let retired = admit_routable(&transport, head).await?;
    admit_routable(&transport, other).await?;
    transport.dht.notify(other)?;
    assert_eq!(
        production_projection(&transport, head)?,
        model_projection(&model, observer, head, false)
    );

    // A send fails, then the sweep retires the head as unavailable: the
    // other routable peer replaces it, and the vacated head slot requests
    // the repair round.
    let model = enabled_step(&overlay, &model, OverlayAction::Depart(head));
    let (model, requested) = observed_step(
        &overlay,
        &model,
        observer,
        LifecycleEvent::SendTerminal(retired),
    );
    assert!(!requested);
    let (model, requested) = observed_step(
        &overlay,
        &model,
        observer,
        LifecycleEvent::RetireUnavailable(retired),
    );
    assert!(requested);
    assert!(transport.peer_lifecycles()?.mark_send_terminal(retired));
    stabilizer.clean_unavailable_connections().await?;
    let replaced = production_projection(&transport, head)?;
    assert_eq!(replaced.successors, vec![other]);
    assert_eq!(
        replaced,
        model_projection(&model, observer, head, requested)
    );

    // Rejoin: the head is re-admitted under the same newer generation.
    let (model, newer) = model_rejoin(&overlay, &model, observer, head);
    assert_eq!(newer, admit_routable(&transport, head).await?);
    let admitted = production_projection(&transport, head)?;
    assert_eq!(admitted, model_projection(&model, observer, head, false));

    // The retired generation's close arrives.
    let (model, requested) =
        observed_step(&overlay, &model, observer, LifecycleEvent::Closed(retired));
    assert!(!requested);
    assert_retired_generation_is_inert(&transport, retired, &model, observer, head, &admitted).await
}
