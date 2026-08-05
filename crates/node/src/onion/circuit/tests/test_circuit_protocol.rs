#[cfg(rings_native)]
use std::sync::atomic::AtomicBool;
#[cfg(rings_native)]
use std::sync::atomic::Ordering;
#[cfg(rings_native)]
use std::sync::Arc;
#[cfg(rings_native)]
use std::sync::Mutex;

use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::session::SessionSk;

#[cfg(rings_native)]
use super::super::cell::encode_message;
use super::super::cell::open_cell;
#[cfg(rings_native)]
use super::super::cell::seal_encoded_message;
#[cfg(rings_native)]
use super::super::cell::seal_message;
use super::super::cell::OnionWireCell;
use super::super::codec::OnionCircuitInput;
use super::super::codec::OnionWireMessage;
use super::super::crypto::decrypt_forward_layer;
#[cfg(rings_native)]
use super::super::crypto::encrypt_client_payload;
use super::super::protocol::OnionCircuitCapabilities;
use super::super::reducer::OnionCircuitReducer;
use super::super::reducer::RelayReturnEdge;
use super::super::reducer::RelayReturnKey;
#[cfg(rings_native)]
use super::super::send_outbox::OnionSendTestHook;
use super::super::*;
use crate::extension::ext::Ctx;
#[cfg(rings_native)]
use crate::extension::ext::EffectScope;
#[cfg(rings_native)]
use crate::extension::ext::Extensions;
#[cfg(rings_native)]
use crate::extension::ext::Interpret;
use crate::extension::ext::Protocol;
#[cfg(rings_native)]
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
#[cfg(rings_native)]
use crate::processor::ProcessorBuilder;
#[cfg(rings_native)]
use crate::processor::ProcessorConfig;

pub(super) fn session() -> SessionSk {
    SessionSk::new_with_seckey(&SecretKey::random()).expect("session key")
}

pub(super) fn open_wire(recipient: &SessionSk, payload: &[u8]) -> OnionWireMessage {
    let cell = bincode::deserialize::<OnionWireCell>(payload).expect("decode encrypted cell");
    open_cell(recipient, cell.bucket, &cell.sealed).expect("open encrypted cell")
}

pub(super) fn return_edge(
    key: RelayReturnKey,
    previous: &SessionSk,
    previous_circuit_id: OnionCircuitId,
) -> RelayReturnEdge {
    RelayReturnEdge {
        key,
        previous_hop: previous.account_did(),
        previous_circuit_id,
        previous_session_public_key: previous.session_public_key(),
    }
}

pub(super) fn test_payload(label: &str) -> OnionCircuitPayload {
    OnionCircuitPayload::new(
        OnionServiceName::https(),
        Bytes::copy_from_slice(label.as_bytes()),
    )
}

fn payload_for_service(service: &str, label: &str) -> OnionCircuitPayload {
    OnionCircuitPayload::try_new(service, Bytes::copy_from_slice(label.as_bytes()))
        .expect("valid payload service")
}

pub(super) fn route(relays: &[SessionSk], exit_session: &SessionSk) -> OnionRoute {
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
            service: OnionExitService::new("https", OnionExitTransport::Tcp)
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

#[cfg(rings_native)]
fn test_scope(session_sk: SessionSk) -> EffectScope {
    let config = ProcessorConfig::new(1, String::new(), session_sk, 1);
    let processor = ProcessorBuilder::from_config(&config)
        .expect("processor builder")
        .advertise_presence(false)
        .build()
        .expect("processor");
    let extensions = Extensions::new(Arc::new(processor));
    EffectScope::new(Scope::new(
        extensions.core(),
        ONION_CIRCUIT_NAMESPACE.to_string(),
    ))
}

#[cfg(rings_native)]
async fn peel_forward_cell(
    protocol: &OnionCircuitProtocol,
    shell: &OnionCircuitShell<RecordingHandler>,
    scope: &EffectScope,
    state: &OnionCircuitState,
    from: Did,
    me: Did,
    payload: &Bytes,
) -> crate::extension::ext::Transition<OnionCircuitState, OnionCircuitEffect> {
    let observed = protocol.step(
        Ctx { did: me, state },
        decode_event(protocol, from, me, payload),
    );
    let [decrypt_cell] = observed.effects.as_slice() else {
        panic!("expected cell decrypt effect");
    };
    let local = shell
        .run(scope, decrypt_cell.clone())
        .await
        .expect("decrypt cell");
    let [local] = local.as_slice() else {
        panic!("expected opened cell reinjection");
    };
    let opened = protocol.step(
        Ctx {
            did: me,
            state: &observed.state,
        },
        decode_event(protocol, me, me, local),
    );
    let [decrypt_layer] = opened.effects.as_slice() else {
        panic!("expected forward-layer decrypt effect");
    };
    let local = shell
        .run(scope, decrypt_layer.clone())
        .await
        .expect("decrypt forward layer");
    let [local] = local.as_slice() else {
        panic!("expected decrypted layer reinjection");
    };
    protocol.step(
        Ctx {
            did: me,
            state: &opened.state,
        },
        decode_event(protocol, me, me, local),
    )
}

#[cfg(rings_native)]
#[derive(Clone, Default)]
struct RecordingHandler {
    clients: Arc<Mutex<Vec<(Did, OnionCircuitId, OnionAuthenticatedPayload)>>>,
}

#[cfg(rings_native)]
impl RecordingHandler {
    fn take_clients(&self) -> Vec<(Did, OnionCircuitId, OnionAuthenticatedPayload)> {
        std::mem::take(&mut self.clients.lock().expect("recorded clients"))
    }
}

#[cfg(rings_native)]
#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
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

#[cfg(rings_native)]
#[derive(Clone, Default)]
struct BlockingExitHandler {
    started: Arc<AtomicBool>,
    started_notify: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[cfg(rings_native)]
impl BlockingExitHandler {
    async fn wait_until_started(&self) {
        while !self.started.load(Ordering::SeqCst) {
            self.started_notify.notified().await;
        }
    }

    fn release(&self) {
        self.release.notify_one();
    }
}

#[cfg(rings_native)]
#[async_trait::async_trait]
impl OnionCircuitHandler for BlockingExitHandler {
    async fn handle_exit(
        &self,
        _scope: &Scope,
        _frame: OnionCircuitExitFrame,
    ) -> crate::error::Result<()> {
        self.started.store(true, Ordering::SeqCst);
        self.started_notify.notify_waiters();
        self.release.notified().await;
        Ok(())
    }

    async fn handle_client(
        &self,
        _scope: &Scope,
        _from: Did,
        _circuit_id: OnionCircuitId,
        _payload: OnionAuthenticatedPayload,
    ) -> crate::error::Result<()> {
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
    let decoded = open_wire(&first, &payload);

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
    let OnionWireMessage::Forward(frame) = open_wire(&first, &payload) else {
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

#[cfg(rings_native)]
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

#[cfg(rings_native)]
fn relay_next_circuit_id(
    relay: &SessionSk,
    first_circuit_id: OnionCircuitId,
    payload: &Bytes,
) -> OnionCircuitId {
    let OnionWireMessage::Forward(frame) = open_wire(relay, payload) else {
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
fn hidden_cell_direction_defers_relay_capability_check_until_after_cell_decrypt() {
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

    assert!(matches!(transition.effects.as_slice(), [
        OnionCircuitEffect::DecryptCell { .. }
    ]));

    let cell = bincode::deserialize::<OnionWireCell>(&payload).expect("decode encrypted cell");
    let message = open_cell(&relay, cell.bucket, &cell.sealed).expect("open encrypted cell");
    let event = super::super::codec::OnionCircuitEvent {
        input: OnionCircuitInput::CellReady {
            from: client.account_did(),
            received_at_ms: 1,
            bucket: cell.bucket,
            message,
        },
    };
    let transition = protocol.step(
        Ctx {
            did: relay.account_did(),
            state: &transition.state,
        },
        event,
    );
    assert!(transition.effects.is_empty());
}

#[cfg(rings_native)]
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
    let [effect] = transition.effects.as_slice() else {
        panic!("expected forward-layer decrypt effect");
    };
    assert!(matches!(effect, OnionCircuitEffect::DecryptForward { .. }));
    let reinjected = shell
        .run(&scope, effect.clone())
        .await
        .expect("decrypt exit layer");
    let [local_payload] = reinjected.as_slice() else {
        panic!("expected decrypted forward layer");
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

#[cfg(rings_native)]
#[tokio::test]
async fn exit_effect_releases_transition_turn_before_adapter_io_completes() {
    let client = session();
    let exit = session();
    let handler = BlockingExitHandler::default();
    let shell = OnionCircuitShell::new(exit.clone(), handler.clone());
    let scope = test_scope(exit.clone());
    let effect = OnionCircuitEffect::Exit {
        from: client.account_did(),
        circuit_id: OnionCircuitId::new([27; 16]),
        return_peer: client.account_did(),
        return_session_public_key: client.session_public_key(),
        client: OnionClientReturn::new(client.session_public_key()),
        forward_nonce: OnionForwardNonce::new([28; 16]),
        forward_sequence: OnionForwardSequence::FIRST,
        payload: test_payload("blocking-exit"),
    };

    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        shell.run(&scope, effect),
    )
    .await
    .expect("exit interpretation must not await adapter I/O")
    .expect("spawn exit adapter");
    handler.wait_until_started().await;
    handler.release();
}

#[cfg(rings_native)]
#[tokio::test]
async fn send_effect_releases_transition_turn_and_preserves_peer_order() {
    let local = session();
    let peer = session();
    let hook = Arc::new(OnionSendTestHook::default());
    let shell = OnionCircuitShell::new_with_send_test_hook(
        local.clone(),
        RecordingHandler::default(),
        Arc::clone(&hook),
    );
    let scope = test_scope(local.clone());

    let messages = [1_u8, 2_u8].map(|tag| {
        OnionWireMessage::Backward(OnionBackwardFrame {
            circuit_id: OnionCircuitId::new([tag; 16]),
            payload: encrypt_client_payload(
                OnionReturnId::new([tag; 16]),
                test_payload("ordered"),
                local.session_public_key(),
                &local,
            )
            .expect("encrypt ordered fixture"),
        })
    });
    for message in &messages {
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            shell.run(&scope, OnionCircuitEffect::SealAndSend {
                to: peer.account_did(),
                recipient: peer.session_public_key(),
                bucket: OnionCellBucket::KiB4,
                encoded_message: encode_message(message).expect("encode ordered fixture"),
            }),
        )
        .await
        .expect("send interpretation must only enqueue")
        .expect("enqueue ordered send");
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_until_blocked())
        .await
        .expect("first overlay send reached blocking hook");
    tokio::task::yield_now().await;
    assert!(hook.observed().expect("observed sends").is_empty());

    hook.release();
    let observed =
        tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_for_observed(2))
            .await
            .expect("ordered drain completed")
            .expect("observed sends");
    let observed = observed
        .iter()
        .map(|payload| open_wire(&peer, payload))
        .collect::<Vec<_>>();
    assert_eq!(observed, messages);
    tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_for_covers(2))
        .await
        .expect("two real cells fill the other two fixed batch slots with cover");
    assert_eq!(hook.cover_count(), 2);
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
        bucket: OnionCellBucket::KiB4,
        circuit_id,
        layer: OnionForwardLayer::Exit {
            client: OnionClientReturn::new(client.session_public_key()),
            return_session_public_key: client.session_public_key(),
            expires_at_ms: 100,
            forward_nonce: OnionForwardNonce::new([9; 16]),
            forward_sequence: OnionForwardSequence::FIRST,
            payload: test_payload("expired"),
        },
    });

    assert_eq!(transition.state, state);
    assert!(transition.effects.is_empty());
}

#[cfg(rings_native)]
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

    assert!(matches!(
        transition.effects.as_slice(),
        [OnionCircuitEffect::SealAndSend { to, .. }] if *to == exit.account_did()
    ));
    assert_eq!(transition.state.relay_return_count(), 1);
}

#[cfg(rings_native)]
#[tokio::test]
async fn two_relays_peel_fixed_size_cells_through_the_exit_reducer_and_shell() {
    let client = session();
    let first = session();
    let second = session();
    let exit = session();
    let route = route(&[first.clone(), second.clone()], &exit);
    let expected = test_payload("multi-hop-fixed-cell");
    let first_edge_id = OnionCircuitId::new([31; 16]);
    let (first_peer, first_payload) = encode_initial_forward(
        OnionClientReturn::new(client.session_public_key()),
        &route,
        first_edge_id,
        expected.clone(),
    )
    .expect("encode multi-hop route");
    assert_eq!(first_peer, first.account_did());

    let first_protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::relay());
    let first_shell = OnionCircuitShell::new(first.clone(), RecordingHandler::default());
    let first_scope = test_scope(first.clone());
    let first_transition = peel_forward_cell(
        &first_protocol,
        &first_shell,
        &first_scope,
        &first_protocol.init(),
        client.account_did(),
        first.account_did(),
        &first_payload,
    )
    .await;
    let [OnionCircuitEffect::SealAndSend {
        to,
        recipient,
        bucket,
        encoded_message,
    }] = first_transition.effects.as_slice()
    else {
        panic!("first relay must emit one padded next-hop cell");
    };
    assert_eq!(*to, second.account_did());
    let second_edge_id =
        match bincode::deserialize(encoded_message.as_ref()).expect("decode second-hop message") {
            OnionWireMessage::Forward(frame) => frame.circuit_id,
            OnionWireMessage::Backward(_) => panic!("forward route emitted a backward message"),
            OnionWireMessage::Cover => panic!("forward route emitted a cover message"),
        };
    assert_ne!(second_edge_id, first_edge_id);
    let second_payload = seal_encoded_message(encoded_message, *recipient, Some(*bucket))
        .expect("seal second-hop cell");
    assert_eq!(first_payload.len(), second_payload.len());

    let second_protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::relay());
    let second_shell = OnionCircuitShell::new(second.clone(), RecordingHandler::default());
    let second_scope = test_scope(second.clone());
    let second_transition = peel_forward_cell(
        &second_protocol,
        &second_shell,
        &second_scope,
        &second_protocol.init(),
        first.account_did(),
        second.account_did(),
        &second_payload,
    )
    .await;
    let [OnionCircuitEffect::SealAndSend {
        to,
        recipient,
        bucket,
        encoded_message,
    }] = second_transition.effects.as_slice()
    else {
        panic!("second relay must emit one padded exit cell");
    };
    assert_eq!(*to, exit.account_did());
    let exit_edge_id =
        match bincode::deserialize(encoded_message.as_ref()).expect("decode exit-hop message") {
            OnionWireMessage::Forward(frame) => frame.circuit_id,
            OnionWireMessage::Backward(_) => panic!("forward route emitted a backward message"),
            OnionWireMessage::Cover => panic!("forward route emitted a cover message"),
        };
    assert_ne!(exit_edge_id, first_edge_id);
    assert_ne!(exit_edge_id, second_edge_id);
    let exit_payload =
        seal_encoded_message(encoded_message, *recipient, Some(*bucket)).expect("seal exit cell");
    assert_eq!(first_payload.len(), exit_payload.len());

    let exit_protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::exit());
    let exit_shell = OnionCircuitShell::new(exit.clone(), RecordingHandler::default());
    let exit_scope = test_scope(exit.clone());
    let exit_transition = peel_forward_cell(
        &exit_protocol,
        &exit_shell,
        &exit_scope,
        &exit_protocol.init(),
        second.account_did(),
        exit.account_did(),
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

#[cfg(rings_native)]
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
    let payload = seal_message(
        &OnionWireMessage::Backward(frame),
        client.session_public_key(),
        None,
    )
    .expect("encode backward cell");
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
