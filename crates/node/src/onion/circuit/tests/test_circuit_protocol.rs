#[cfg(feature = "node")]
use std::sync::Arc;
#[cfg(feature = "node")]
use std::sync::Mutex;

use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::session::SessionSk;

#[cfg(feature = "node")]
use super::super::codec::encode_wire_message;
use super::super::codec::OnionCircuitInput;
use super::super::codec::OnionWireMessage;
use super::super::crypto::decrypt_client_payload;
use super::super::crypto::decrypt_forward_layer;
use super::super::crypto::encrypt_client_payload;
use super::super::limiter::OnionCryptoLimiter;
use super::super::protocol::OnionCircuitCapabilities;
use super::super::reducer::remember_return_hop;
use super::super::reducer::OnionCircuitReducer;
use super::super::reducer::RelayReturnKey;
use super::super::*;
use crate::extension::ext::Ctx;
#[cfg(feature = "node")]
use crate::extension::ext::Extensions;
#[cfg(feature = "node")]
use crate::extension::ext::Interpret;
use crate::extension::ext::Protocol;
#[cfg(feature = "node")]
use crate::extension::ext::Scope;
use crate::extension::ext::Wire;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitService;
use crate::onion::OnionExitTransport;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteHop;
use crate::onion::OnionServiceName;
use crate::online::OnlineNodeType;
#[cfg(feature = "node")]
use crate::processor::ProcessorBuilder;
#[cfg(feature = "node")]
use crate::processor::ProcessorConfig;

fn session() -> SessionSk {
    SessionSk::new_with_seckey(&SecretKey::random()).expect("session key")
}

fn test_payload(label: &str) -> OnionCircuitPayload {
    OnionCircuitPayload::new(
        OnionServiceName::https(),
        Bytes::copy_from_slice(label.as_bytes()),
    )
}

fn payload_for_service(service: &str, label: &str) -> OnionCircuitPayload {
    OnionCircuitPayload::try_new(service, Bytes::copy_from_slice(label.as_bytes()))
        .expect("valid payload service")
}

fn route(relays: &[SessionSk], exit_session: &SessionSk) -> OnionRoute {
    route_for_service("https", relays, exit_session)
}

fn route_for_service(service: &str, relays: &[SessionSk], exit_session: &SessionSk) -> OnionRoute {
    let exit = exit_session.account_did();
    let public_key = exit_session
        .session()
        .account_verification_pubkey()
        .expect("verification key");
    let mut encryption_hops = relays
        .iter()
        .map(|relay| OnionRouteHop::new(relay.account_did(), relay.session_public_key()))
        .collect::<Vec<_>>();
    encryption_hops.push(OnionRouteHop::new(exit, exit_session.session_public_key()));
    let exit = OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did: exit,
            public_key,
            session_public_key: exit_session.session_public_key(),
            node_type: OnlineNodeType::Native,
            network_id: 1,
            service: OnionExitService::new("https", OnionExitTransport::Https)
                .expect("valid test service"),
            policy: Default::default(),
            started_at_ms: 0,
            heartbeat_at_ms: 0,
            expires_at_ms: 1,
            version: "test".to_string(),
        },
        exit_session,
    )
    .expect("signed exit");
    OnionRoute::new(
        OnionServiceName::parse(service).expect("valid route service"),
        encryption_hops,
        exit,
    )
    .expect("valid route")
}

fn decode_event(
    protocol: &OnionCircuitProtocol,
    from: Did,
    me: Did,
    payload: &Bytes,
) -> super::super::codec::OnionCircuitEvent {
    protocol
        .decode(Wire {
            from,
            me,
            payload: payload.as_ref(),
        })
        .expect("decode onion circuit event")
}

#[cfg(feature = "node")]
fn test_scope(session_sk: SessionSk) -> Scope {
    let config = ProcessorConfig::new(1, String::new(), session_sk, 1);
    let processor = ProcessorBuilder::from_config(&config)
        .expect("processor builder")
        .advertise_presence(false)
        .build()
        .expect("processor");
    let extensions = Extensions::new(Arc::new(processor));
    Scope::new(extensions.core(), ONION_CIRCUIT_NAMESPACE.to_string())
}

#[cfg(feature = "node")]
#[derive(Clone, Default)]
struct RecordingHandler {
    clients: Arc<Mutex<Vec<(Did, OnionCircuitId, OnionAuthenticatedPayload)>>>,
}

#[cfg(feature = "node")]
impl RecordingHandler {
    fn take_clients(&self) -> Vec<(Did, OnionCircuitId, OnionAuthenticatedPayload)> {
        std::mem::take(&mut self.clients.lock().expect("recorded clients"))
    }
}

#[cfg(feature = "node")]
#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
impl OnionCircuitHandler for RecordingHandler {
    async fn handle_exit(
        &self,
        _scope: &Scope,
        _frame: OnionCircuitExitFrame,
    ) -> crate::error::Result<()> {
        Ok(())
    }

    async fn handle_client(
        &self,
        _scope: &Scope,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: OnionAuthenticatedPayload,
    ) -> crate::error::Result<()> {
        self.clients
            .lock()
            .map_err(|_| crate::error::Error::Lock)?
            .push((from, circuit_id, payload));
        Ok(())
    }
}

#[test]
fn initial_forward_targets_first_hop_and_hides_payload() {
    let client = session();
    let first = session();
    let second = session();
    let exit = session();
    let route = route(&[first.clone(), second], &exit);
    let circuit_id = OnionCircuitId::new([9; 16]);

    let (to, payload) = encode_initial_forward(
        OnionClientReturn::new(client.session_public_key()),
        &route,
        circuit_id,
        test_payload("probe"),
    )
    .expect("encode initial route");
    let decoded = bincode::deserialize::<OnionWireMessage>(&payload).expect("decode initial route");

    assert_eq!(to, first.account_did());
    let OnionWireMessage::Forward(frame) = decoded else {
        panic!("expected forward frame");
    };
    assert_eq!(frame.circuit_id, circuit_id);
    assert!(!format!("{frame:?}").contains(&format!("{:?}", client.account_did())));
    assert!(!format!("{:?}", frame.layer).contains("probe"));
}

#[test]
fn relay_layer_uses_distinct_next_edge_circuit_id() {
    let client = session();
    let first = session();
    let exit = session();
    let route = route(std::slice::from_ref(&first), &exit);
    let first_circuit_id = OnionCircuitId::new([9; 16]);
    let (_, payload) = encode_initial_forward(
        OnionClientReturn::new(client.session_public_key()),
        &route,
        first_circuit_id,
        test_payload("probe"),
    )
    .expect("encode initial route");
    let OnionWireMessage::Forward(frame) =
        bincode::deserialize::<OnionWireMessage>(&payload).expect("decode initial route")
    else {
        panic!("expected forward frame");
    };
    let OnionForwardLayer::Relay {
        next_circuit_id, ..
    } = decrypt_forward_layer(&first, first_circuit_id, &frame.layer).expect("decrypt relay layer")
    else {
        panic!("expected relay layer");
    };

    assert_ne!(next_circuit_id, first_circuit_id);
}

#[cfg(feature = "node")]
#[test]
fn circuit_path_reuses_edge_ids_for_stream_payloads() {
    let client = session();
    let first = session();
    let exit = session();
    let route = route(std::slice::from_ref(&first), &exit);
    let first_circuit_id = OnionCircuitId::new([9; 16]);
    let client_return = OnionClientReturn::new(client.session_public_key());
    let path = OnionCircuitPath::new(route, first_circuit_id).expect("stable circuit path");

    let (_, first_payload) = path
        .encode_forward(client_return, test_payload("first"))
        .expect("encode first payload");
    let (_, second_payload) = path
        .encode_forward(client_return, test_payload("second"))
        .expect("encode second payload");

    let first_next = relay_next_circuit_id(&first, first_circuit_id, &first_payload);
    let second_next = relay_next_circuit_id(&first, first_circuit_id, &second_payload);

    assert_eq!(first_next, second_next);
}

#[cfg(feature = "node")]
fn relay_next_circuit_id(
    relay: &SessionSk,
    first_circuit_id: OnionCircuitId,
    payload: &Bytes,
) -> OnionCircuitId {
    let OnionWireMessage::Forward(frame) =
        bincode::deserialize::<OnionWireMessage>(payload).expect("decode forward frame")
    else {
        panic!("expected forward frame");
    };
    assert_eq!(frame.circuit_id, first_circuit_id);
    let OnionForwardLayer::Relay {
        next_circuit_id, ..
    } = decrypt_forward_layer(relay, first_circuit_id, &frame.layer).expect("decrypt relay layer")
    else {
        panic!("expected relay layer");
    };
    next_circuit_id
}

#[test]
fn route_constructor_rejects_mismatched_exit_hop() {
    let first = session();
    let exit = session();
    let route = route(&[], &exit);
    let encryption_hops = vec![OnionRouteHop::new(
        first.account_did(),
        first.session_public_key(),
    )];

    assert!(matches!(
        OnionRoute::new(
            route.service_name().clone(),
            encryption_hops,
            route.exit().clone(),
        ),
        Err(crate::error::Error::OnionRouteError(_))
    ));
}

#[test]
fn initial_forward_requires_route_payload_service_match() {
    let client = session();
    let exit = session();
    let route = route(&[], &exit);
    let circuit_id = OnionCircuitId::new([9; 16]);

    assert!(matches!(
        encode_initial_forward(
            OnionClientReturn::new(client.session_public_key()),
            &route,
            circuit_id,
            payload_for_service("tcp", "wrong-service"),
        ),
        Err(crate::error::Error::OnionRouteError(_))
    ));
}

#[test]
fn initial_forward_accepts_canonical_payload_for_mixed_case_route_service() {
    let client = session();
    let exit = session();
    let route = route_for_service("HTTPS", &[], &exit);
    let circuit_id = OnionCircuitId::new([10; 16]);

    let result = encode_initial_forward(
        OnionClientReturn::new(client.session_public_key()),
        &route,
        circuit_id,
        payload_for_service("https", "canonical-service"),
    );

    assert!(result.is_ok());
}

#[test]
fn relay_forward_requires_opt_in_before_crypto_effect() {
    let client = session();
    let relay = session();
    let exit = session();
    let route = route(std::slice::from_ref(&relay), &exit);
    let circuit_id = OnionCircuitId::new([1; 16]);
    let (_, payload) = encode_initial_forward(
        OnionClientReturn::new(client.session_public_key()),
        &route,
        circuit_id,
        test_payload("tcp-shutdown"),
    )
    .expect("encode forward");
    let protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::client());
    let event = decode_event(
        &protocol,
        client.account_did(),
        relay.account_did(),
        &payload,
    );

    let transition = protocol.step(
        Ctx {
            did: relay.account_did(),
            state: &protocol.init(),
        },
        event,
    );

    assert!(transition.effects.is_empty());
}

#[cfg(feature = "node")]
#[tokio::test]
async fn relay_capability_does_not_execute_exit_layer() {
    let client = session();
    let relay = session();
    let route = route(&[], &relay);
    let circuit_id = OnionCircuitId::new([4; 16]);
    let (_, payload) = encode_initial_forward(
        OnionClientReturn::new(client.session_public_key()),
        &route,
        circuit_id,
        test_payload("tcp-shutdown"),
    )
    .expect("encode exit layer");
    let protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::relay());
    let shell = OnionCircuitShell::new(relay.clone(), RecordingHandler::default());
    let scope = test_scope(relay.clone());
    let state = protocol.init();
    let event = decode_event(
        &protocol,
        client.account_did(),
        relay.account_did(),
        &payload,
    );
    let transition = protocol.step(
        Ctx {
            did: relay.account_did(),
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
        relay.account_did(),
        relay.account_did(),
        local_payload,
    );

    let transition = protocol.step(
        Ctx {
            did: relay.account_did(),
            state: &transition.state,
        },
        event,
    );

    assert!(transition.effects.is_empty());
}

#[test]
fn expired_exit_layer_emits_no_exit_effect() {
    let client = session();
    let reducer = OnionCircuitReducer::new(OnionCircuitCapabilities::exit());
    let state = OnionCircuitState::default();
    let circuit_id = OnionCircuitId::new([8; 16]);

    let transition = reducer.apply(&state, OnionCircuitInput::ForwardReady {
        from: client.account_did(),
        received_at_ms: 100,
        circuit_id,
        layer: OnionForwardLayer::Exit {
            client: OnionClientReturn::new(client.session_public_key()),
            expires_at_ms: 100,
            forward_nonce: OnionForwardNonce::new([9; 16]),
            payload: test_payload("expired"),
        },
    });

    assert_eq!(transition.state, state);
    assert!(transition.effects.is_empty());
}

#[cfg(feature = "node")]
#[tokio::test]
async fn relay_decrypts_one_layer_and_remembers_return_hop() {
    let client = session();
    let relay = session();
    let exit = session();
    let route = route(std::slice::from_ref(&relay), &exit);
    let circuit_id = OnionCircuitId::new([2; 16]);
    let (_, payload) = encode_initial_forward(
        OnionClientReturn::new(client.session_public_key()),
        &route,
        circuit_id,
        test_payload("tcp-shutdown"),
    )
    .expect("encode forward");
    let protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::relay());
    let shell = OnionCircuitShell::new(relay.clone(), RecordingHandler::default());
    let scope = test_scope(relay.clone());
    let event = decode_event(
        &protocol,
        client.account_did(),
        relay.account_did(),
        &payload,
    );
    let state = protocol.init();

    let transition = protocol.step(
        Ctx {
            did: relay.account_did(),
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
        relay.account_did(),
        relay.account_did(),
        local_payload,
    );

    let transition = protocol.step(
        Ctx {
            did: relay.account_did(),
            state: &transition.state,
        },
        event,
    );

    assert_eq!(transition.effects.len(), 1);
    assert!(matches!(
        transition.effects.as_slice(),
        [OnionCircuitEffect::Send { to, .. }] if *to == exit.account_did()
    ));
    assert_eq!(transition.state.relay_return_count(), 1);
}

#[cfg(feature = "node")]
#[tokio::test]
async fn client_backward_payload_decryption_runs_in_shell_handler() {
    let client = session();
    let exit = session();
    let protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::client());
    let handler = RecordingHandler::default();
    let shell = OnionCircuitShell::new(client.clone(), handler.clone());
    let scope = test_scope(client.clone());
    let state = protocol.init();
    let circuit_id = OnionCircuitId::new([3; 16]);
    let return_id = OnionReturnId::new([13; 16]);
    let expected_exit = route(&[], &exit).exit().clone();
    let expected = test_payload("closed");
    let frame = OnionBackwardFrame {
        circuit_id,
        payload: encrypt_client_payload(
            return_id,
            expected.clone(),
            client.session_public_key(),
            &exit,
        )
        .expect("encrypt backward"),
    };
    let payload = encode_wire_message(OnionWireMessage::Backward(frame)).expect("encode backward");
    let event = decode_event(
        &protocol,
        exit.account_did(),
        client.account_did(),
        &payload,
    );
    let transition = protocol.step(
        Ctx {
            did: client.account_did(),
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
        client.account_did(),
        client.account_did(),
        local_payload,
    );
    let transition = protocol.step(
        Ctx {
            did: client.account_did(),
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
    assert_eq!(*from, exit.account_did());
    assert_eq!(*returned_circuit_id, circuit_id);
    assert_eq!(
        authenticated
            .clone()
            .into_verified_payload(return_id, &expected_exit)
            .expect("valid exit proof")
            .payload,
        expected
    );
}

#[test]
fn relay_return_table_evicts_expired_entries() {
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
        first,
        previous.account_did(),
        previous_circuit_id,
        100,
    )
    .expect("first return hop");
    assert!(remember_return_hop(
        &mut state,
        1,
        10,
        second,
        previous.account_did(),
        previous_circuit_id,
        105,
    )
    .is_err());

    remember_return_hop(
        &mut state,
        1,
        10,
        second,
        previous.account_did(),
        previous_circuit_id,
        111,
    )
    .expect("expired entry evicted");
    assert_eq!(state.relay_return_count(), 1);
}

#[test]
fn relay_return_table_rejects_live_edge_overwrite() {
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
        key,
        previous.account_did(),
        previous_circuit_id,
        100,
    )
    .expect("first return hop");

    assert!(matches!(
        remember_return_hop(
            &mut state,
            8,
            10,
            key,
            attacker_previous.account_did(),
            previous_circuit_id,
            101,
        ),
        Err(crate::error::Error::OnionRouteError(_))
    ));
    assert_eq!(state.relay_return_count(), 1);
}

#[test]
fn crypto_limiter_bounds_sender_window() {
    let peer = session().account_did();
    let mut limiter = OnionCryptoLimiter::with_limit(2);

    assert!(limiter.admit(peer, 100).is_ok());
    assert!(limiter.admit(peer, 101).is_ok());
    assert!(matches!(
        limiter.admit(peer, 102),
        Err(crate::error::Error::NoPermission)
    ));
    assert!(limiter
        .admit(peer, 100 + ONION_CRYPTO_LIMIT_WINDOW_MS)
        .is_ok());
}

#[test]
fn aead_context_binds_direction_and_circuit_id() {
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
    let OnionWireMessage::Forward(frame) =
        bincode::deserialize::<OnionWireMessage>(&forward_payload).expect("decode forward")
    else {
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
fn backward_payload_authentication_rejects_wrong_exit_signer() {
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
