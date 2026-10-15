//! Exit-layer admission and multi-hop relay paths of the circuit reducer and shell.

use std::sync::atomic::Ordering;

use rings_core::message::MessageSigner;

use super::super::cell::seal_encoded_message;
use super::super::cell::seal_message;
use super::super::codec::OnionCircuitInput;
use super::super::codec::OnionWireMessage;
use super::super::crypto::decrypt_forward_layer;
use super::super::crypto::encrypt_client_payload;
use super::super::encode_initial_forward;
use super::super::protocol::OnionCircuitCapabilities;
use super::super::reducer::OnionCircuitReducer;
use super::super::OnionAlgebra;
use super::super::OnionBackwardFrame;
use super::super::OnionCellBucket;
use super::super::OnionCircuitEffect;
use super::super::OnionCircuitExitFrame;
use super::super::OnionCircuitId;
use super::super::OnionCircuitProtocol;
use super::super::OnionCircuitShell;
use super::super::OnionCircuitState;
use super::super::OnionClientReturn;
use super::super::OnionForwardLayer;
use super::super::OnionForwardNonce;
use super::super::OnionForwardSequence;
use super::super::OnionReturnId;
use super::test_circuit_protocol::decode_event;
use super::test_circuit_protocol::open_wire;
use super::test_circuit_protocol::payload_for_service;
use super::test_circuit_protocol::peel_forward_cell;
use super::test_circuit_protocol::relay_to_terminal;
use super::test_circuit_protocol::route;
use super::test_circuit_protocol::session;
use super::test_circuit_protocol::test_payload;
use super::test_circuit_protocol::test_scope;
use super::test_circuit_protocol::RecordingExit;
use super::test_circuit_protocol::RecordingHandler;
use super::test_circuit_protocol::RESTARTED_PROCESS_EPOCH;
use super::test_circuit_protocol::RESTART_REPLAY_CIRCUIT_ID;
use super::test_circuit_protocol::TEST_PROCESS_EPOCH;
use crate::extension::ext::Ctx;
use crate::extension::ext::Interpret;
use crate::extension::ext::Protocol;
use crate::onion::OnionServiceName;
use crate::tests::TEST_NETWORK_ID;

/// Whether the reducer emits an exit effect for an exit layer expiring at `expires_at_ms` and
/// received at `received_at_ms`, all other fields fixed. A pair of calls that differ only in the
/// expiry pins the window as the reason for a rejection.
fn exit_layer_is_admitted(received_at_ms: u128, expires_at_ms: u128) -> bool {
    let client = session();
    let reducer = OnionCircuitReducer::new(OnionCircuitCapabilities::Exit(TEST_PROCESS_EPOCH));
    let state = OnionCircuitState::default();
    let transition = reducer.apply(&state, OnionCircuitInput::ForwardReady {
        from: client.delegator_did(),
        received_at_ms,
        bucket: OnionCellBucket::KiB4,
        circuit_id: OnionCircuitId::new([8; 16]),
        layer: OnionForwardLayer::Exit {
            process_epoch: TEST_PROCESS_EPOCH,
            client: OnionClientReturn::new(client.delegatee_public_key()),
            return_delegatee_public_key: client.delegatee_public_key(),
            expires_at_ms,
            forward_nonce: OnionForwardNonce::new([9; 16]),
            forward_sequence: OnionForwardSequence::FIRST,
            payload: test_payload("window"),
        },
    });
    matches!(transition.effects.as_slice(), [
        OnionCircuitEffect::Exit { .. }
    ])
}

/// Law: at the window's lower boundary, an on-grid layer that expires exactly at its arrival
/// (`x = arr = Q`) is rejected, while the next grid point `x = 2Q` is admitted.
#[test]
fn test_expired_exit_layer_emits_no_exit_effect() {
    let quantum = super::super::ONION_FORWARD_EXPIRY_QUANTUM_MS;
    assert!(!exit_layer_is_admitted(quantum, quantum));
    assert!(exit_layer_is_admitted(quantum, 2 * quantum));
}

#[test]
fn test_read_only_reducer_arm_structurally_shares_return_state() {
    let peer = session();
    let reducer = OnionCircuitReducer::new(OnionCircuitCapabilities::Relay);
    let state = OnionCircuitState::default();

    let transition = reducer.apply(&state, OnionCircuitInput::CellReady {
        from: peer.delegator_did(),
        received_at_ms: 1,
        bucket: OnionCellBucket::KiB4,
        message: OnionWireMessage::Cover,
    });

    assert!(state.shares_return_table_with(&transition.state));
    assert!(transition.effects.is_empty());
}

/// Law: at the window's upper boundary, with `arr = Q − 1`, the on-grid layer
/// `x = 6Q = arr + V + 1` is rejected, while the grid point just inside, `x = 5Q ≤ arr + V`, is
/// admitted.
#[test]
fn test_overlong_exit_layer_emits_no_exit_effect() {
    let quantum = super::super::ONION_FORWARD_EXPIRY_QUANTUM_MS;
    let received_at_ms = quantum - 1;
    let overlong = received_at_ms + super::super::ONION_FORWARD_MAX_VALIDITY_MS + 1;
    assert_eq!(overlong, 6 * quantum);
    assert!(!exit_layer_is_admitted(received_at_ms, overlong));
    assert!(exit_layer_is_admitted(received_at_ms, overlong - quantum));
}

#[tokio::test]
async fn test_relay_decrypts_one_layer_and_remembers_return_hop() {
    let client = session();
    let guard = session();
    let relay = session();
    let exit = session();
    let route = route(&guard, &relay, &exit);
    let circuit_id = OnionCircuitId::new([2; 16]);
    let (_, payload) = encode_initial_forward(
        OnionClientReturn::new(client.delegatee_public_key()),
        &route,
        circuit_id,
        test_payload("tcp-shutdown"),
    )
    .expect("encode forward");
    let payload = relay_to_terminal(&[&guard], &payload);
    let protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::Relay);
    let shell = OnionCircuitShell::new(relay.clone(), RecordingHandler::default());
    let scope = test_scope(relay.clone());
    let event = decode_event(
        &protocol,
        guard.delegator_did(),
        relay.delegator_did(),
        &payload,
    );
    let state = protocol.init();

    let transition = protocol.step(
        Ctx {
            did: relay.delegator_did(),
            state: &state,
        },
        event,
    );
    let [effect] = transition.effects.as_slice() else {
        panic!("expected decrypt effect");
    };
    let reinjected = shell
        .run(&scope, effect.clone())
        .await
        .expect("decrypt forward");
    let [local_payload] = reinjected.as_slice() else {
        panic!("expected local payload");
    };
    let event = decode_event(
        &protocol,
        relay.delegator_did(),
        relay.delegator_did(),
        local_payload,
    );

    let transition = protocol.step(
        Ctx {
            did: relay.delegator_did(),
            state: &transition.state,
        },
        event,
    );
    let [effect] = transition.effects.as_slice() else {
        panic!("expected forward-layer decrypt effect");
    };
    let reinjected = shell
        .run(&scope, effect.clone())
        .await
        .expect("decrypt relay layer");
    let [local_payload] = reinjected.as_slice() else {
        panic!("expected decrypted relay layer");
    };
    let event = decode_event(
        &protocol,
        relay.delegator_did(),
        relay.delegator_did(),
        local_payload,
    );
    let transition = protocol.step(
        Ctx {
            did: relay.delegator_did(),
            state: &transition.state,
        },
        event,
    );

    assert!(matches!(
        transition.effects.as_slice(),
        [OnionCircuitEffect::SealAndSend { to, .. }] if *to == exit.delegator_did()
    ));
    assert_eq!(transition.state.relay_return_count(), 1);
}

#[tokio::test]
async fn test_two_relays_peel_fixed_size_cells_through_the_exit_reducer_and_shell() {
    let client = session();
    let first = session();
    let second = session();
    let exit = session();
    let route = route(&first, &second, &exit);
    let expected = test_payload("multi-hop-fixed-cell");
    let first_edge_id = OnionCircuitId::new([31; 16]);
    let (first_peer, first_payload) = encode_initial_forward(
        OnionClientReturn::new(client.delegatee_public_key()),
        &route,
        first_edge_id,
        expected.clone(),
    )
    .expect("encode multi-hop route");
    assert_eq!(first_peer, first.delegator_did());

    let first_protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::Relay);
    let first_shell = OnionCircuitShell::new(first.clone(), RecordingHandler::default());
    let first_scope = test_scope(first.clone());
    let first_transition = peel_forward_cell(
        &first_protocol,
        &first_shell,
        &first_scope,
        &first_protocol.init(),
        client.delegator_did(),
        first.delegator_did(),
        &first_payload,
    )
    .await;
    let [OnionCircuitEffect::SealAndSend {
        to,
        recipient,
        bucket,
        encoded_message,
        ..
    }] = first_transition.effects.as_slice()
    else {
        panic!("first relay must emit one padded next-hop cell");
    };
    assert_eq!(*to, second.delegator_did());
    let second_edge_id = match rings_codec::deserialize(encoded_message.as_ref())
        .expect("decode second-hop message")
    {
        OnionWireMessage::Forward(frame) => frame.circuit_id,
        OnionWireMessage::Backward(_) => panic!("forward route emitted a backward message"),
        OnionWireMessage::Cover => panic!("forward route emitted a cover message"),
        OnionWireMessage::Destroy { .. } => panic!("forward route emitted a destroy message"),
    };
    assert_ne!(second_edge_id, first_edge_id);
    let second_payload = seal_encoded_message(encoded_message, *recipient, Some(*bucket))
        .expect("seal second-hop cell");
    assert_eq!(first_payload.len(), second_payload.len());

    let second_protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::Relay);
    let second_shell = OnionCircuitShell::new(second.clone(), RecordingHandler::default());
    let second_scope = test_scope(second.clone());
    let second_transition = peel_forward_cell(
        &second_protocol,
        &second_shell,
        &second_scope,
        &second_protocol.init(),
        first.delegator_did(),
        second.delegator_did(),
        &second_payload,
    )
    .await;
    let [OnionCircuitEffect::SealAndSend {
        to,
        recipient,
        bucket,
        encoded_message,
        ..
    }] = second_transition.effects.as_slice()
    else {
        panic!("second relay must emit one padded exit cell");
    };
    assert_eq!(*to, exit.delegator_did());
    let exit_edge_id = match rings_codec::deserialize(encoded_message.as_ref())
        .expect("decode exit-hop message")
    {
        OnionWireMessage::Forward(frame) => frame.circuit_id,
        OnionWireMessage::Backward(_) => panic!("forward route emitted a backward message"),
        OnionWireMessage::Cover => panic!("forward route emitted a cover message"),
        OnionWireMessage::Destroy { .. } => panic!("forward route emitted a destroy message"),
    };
    assert_ne!(exit_edge_id, first_edge_id);
    assert_ne!(exit_edge_id, second_edge_id);
    let exit_payload =
        seal_encoded_message(encoded_message, *recipient, Some(*bucket)).expect("seal exit cell");
    assert_eq!(first_payload.len(), exit_payload.len());

    let exit_protocol =
        OnionCircuitProtocol::new(OnionCircuitCapabilities::Exit(TEST_PROCESS_EPOCH));
    let exit_shell = OnionCircuitShell::new(exit.clone(), RecordingHandler::default());
    let exit_scope = test_scope(exit.clone());
    let exit_transition = peel_forward_cell(
        &exit_protocol,
        &exit_shell,
        &exit_scope,
        &exit_protocol.init(),
        second.delegator_did(),
        exit.delegator_did(),
        &exit_payload,
    )
    .await;
    assert!(matches!(
        exit_transition.effects.as_slice(),
        [OnionCircuitEffect::Exit { payload, .. }] if payload == &expected
    ));
    assert_eq!(first_transition.state.relay_return_count(), 1);
    assert_eq!(second_transition.state.relay_return_count(), 1);
}

/// A cell admitted by the original runtime is replayed against a fresh runtime that keeps the
/// same `DelegateeKey` but has an empty replay cache and a new process epoch. Epoch admission must
/// reject the cell before the reducer emits an exit effect, independently of cache persistence.
#[tokio::test]
async fn test_restarted_exit_rejects_old_epoch_before_replay_state_or_side_effect() {
    let client = session();
    let guard = session();
    let relay = session();
    let exit = session();
    let route = route(&guard, &relay, &exit);
    let original_epoch = route.exit().process_epoch;
    let circuit_id = RESTART_REPLAY_CIRCUIT_ID;
    let (_, payload) = encode_initial_forward(
        OnionClientReturn::new(client.delegatee_public_key()),
        &route,
        circuit_id,
        test_payload("one-shot"),
    )
    .expect("encode original process cell");
    let payload = relay_to_terminal(&[&guard, &relay], &payload);

    let original_protocol =
        OnionCircuitProtocol::new(OnionCircuitCapabilities::Exit(original_epoch));
    let original_handler = RecordingHandler::default();
    let original_shell = OnionCircuitShell::new(exit.clone(), original_handler.clone());
    let original_scope = test_scope(exit.clone());
    let original_transition = peel_forward_cell(
        &original_protocol,
        &original_shell,
        &original_scope,
        &original_protocol.init(),
        relay.delegator_did(),
        exit.delegator_did(),
        &payload,
    )
    .await;
    let [exit_effect] = original_transition.effects.as_slice() else {
        panic!("the original process must authorize one exit effect");
    };
    original_shell
        .run(&original_scope, exit_effect.clone())
        .await
        .expect("consume original replay witness");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        original_handler.wait_for_exit_count(1),
    )
    .await
    .expect("original exit effect completed");

    let restarted_handler = RecordingHandler::default();
    let restarted_protocol =
        OnionCircuitProtocol::new(OnionCircuitCapabilities::Exit(RESTARTED_PROCESS_EPOCH));
    let restarted_shell = OnionCircuitShell::new(exit.clone(), restarted_handler.clone());
    let restarted_scope = test_scope(exit.clone());
    let restarted_transition = peel_forward_cell(
        &restarted_protocol,
        &restarted_shell,
        &restarted_scope,
        &restarted_protocol.init(),
        relay.delegator_did(),
        exit.delegator_did(),
        &payload,
    )
    .await;

    assert!(restarted_transition.effects.is_empty());
    assert_eq!(restarted_handler.exit_count(), 0);
}

#[tokio::test]
async fn test_client_backward_payload_decryption_runs_in_shell_handler() {
    let client = session();
    let exit = session();
    let protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::Client);
    let handler = RecordingHandler::default();
    let shell = OnionCircuitShell::new(client.clone(), handler.clone());
    let scope = test_scope(client.clone());
    let state = protocol.init();
    let circuit_id = OnionCircuitId::new([3; 16]);
    let return_id = OnionReturnId::new([13; 16]);
    let expected_exit = route(&session(), &session(), &exit).exit().clone();
    let expected = test_payload("closed");
    let frame = OnionBackwardFrame {
        circuit_id,
        payload: encrypt_client_payload(
            return_id,
            expected.clone(),
            client.delegatee_public_key(),
            MessageSigner::new(&exit, TEST_NETWORK_ID),
        )
        .expect("encrypt backward"),
    };
    let payload = seal_message(
        &OnionWireMessage::Backward(frame),
        client.delegatee_public_key(),
        None,
    )
    .expect("encode backward cell");
    let event = decode_event(
        &protocol,
        exit.delegator_did(),
        client.delegator_did(),
        &payload,
    );
    let transition = protocol.step(
        Ctx {
            did: client.delegator_did(),
            state: &state,
        },
        event,
    );
    let [effect] = transition.effects.as_slice() else {
        panic!("expected timestamp effect");
    };
    let reinjected = shell
        .run(&scope, effect.clone())
        .await
        .expect("timestamp backward");
    let [local_payload] = reinjected.as_slice() else {
        panic!("expected local payload");
    };
    let event = decode_event(
        &protocol,
        client.delegator_did(),
        client.delegator_did(),
        local_payload,
    );
    let transition = protocol.step(
        Ctx {
            did: client.delegator_did(),
            state: &transition.state,
        },
        event,
    );
    let [effect] = transition.effects.as_slice() else {
        panic!("expected decrypt-client effect");
    };

    let outputs = shell
        .run(&scope, effect.clone())
        .await
        .expect("decrypt client");

    assert!(outputs.is_empty());
    let clients = handler.take_clients();
    let [(from, returned_circuit_id, authenticated)] = clients.as_slice() else {
        panic!("expected one client payload");
    };
    assert_eq!(*from, exit.delegator_did());
    assert_eq!(*returned_circuit_id, circuit_id);
    assert_eq!(
        authenticated
            .clone()
            .into_verified_payload(return_id, &expected_exit, TEST_NETWORK_ID)
            .expect("valid exit proof")
            .payload,
        expected
    );
}

/// The right fold places position `i` on hop `i`: each relay layer names its successor and
/// answers to its predecessor (the client at position zero), and the innermost layer carries the
/// world-facing application.
#[test]
fn test_forward_fold_places_each_position_on_its_hop() {
    let client = session();
    let first = session();
    let second = session();
    let exit = session();
    let route = route(&first, &second, &exit);
    let first_circuit_id = OnionCircuitId::new([51; 16]);
    let client_return = OnionClientReturn::new(client.delegatee_public_key());
    let (_, payload) = encode_initial_forward(
        client_return,
        &route,
        first_circuit_id,
        test_payload("fold"),
    )
    .expect("encode initial route");
    let OnionWireMessage::Forward(frame) = open_wire(&first, &payload) else {
        panic!("expected forward frame");
    };
    let OnionForwardLayer::Relay {
        next_hop: second_hop,
        next_circuit_id: second_circuit_id,
        next_delegatee_public_key: second_key,
        return_delegatee_public_key: first_return,
        inner,
    } = decrypt_forward_layer(&first, first_circuit_id, &frame.layer).expect("first layer")
    else {
        panic!("expected relay layer at position zero");
    };
    let OnionForwardLayer::Relay {
        next_hop: exit_hop,
        next_circuit_id: exit_circuit_id,
        next_delegatee_public_key: exit_key,
        return_delegatee_public_key: second_return,
        inner,
    } = decrypt_forward_layer(&second, second_circuit_id, &inner).expect("second layer")
    else {
        panic!("expected relay layer at position one");
    };
    let OnionForwardLayer::Exit {
        client: exit_client,
        return_delegatee_public_key: exit_return,
        payload: exit_payload,
        ..
    } = decrypt_forward_layer(&exit, exit_circuit_id, &inner).expect("exit layer")
    else {
        panic!("expected exit layer at the world-facing position");
    };

    assert_eq!(
        (second_hop, second_key, first_return),
        (
            second.delegator_did(),
            second.delegatee_public_key(),
            client.delegatee_public_key()
        )
    );
    assert_eq!(
        (exit_hop, exit_key, second_return),
        (
            exit.delegator_did(),
            exit.delegatee_public_key(),
            first.delegatee_public_key()
        )
    );
    assert_eq!(
        (exit_client, exit_return, exit_payload),
        (
            client_return,
            second.delegatee_public_key(),
            test_payload("fold")
        )
    );
}

/// The algebra is one lookup on the frame's symbol: a registered symbol reaches its
/// interpretation, and a symbol without an entry is dropped.
#[tokio::test]
async fn test_algebra_dispatches_on_the_frame_symbol() {
    let client = session();
    let exit = session();
    let scope = test_scope(exit.clone()).lifecycle();
    let tcp_exit = RecordingExit::default();
    let algebra = OnionAlgebra::default().register(OnionServiceName::tcp(), tcp_exit.clone());
    let frame = |service: &str, nonce: u8| OnionCircuitExitFrame {
        from: client.delegator_did(),
        circuit_id: OnionCircuitId::new([54; 16]),
        return_peer: client.delegator_did(),
        return_delegatee_public_key: client.delegatee_public_key(),
        client: OnionClientReturn::new(client.delegatee_public_key()),
        forward_nonce: OnionForwardNonce::new([nonce; 16]),
        forward_sequence: OnionForwardSequence::FIRST,
        payload: payload_for_service(service, "body"),
    };

    for (service, nonce) in [("tcp", 55), ("https", 56)] {
        algebra
            .evaluate(&scope, frame(service, nonce))
            .await
            .expect("evaluate exit frame");
    }

    assert_eq!(tcp_exit.exit_count.load(Ordering::SeqCst), 1);
}
