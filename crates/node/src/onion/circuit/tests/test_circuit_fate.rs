//! `Law (Circuit fate)` of the pure reducer (#963): a retired link or a received `Destroy` removes
//! the circuits it touches and carries their destruction away from it.

use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;

use super::super::codec::OnionCircuitInput;
use super::super::codec::OnionWireMessage;
use super::super::protocol::OnionCircuitCapabilities;
use super::super::reducer::remember_return_hop;
use super::super::reducer::OnionCircuitReducer;
use super::super::reducer::RelayReturnEdge;
use super::super::reducer::RelayReturnKey;
use super::super::OnionCellBucket;
use super::super::OnionCircuitEffect;
use super::super::OnionCircuitId;
use super::super::OnionCircuitState;
use super::super::OnionCircuitTeardown;
use super::super::OnionRelayedCell;
use super::super::MAX_ONION_RELAY_CIRCUITS;
use super::super::ONION_RELAY_RETURN_TTL_MS;
use super::test_circuit_protocol::session;

/// Circuit id of the relay's edge toward the client.
const PREVIOUS_CIRCUIT: OnionCircuitId = OnionCircuitId::new([1; 16]);

/// Circuit id of the relay's edge toward the exit.
const NEXT_CIRCUIT: OnionCircuitId = OnionCircuitId::new([2; 16]);

/// A relay holding one circuit between `previous` and `next`.
fn relay_with_one_circuit(previous: &DelegateeKey, next: &DelegateeKey) -> OnionCircuitState {
    let mut state = OnionCircuitState::default();
    remember_return_hop(
        &mut state,
        MAX_ONION_RELAY_CIRCUITS,
        ONION_RELAY_RETURN_TTL_MS,
        RelayReturnEdge {
            key: RelayReturnKey {
                circuit_id: NEXT_CIRCUIT,
                next_hop: next.delegator_did(),
            },
            previous_hop: previous.delegator_did(),
            previous_circuit_id: PREVIOUS_CIRCUIT,
            previous_delegatee_public_key: previous.delegatee_public_key(),
            next_delegatee_public_key: next.delegatee_public_key(),
        },
        0,
    )
    .expect("remember the circuit");
    state
}

/// `Destroy` of `circuit_id` sent to `to`, sealed for `recipient`, as the reducer emits it.
fn is_destroy_sent(
    effect: &OnionCircuitEffect,
    to: &DelegateeKey,
    circuit_id: OnionCircuitId,
) -> bool {
    match effect {
        OnionCircuitEffect::SealAndSend {
            to: sent_to,
            recipient,
            bucket,
            encoded_message,
            cell,
        } => {
            *cell == OnionRelayedCell::Destroy
                && *sent_to == to.delegator_did()
                && *recipient == to.delegatee_public_key()
                && *bucket == OnionCellBucket::KiB4
                && rings_codec::deserialize::<OnionWireMessage>(encoded_message.as_ref())
                    .is_ok_and(|message| message == OnionWireMessage::Destroy { circuit_id })
        }
        _ => false,
    }
}

/// The effects and next state of `input` applied to `state` at a relay.
fn apply(
    state: &OnionCircuitState,
    input: OnionCircuitInput,
) -> (OnionCircuitState, Vec<OnionCircuitEffect>) {
    let transition = OnionCircuitReducer::new(OnionCircuitCapabilities::Relay).apply(state, input);
    (transition.state, transition.effects)
}

/// A `Destroy` cell from `from`.
fn destroy_from(from: Did, circuit_id: OnionCircuitId) -> OnionCircuitInput {
    OnionCircuitInput::CellReady {
        from,
        received_at_ms: 1,
        bucket: OnionCellBucket::KiB4,
        message: OnionWireMessage::Destroy { circuit_id },
    }
}

/// A retired next hop destroys the circuit toward the client and tears down local endpoints
/// on that link; a retired previous hop destroys it toward the exit.
#[test]
fn test_a_retired_link_destroys_its_circuits_away_from_it() {
    let previous = session();
    let next = session();
    let state = relay_with_one_circuit(&previous, &next);

    let (after, effects) = apply(&state, OnionCircuitInput::PeerRetired {
        peer: next.delegator_did(),
    });
    assert_eq!(after.relay_return_count(), 0);
    assert_eq!(effects.len(), 2);
    assert!(is_destroy_sent(&effects[0], &previous, PREVIOUS_CIRCUIT));
    assert_eq!(
        effects[1],
        OnionCircuitEffect::Teardown(OnionCircuitTeardown::Peer(next.delegator_did()))
    );

    let (after, effects) = apply(&state, OnionCircuitInput::PeerRetired {
        peer: previous.delegator_did(),
    });
    assert_eq!(after.relay_return_count(), 0);
    assert!(is_destroy_sent(&effects[0], &next, NEXT_CIRCUIT));
}

/// A `Destroy` from either neighbour of a relayed circuit is carried on to the other one and
/// removes the circuit; the same id on an unrelated link changes nothing.
#[test]
fn test_a_destroy_crosses_a_relay_to_the_other_neighbour() {
    let previous = session();
    let next = session();
    let stranger = session();
    let state = relay_with_one_circuit(&previous, &next);

    let (after, effects) = apply(&state, destroy_from(next.delegator_did(), NEXT_CIRCUIT));
    assert_eq!(after.relay_return_count(), 0);
    assert!(is_destroy_sent(&effects[0], &previous, PREVIOUS_CIRCUIT));

    let (after, effects) = apply(
        &state,
        destroy_from(previous.delegator_did(), PREVIOUS_CIRCUIT),
    );
    assert_eq!(after.relay_return_count(), 0);
    assert!(is_destroy_sent(&effects[0], &next, NEXT_CIRCUIT));

    let (after, effects) = apply(&state, destroy_from(stranger.delegator_did(), NEXT_CIRCUIT));
    assert_eq!(after.relay_return_count(), 1);
    assert_eq!(effects, vec![OnionCircuitEffect::Teardown(
        OnionCircuitTeardown::Circuit {
            from: stranger.delegator_did(),
            circuit_id: NEXT_CIRCUIT,
        }
    )]);
}

/// A teardown names exactly the endpoints on its link: every circuit of a retired peer, or the
/// one circuit a `Destroy` named on the link it came from.
#[test]
fn test_a_teardown_ends_only_the_endpoints_on_its_link() {
    let peer = session().delegator_did();
    let other = session().delegator_did();
    assert!(OnionCircuitTeardown::Peer(peer).ends(peer, NEXT_CIRCUIT));
    assert!(!OnionCircuitTeardown::Peer(peer).ends(other, NEXT_CIRCUIT));
    let circuit = OnionCircuitTeardown::Circuit {
        from: peer,
        circuit_id: NEXT_CIRCUIT,
    };
    assert!(circuit.ends(peer, NEXT_CIRCUIT));
    assert!(!circuit.ends(peer, PREVIOUS_CIRCUIT));
    assert!(!circuit.ends(other, NEXT_CIRCUIT));
}

/// A relayed cell this node's outbox refused destroys its circuit here: the edge is removed and a
/// `Destroy` goes to both neighbours, the one behind the full queue included.
#[test]
fn test_a_refused_cell_destroys_its_circuit_toward_both_neighbours() {
    let previous = session();
    let next = session();
    let state = relay_with_one_circuit(&previous, &next);

    for (to, circuit_id) in [
        (next.delegator_did(), NEXT_CIRCUIT),
        (previous.delegator_did(), PREVIOUS_CIRCUIT),
    ] {
        let (after, effects) = apply(&state, OnionCircuitInput::CellRefused { to, circuit_id });
        assert_eq!(after.relay_return_count(), 0);
        assert_eq!(effects.len(), 2);
        assert!(is_destroy_sent(&effects[0], &next, NEXT_CIRCUIT));
        assert!(is_destroy_sent(&effects[1], &previous, PREVIOUS_CIRCUIT));
    }
}
