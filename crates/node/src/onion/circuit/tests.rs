use std::sync::Arc;
use std::sync::Mutex;

use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::session::SessionSk;

use super::codec::encode_wire_message;
use super::codec::OnionWireMessage;
use super::crypto::decrypt_client_payload;
use super::crypto::decrypt_forward_layer;
use super::crypto::encrypt_client_payload;
use super::limiter::OnionCryptoLimiter;
use super::protocol::OnionCircuitCapabilities;
use super::reducer::remember_return_hop;
use super::reducer::RelayReturnKey;
use super::*;
use crate::extension::ext::Ctx;
use crate::extension::ext::Extensions;
use crate::extension::ext::Interpret;
use crate::extension::ext::Protocol;
use crate::extension::ext::Scope;
use crate::extension::ext::Wire;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitService;
use crate::onion::OnionExitTransport;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteHop;
use crate::online::OnlineNodeType;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

fn session() -> SessionSk {
    SessionSk::new_with_seckey(&SecretKey::random()).expect("session key")
}

fn test_payload(label: &str) -> OnionCircuitPayload {
    OnionCircuitPayload::new("test", Bytes::copy_from_slice(label.as_bytes()))
}

fn route(relays: &[SessionSk], exit_session: &SessionSk) -> OnionRoute {
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
    let hops = encryption_hops
        .iter()
        .map(|hop| hop.did)
        .collect::<Vec<_>>();
    OnionRoute {
        service: "https".to_string(),
        hops,
        encryption_hops,
        exit: OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did: exit,
                public_key,
                session_public_key: exit_session.session_public_key(),
                node_type: OnlineNodeType::Native,
                network_id: 1,
                services: vec![OnionExitService {
                    name: "https".to_string(),
                    transport: OnionExitTransport::Https,
                }],
                policy: Default::default(),
                started_at_ms: 0,
                heartbeat_at_ms: 0,
                expires_at_ms: 1,
                version: "test".to_string(),
            },
            exit_session,
        )
        .expect("signed exit"),
    }
}

fn decode_event(
    protocol: &OnionCircuitProtocol,
    from: Did,
    me: Did,
    payload: &Bytes,
) -> super::codec::OnionCircuitEvent {
    protocol
        .decode(Wire {
            from,
            me,
            payload: payload.as_ref(),
        })
        .expect("decode onion circuit event")
}

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

#[derive(Clone, Default)]
struct RecordingHandler {
    clients: Arc<Mutex<Vec<(Did, OnionCircuitId, OnionAuthenticatedPayload)>>>,
}

impl RecordingHandler {
    fn take_clients(&self) -> Vec<(Did, OnionCircuitId, OnionAuthenticatedPayload)> {
        std::mem::take(&mut self.clients.lock().expect("recorded clients"))
    }
}

#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
impl OnionCircuitHandler for RecordingHandler {
    async fn handle_exit(
        &self,
        _scope: &Scope,
        _from: Did,
        _circuit_id: OnionCircuitId,
        _return_peer: Did,
        _client: OnionClientReturn,
        _payload: OnionCircuitPayload,
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
fn route_first_hop_rejects_mismatched_hop_lists() {
    let first = session();
    let second = session();
    let exit = session();
    let mut route = route(&[first, second.clone()], &exit);
    let wrong_first = second.account_did();
    if let Some(first_hop) = route.hops.first_mut() {
        *first_hop = wrong_first;
    }

    assert!(matches!(
        route_first_hop(&route),
        Err(crate::error::Error::OnionRouteError(_))
    ));
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
    let expected_exit = route(&[], &exit).exit;
    let expected = test_payload("closed");
    let frame = OnionBackwardFrame {
        circuit_id,
        payload: encrypt_client_payload(
            circuit_id,
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
            .into_verified_payload(circuit_id, &expected_exit)
            .expect("valid exit proof"),
        expected
    );
}

#[test]
fn relay_return_table_evicts_expired_entries() {
    let previous = session();
    let next = session();
    let other_next = session();
    let mut state = OnionCircuitState::default();
    let first = RelayReturnKey {
        circuit_id: OnionCircuitId::new([1; 16]),
        next_hop: next.account_did(),
    };
    let second = RelayReturnKey {
        circuit_id: OnionCircuitId::new([2; 16]),
        next_hop: other_next.account_did(),
    };

    remember_return_hop(&mut state, 1, 10, first, previous.account_did(), 100)
        .expect("first return hop");
    assert!(remember_return_hop(&mut state, 1, 10, second, previous.account_did(), 105).is_err());

    remember_return_hop(&mut state, 1, 10, second, previous.account_did(), 111)
        .expect("expired entry evicted");
    assert_eq!(state.relay_return_count(), 1);
}

#[test]
fn relay_return_table_rejects_live_edge_overwrite() {
    let previous = session();
    let attacker_previous = session();
    let next = session();
    let mut state = OnionCircuitState::default();
    let key = RelayReturnKey {
        circuit_id: OnionCircuitId::new([7; 16]),
        next_hop: next.account_did(),
    };

    remember_return_hop(&mut state, 8, 10, key, previous.account_did(), 100)
        .expect("first return hop");

    assert!(matches!(
        remember_return_hop(&mut state, 8, 10, key, attacker_previous.account_did(), 101),
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
    assert!(decrypt_client_payload(&exit, circuit_id, &frame.layer).is_err());

    let backward = encrypt_client_payload(
        circuit_id,
        test_payload("tcp-close"),
        client.session_public_key(),
        &exit,
    )
    .expect("encrypt backward");
    assert!(decrypt_client_payload(&client, circuit_id, &backward).is_ok());
    assert!(decrypt_client_payload(&client, wrong_circuit_id, &backward).is_err());
    assert!(decrypt_forward_layer(&client, circuit_id, &backward).is_err());
}

#[test]
fn backward_payload_authentication_rejects_wrong_exit_signer() {
    let client = session();
    let exit = session();
    let attacker = session();
    let route = route(&[], &exit);
    let circuit_id = OnionCircuitId::new([8; 16]);
    let sealed = encrypt_client_payload(
        circuit_id,
        test_payload("forged"),
        client.session_public_key(),
        &attacker,
    )
    .expect("encrypt forged payload");

    let authenticated =
        decrypt_client_payload(&client, circuit_id, &sealed).expect("decrypt forged payload");

    assert!(matches!(
        authenticated.into_verified_payload(circuit_id, &route.exit),
        Err(crate::error::Error::OnionRouteError(_))
    ));
}
