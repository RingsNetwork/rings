use super::super::codec::OnionCircuitInput;
use super::super::codec::OnionWireMessage;
use super::super::crypto::decrypt_client_payload;
use super::super::crypto::decrypt_forward_layer;
use super::super::crypto::edge_circuit_ids_with;
use super::super::crypto::encrypt_client_payload;
use super::super::limiter::OnionCryptoLimiter;
use super::super::protocol::OnionCircuitCapabilities;
use super::super::reducer::remember_return_hop;
use super::super::reducer::OnionCircuitReducer;
use super::super::reducer::RelayReturnKey;
use super::super::*;
use super::test_circuit_protocol::open_wire;
use super::test_circuit_protocol::return_edge;
use super::test_circuit_protocol::route;
use super::test_circuit_protocol::session;
use super::test_circuit_protocol::test_payload;

#[test]
fn test_relay_return_table_evicts_expired_entries() {
    let previous = session();
    let next = session();
    let other_next = session();
    let mut state = OnionCircuitState::default();
    let previous_circuit_id = OnionCircuitId::new([11; 16]);
    let first = RelayReturnKey {
        circuit_id: OnionCircuitId::new([1; 16]),
        next_hop: next.account_did(),
    };
    let second = RelayReturnKey {
        circuit_id: OnionCircuitId::new([2; 16]),
        next_hop: other_next.account_did(),
    };

    remember_return_hop(
        &mut state,
        1,
        10,
        return_edge(first, &previous, previous_circuit_id),
        100,
    )
    .expect("first return hop");
    assert!(remember_return_hop(
        &mut state,
        1,
        10,
        return_edge(second, &previous, previous_circuit_id),
        105,
    )
    .is_err());

    remember_return_hop(
        &mut state,
        1,
        10,
        return_edge(second, &previous, previous_circuit_id),
        111,
    )
    .expect("expired entry evicted");
    assert_eq!(state.relay_return_count(), 1);
}

#[test]
fn test_backward_cell_after_return_expiry_is_never_forwarded() {
    let client = session();
    let next = session();
    let mut state = OnionCircuitState::default();
    let next_circuit_id = OnionCircuitId::new([41; 16]);
    remember_return_hop(
        &mut state,
        8,
        10,
        return_edge(
            RelayReturnKey {
                circuit_id: next_circuit_id,
                next_hop: next.account_did(),
            },
            &client,
            OnionCircuitId::new([42; 16]),
        ),
        100,
    )
    .expect("remember live return edge");
    let backward = OnionWireMessage::Backward(OnionBackwardFrame {
        circuit_id: next_circuit_id,
        payload: encrypt_client_payload(
            OnionReturnId::new([43; 16]),
            test_payload("expired-return"),
            client.session_public_key(),
            &next,
        )
        .expect("encrypt backward fixture"),
    });
    let reducer = OnionCircuitReducer::new(OnionCircuitCapabilities::relay());
    let transition = reducer.apply(&state, OnionCircuitInput::CellReady {
        from: next.account_did(),
        received_at_ms: 111,
        bucket: OnionCellBucket::KiB4,
        message: backward,
    });

    assert_eq!(transition.state.relay_return_count(), 0);
    assert!(matches!(transition.effects.as_slice(), [
        OnionCircuitEffect::DecryptClient { .. }
    ]));
    assert!(!transition
        .effects
        .iter()
        .any(|effect| matches!(effect, OnionCircuitEffect::SealAndSend { .. })));
}

#[test]
fn test_relay_return_table_rejects_live_edge_overwrite() {
    let previous = session();
    let attacker_previous = session();
    let next = session();
    let mut state = OnionCircuitState::default();
    let previous_circuit_id = OnionCircuitId::new([6; 16]);
    let key = RelayReturnKey {
        circuit_id: OnionCircuitId::new([7; 16]),
        next_hop: next.account_did(),
    };

    remember_return_hop(
        &mut state,
        8,
        10,
        return_edge(key, &previous, previous_circuit_id),
        100,
    )
    .expect("first return hop");

    assert!(matches!(
        remember_return_hop(
            &mut state,
            8,
            10,
            return_edge(key, &attacker_previous, previous_circuit_id),
            101,
        ),
        Err(crate::error::Error::OnionRouteError(_))
    ));
    assert_eq!(state.relay_return_count(), 1);
}

#[test]
fn test_relay_return_table_preserves_capacity_for_other_authenticated_peers() {
    let first_peer = session();
    let second_peer = session();
    let next = session();
    let mut state = OnionCircuitState::default();

    for circuit_byte in [1, 2] {
        remember_return_hop(
            &mut state,
            32,
            100,
            return_edge(
                RelayReturnKey {
                    circuit_id: OnionCircuitId::new([circuit_byte; 16]),
                    next_hop: next.account_did(),
                },
                &first_peer,
                OnionCircuitId::new([circuit_byte + 10; 16]),
            ),
            1,
        )
        .expect("first peer's bounded share");
    }

    assert!(matches!(
        remember_return_hop(
            &mut state,
            32,
            100,
            return_edge(
                RelayReturnKey {
                    circuit_id: OnionCircuitId::new([3; 16]),
                    next_hop: next.account_did(),
                },
                &first_peer,
                OnionCircuitId::new([13; 16]),
            ),
            1,
        ),
        Err(crate::error::Error::OnionRouteError(
            crate::onion::OnionRouteError::RelayPeerTableFull
        ))
    ));
    remember_return_hop(
        &mut state,
        32,
        100,
        return_edge(
            RelayReturnKey {
                circuit_id: OnionCircuitId::new([4; 16]),
                next_hop: next.account_did(),
            },
            &second_peer,
            OnionCircuitId::new([14; 16]),
        ),
        1,
    )
    .expect("another peer retains capacity");
}

#[test]
fn test_crypto_limiter_bounds_sender_window() {
    let peer = session().account_did();
    let mut limiter = OnionCryptoLimiter::with_limit(2);

    assert!(limiter.admit(peer, 100, 0).is_ok());
    assert!(limiter.admit(peer, 101, 0).is_ok());
    assert!(matches!(
        limiter.admit(peer, 102, 0),
        Err(crate::error::Error::NoPermission)
    ));
    assert!(limiter
        .admit(peer, 100 + ONION_CRYPTO_LIMIT_WINDOW_MS, 0)
        .is_ok());
}

#[test]
fn test_one_hop_cover_cell_has_no_state_transition_or_effect() {
    let peer = session();
    let reducer = OnionCircuitReducer::new(OnionCircuitCapabilities::relay());
    let state = OnionCircuitState::default();

    let transition = reducer.apply(&state, OnionCircuitInput::CellReady {
        from: peer.account_did(),
        received_at_ms: 1,
        bucket: OnionCellBucket::KiB4,
        message: OnionWireMessage::Cover,
    });

    assert_eq!(transition.state, state);
    assert!(transition.effects.is_empty());
}

#[test]
fn test_edge_circuit_id_allocation_retries_collisions_and_fails_boundedly() {
    let first = OnionCircuitId::new([1; 16]);
    let second = OnionCircuitId::new([2; 16]);
    let third = OnionCircuitId::new([3; 16]);
    let mut candidates = [first, second, second, third].into_iter();

    let ids = edge_circuit_ids_with(3, first, || {
        candidates.next().expect("bounded collision fixture")
    })
    .expect("unique candidates eventually succeed");
    assert_eq!(ids, vec![first, second, third]);

    assert!(matches!(
        edge_circuit_ids_with(2, first, || first),
        Err(crate::error::Error::OnionRouteError(
            crate::onion::OnionRouteError::CircuitIdAllocationFailed
        ))
    ));
}

#[test]
fn test_aead_context_binds_direction_and_circuit_id() {
    let client = session();
    let exit = session();
    let route = route(&[], &exit);
    let circuit_id = OnionCircuitId::new([5; 16]);
    let wrong_circuit_id = OnionCircuitId::new([6; 16]);
    let (_, forward_payload) = encode_initial_forward(
        OnionClientReturn::new(client.session_public_key()),
        &route,
        circuit_id,
        test_payload("tcp-shutdown"),
    )
    .expect("encode forward");
    let OnionWireMessage::Forward(frame) = open_wire(&exit, &forward_payload) else {
        panic!("expected forward frame");
    };

    assert!(decrypt_forward_layer(&exit, circuit_id, &frame.layer).is_ok());
    assert!(decrypt_forward_layer(&exit, wrong_circuit_id, &frame.layer).is_err());
    assert!(decrypt_client_payload(&exit, &frame.layer).is_err());

    let return_id = OnionReturnId::new([15; 16]);
    let wrong_return_id = OnionReturnId::new([16; 16]);
    let backward = encrypt_client_payload(
        return_id,
        test_payload("tcp-close"),
        client.session_public_key(),
        &exit,
    )
    .expect("encrypt backward");
    let authenticated = decrypt_client_payload(&client, &backward).expect("decrypt backward");
    assert!(authenticated
        .clone()
        .into_verified_payload(return_id, route.exit())
        .is_ok());
    assert!(authenticated
        .into_verified_payload(wrong_return_id, route.exit())
        .is_err());
    assert!(decrypt_forward_layer(&client, wrong_circuit_id, &backward).is_err());
}

#[test]
fn test_backward_payload_authentication_rejects_wrong_exit_signer() {
    let client = session();
    let exit = session();
    let attacker = session();
    let route = route(&[], &exit);
    let return_id = OnionReturnId::new([8; 16]);
    let sealed = encrypt_client_payload(
        return_id,
        test_payload("forged"),
        client.session_public_key(),
        &attacker,
    )
    .expect("encrypt forged payload");

    let authenticated = decrypt_client_payload(&client, &sealed).expect("decrypt forged payload");

    assert!(matches!(
        authenticated.into_verified_payload(return_id, route.exit()),
        Err(crate::error::Error::OnionRouteError(_))
    ));
}
