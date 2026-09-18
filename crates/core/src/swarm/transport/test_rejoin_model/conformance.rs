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
//! π(transport) = (successors, predecessor, fingers, admitted generation of the peer)
//! ∀ step i of the trace.  π(transport_i) = π(model_i)
//! ```
//!
//! The transport carries the production table sizes (`K = 3`, 160 finger
//! slots), so the finger projection compared here is not degenerate. The
//! first test removes on the close path (`Ordinary`); the second, under the
//! dummy transport whose data channels are routable, removes the head on the
//! send-terminal path (`Unavailable`), the composition whose argument shape
//! differs from production's.

#[cfg(feature = "dummy")]
use std::sync::Arc;

use super::enabled_step;
use super::node::LifecycleEvent;
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
use crate::swarm::transport::pending::PendingConnectionAttempt;
use crate::swarm::transport::tests::transport_with_key;
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
}

/// `π(model)` at `observer`, about `peer`.
fn model_projection(state: &OverlayState, observer: Did, peer: Did) -> Projection {
    let node = &state.nodes[&observer];
    Projection {
        successors: node.topology.successors.clone(),
        predecessor: node.topology.predecessor,
        fingers: node.topology.fingers.clone(),
        admitted: node.lifecycles.active_attempt(peer),
    }
}

/// `π(transport)`, about `peer`.
fn production_projection(transport: &SwarmTransport, peer: Did) -> Result<Projection> {
    let topology = transport.dht.topology_state()?;
    Ok(Projection {
        successors: topology.successors,
        predecessor: topology.predecessor,
        fingers: topology.fingers,
        admitted: transport.active_attempt(peer)?,
    })
}

/// The model ring of three peers around the transport's own identity, with
/// the production table sizes and the restart budget.
fn ring_around(transport: &SwarmTransport) -> Overlay {
    let budget = Budget {
        departures: 1,
        rejoins: 1,
        ..super::QUIET
    };
    Overlay::new(
        transport.dht.did,
        3,
        DEFAULT_SUCCESSOR_CAPACITY,
        DEFAULT_FINGER_TABLE_SIZE,
        budget,
        ShellMutation::Faithful,
    )
}

/// The model's rejoin of `departed` through `observer`: restart, bootstrap
/// dial, answer, and admission under the newer generation.
fn model_rejoin(
    overlay: &Overlay,
    state: &OverlayState,
    observer: Did,
    departed: Did,
) -> (OverlayState, PendingConnectionAttempt) {
    let up = enabled_step(overlay, state, OverlayAction::Rejoin(departed));
    let dialed = enabled_step(overlay, &up, OverlayAction::Dial {
        from: departed,
        to: observer,
    });
    let offer = dialed.network.first().cloned().unwrap();
    let answered = enabled_step(overlay, &dialed, OverlayAction::Deliver(offer));
    let newer = answered.nodes[&observer]
        .lifecycles
        .unadmitted_attempt(departed)
        .unwrap();
    let admitted = enabled_step(overlay, &answered, OverlayAction::Observe {
        peer: observer,
        event: LifecycleEvent::ChannelOpened(newer),
    });
    (admitted, newer)
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

/// Law: on the close path, the model's composed steps and the production
/// shell agree on `π` after every step, and the retired generation's late
/// events are inert in both.
#[tokio::test]
async fn test_model_agrees_with_the_production_shell_on_the_close_path() -> Result<()> {
    let transport = transport_with_key(&SecretKey::try_from(OBSERVER_KEY)?)?;
    let overlay = ring_around(&transport);
    let [observer, successor, departed] = <[Did; 3]>::try_from(overlay.ring()).unwrap();
    let observe = |event| OverlayAction::Observe {
        peer: observer,
        event,
    };

    // Init: both peers admitted in ring order, predecessor notified.
    let model = overlay.converged_mesh();
    admit_unlinked(&transport, successor).await?;
    let retired = admit_unlinked(&transport, departed).await?;
    transport.dht.notify(departed)?;
    assert_eq!(
        production_projection(&transport, departed)?,
        model_projection(&model, observer, departed)
    );

    // Disconnect and removal: the admitted generation closes.
    let model = enabled_step(&overlay, &model, OverlayAction::Depart(departed));
    let model = enabled_step(&overlay, &model, observe(LifecycleEvent::Closed(retired)));
    assert!(transport.disconnect_attempt(retired).await?);
    assert_eq!(
        production_projection(&transport, departed)?,
        model_projection(&model, observer, departed)
    );

    // Rejoin: the same peer is admitted under a newer generation.
    let (model, newer) = model_rejoin(&overlay, &model, observer, departed);
    let production_newer = admit_unlinked(&transport, departed).await?;
    assert_eq!(newer, production_newer);
    assert!(newer.generation() > retired.generation());
    let admitted = production_projection(&transport, departed)?;
    assert_eq!(admitted, model_projection(&model, observer, departed));

    // The retired generation's remaining event arrives: inert on both sides.
    let model = enabled_step(
        &overlay,
        &model,
        observe(LifecycleEvent::SendTerminal(retired)),
    );
    assert!(transport.disconnect_unavailable(retired).await?.is_none());
    assert!(!transport.disconnect_attempt(retired).await?);
    assert!(!transport.remove_retired_attempt_topology(retired)?);
    assert_eq!(production_projection(&transport, departed)?, admitted);
    assert_eq!(model_projection(&model, observer, departed), admitted);
    Ok(())
}

/// Law: on the send-terminal path, the unavailable head is replaced by the
/// routable admitted successors in both the model and the production shell,
/// the rejoined head is re-admitted under the same newer generation, and the
/// retired generation's sweep is inert in both.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_model_agrees_with_the_production_shell_on_the_unavailable_path() -> Result<()> {
    use crate::swarm::callback::InnerSwarmCallback;
    use crate::swarm::transport::tests::open_dummy_data_channel_before_ice_connected;
    use crate::swarm::transport::tests::NoopSwarmCallback;

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

    let transport = Arc::new(transport_with_key(&SecretKey::try_from(OBSERVER_KEY)?)?);
    let overlay = ring_around(&transport);
    let [observer, head, other] = <[Did; 3]>::try_from(overlay.ring()).unwrap();
    let observe = |event| OverlayAction::Observe {
        peer: observer,
        event,
    };

    // Init: both peers admitted and routable; the head is the departing peer.
    let model = overlay.converged_mesh();
    let retired = admit_routable(&transport, head).await?;
    admit_routable(&transport, other).await?;
    transport.dht.notify(other)?;
    assert_eq!(
        production_projection(&transport, head)?,
        model_projection(&model, observer, head)
    );

    // A send fails, then the sweep retires the head as unavailable: the
    // other routable peer replaces it.
    let model = enabled_step(&overlay, &model, OverlayAction::Depart(head));
    let model = enabled_step(
        &overlay,
        &model,
        observe(LifecycleEvent::SendTerminal(retired)),
    );
    let model = enabled_step(
        &overlay,
        &model,
        observe(LifecycleEvent::RetireUnavailable(retired)),
    );
    assert!(transport.peer_lifecycles()?.mark_send_terminal(retired));
    let outcome = transport.disconnect_unavailable(retired).await?.unwrap();
    assert_eq!(outcome.fallback(), Some(other));
    let replaced = production_projection(&transport, head)?;
    assert_eq!(replaced.successors, vec![other]);
    assert_eq!(replaced, model_projection(&model, observer, head));

    // Rejoin: the head is re-admitted under the same newer generation.
    let (model, newer) = model_rejoin(&overlay, &model, observer, head);
    let production_newer = admit_routable(&transport, head).await?;
    assert_eq!(newer, production_newer);
    let admitted = production_projection(&transport, head)?;
    assert_eq!(admitted, model_projection(&model, observer, head));

    // The retired generation's close arrives: inert on both sides.
    let model = enabled_step(&overlay, &model, observe(LifecycleEvent::Closed(retired)));
    assert!(transport.disconnect_unavailable(retired).await?.is_none());
    assert!(!transport.disconnect_attempt(retired).await?);
    assert_eq!(production_projection(&transport, head)?, admitted);
    assert_eq!(model_projection(&model, observer, head), admitted);
    Ok(())
}
