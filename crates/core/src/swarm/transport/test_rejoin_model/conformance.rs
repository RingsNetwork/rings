//! Conformance of the model's composed steps with the production shell.
//!
//! The model composes registry and topology transitions itself (`node`),
//! because `SwarmTransport` performs that composition behind async transport
//! effects a checker cannot expand. This test closes the gap on the
//! acceptance trace: the same disconnect, removal, rejoin, and retired-close
//! sequence is driven through a real `SwarmTransport` and through the model,
//! and after every step the observer's production state must equal the
//! model's:
//!
//! ```text
//! π(transport) = (successors, predecessor, fingers, admitted generation of the peer)
//! ∀ step i of the trace.  π(transport_i) = π(model_i)
//! ```
//!
//! The transport carries the production table sizes (`K = 3`, 160 finger
//! slots), so the finger projection compared here is not degenerate. The
//! harness has no physical connections, so the unavailable-peer replacement
//! (which requires transport readiness) is outside this conformance; its
//! argument shape is covered by
//! `test_replacement_normalization_is_absorbed_by_the_production_remove`.

use std::sync::Arc;

use super::enabled_step;
use super::node::Callback;
use super::overlay::Budgets;
use super::overlay::Overlay;
use super::overlay::OverlayAction;
use super::overlay::OverlayState;
use super::overlay::ShellMutation;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::swarm::transport::pending::PendingConnectionAttempt;
use crate::swarm::transport::tests::transport_with_key_and_measure;
use crate::swarm::transport::tests::RecordingMeasure;
use crate::swarm::transport::SwarmTransport;

/// Successor capacity of the harness transport.
const PRODUCTION_SUCCESSOR_CAPACITY: usize = 3;

/// Fixed observer key, so the compared ring is the same in every run.
const OBSERVER_KEY: &str = "65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0";

/// `π`: the observable the two sides must agree on.
type Projection = (
    Vec<Did>,
    Option<Did>,
    Vec<Option<Did>>,
    Option<PendingConnectionAttempt>,
);

/// `π(model)` at `observer`, about `peer`.
fn model_projection(state: &OverlayState, observer: Did, peer: Did) -> Projection {
    let node = &state.nodes[&observer];
    (
        node.topology.successors.clone(),
        node.topology.predecessor,
        node.topology.fingers.clone(),
        node.lifecycles.active_attempt(peer),
    )
}

/// `π(transport)`, about `peer`.
fn production_projection(transport: &SwarmTransport, peer: Did) -> Result<Projection> {
    let topology = transport.dht.topology_state()?;
    Ok((
        topology.successors,
        topology.predecessor,
        topology.fingers,
        transport.active_attempt(peer)?,
    ))
}

/// Production admission of `peer`: reservation, activation, and the topology
/// `Admit`, as `commit_connection_admission` orders them.
async fn admit(transport: &SwarmTransport, peer: Did) -> Result<PendingConnectionAttempt> {
    let attempt = transport.reserve_pending_connection(peer).await?;
    transport.dht.admit_connected(peer, None)?;
    assert!(transport.activate_connection_for_test(attempt)?);
    Ok(attempt)
}

/// Law: on the acceptance trace, the model's composed steps and the
/// production shell agree on `π` after every step, and the retired
/// generation's close is inert in both.
#[tokio::test]
async fn test_model_steps_agree_with_the_production_shell_on_the_acceptance_trace() -> Result<()> {
    let key = SecretKey::try_from(OBSERVER_KEY)?;
    let transport = transport_with_key_and_measure(&key, Arc::new(RecordingMeasure::default()))?;
    let observer = transport.dht.did;
    let budgets = Budgets {
        departures: 1,
        rejoins: 1,
        ..super::QUIET
    };
    let overlay = Overlay::new(
        observer,
        3,
        PRODUCTION_SUCCESSOR_CAPACITY,
        DEFAULT_FINGER_TABLE_SIZE,
        budgets,
        ShellMutation::Faithful,
    );
    let [_, successor, departed] = <[Did; 3]>::try_from(overlay.ring().to_vec()).unwrap();
    let observe = |callback| OverlayAction::Observe {
        node: observer,
        callback,
    };

    // Init: both peers admitted in ring order, predecessor notified.
    let model = overlay.converged_mesh();
    admit(&transport, successor).await?;
    let retired = admit(&transport, departed).await?;
    transport.dht.notify(departed)?;
    assert_eq!(
        production_projection(&transport, departed)?,
        model_projection(&model, observer, departed)
    );

    // Disconnect and removal: the admitted generation closes.
    let model = enabled_step(&overlay, &model, OverlayAction::Depart(departed));
    let model = enabled_step(&overlay, &model, observe(Callback::Closed(retired)));
    assert!(transport.disconnect_attempt(retired).await?);
    assert_eq!(
        production_projection(&transport, departed)?,
        model_projection(&model, observer, departed)
    );

    // Rejoin: the same peer is admitted under a newer generation.
    let model = enabled_step(&overlay, &model, OverlayAction::Rejoin(departed));
    let model = enabled_step(&overlay, &model, OverlayAction::Dial {
        from: departed,
        to: observer,
    });
    let offer = model.network.first().cloned().unwrap();
    let model = enabled_step(&overlay, &model, OverlayAction::Deliver(offer));
    let newer = admit(&transport, departed).await?;
    let model = enabled_step(&overlay, &model, observe(Callback::ChannelOpened(newer)));
    assert!(newer.generation() > retired.generation());
    let admitted = production_projection(&transport, departed)?;
    assert_eq!(admitted, model_projection(&model, observer, departed));

    // The retired generation's remaining event arrives: inert on both sides.
    let model = enabled_step(&overlay, &model, observe(Callback::SendTerminal(retired)));
    assert!(transport.disconnect_unavailable(retired).await?.is_none());
    assert!(!transport.disconnect_attempt(retired).await?);
    assert!(!transport.remove_retired_attempt_topology(retired)?);
    assert_eq!(production_projection(&transport, departed)?, admitted);
    assert_eq!(model_projection(&model, observer, departed), admitted);
    Ok(())
}
