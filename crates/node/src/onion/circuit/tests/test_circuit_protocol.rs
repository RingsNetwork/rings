use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::message::MessageSigner;

use super::super::cell::encode_message;
use super::super::cell::open_cell;
use super::super::cell::seal_message;
use super::super::cell::OnionWireCell;
use super::super::codec::OnionCircuitInput;
use super::super::codec::OnionWireMessage;
use super::super::crypto::decrypt_forward_layer;
use super::super::crypto::encrypt_client_payload;
use super::super::protocol::OnionCircuitCapabilities;
use super::super::reducer::RelayReturnEdge;
use super::super::reducer::RelayReturnKey;
use super::super::send_outbox::OnionSendTestHook;
use super::super::*;
use crate::extension::ext::Ctx;
use crate::extension::ext::EffectScope;
use crate::extension::ext::Interpret;
use crate::extension::ext::Protocol;
use crate::extension::ext::Scope;
use crate::extension::ext::Wire;
use crate::onion::replay::OnionForwardReplayKey;
use crate::onion::replay::OnionForwardReplayPartitions;
use crate::onion::replay::ReplayAdmission;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionLoop;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionRouteHop;
use crate::onion::OnionServiceName;
use crate::online::OnlineNodeType;
use crate::sync_lock::lock;
use crate::tests::TEST_NETWORK_ID;

/// Stable non-zero epoch used by ordinary circuit fixtures; only determinism matters here.
pub(super) const TEST_PROCESS_EPOCH: OnionProcessEpoch = OnionProcessEpoch::new([17; 16]);
/// A distinct epoch models a restarted process that reuses the same delegated delegatee key.
pub(super) const RESTARTED_PROCESS_EPOCH: OnionProcessEpoch = OnionProcessEpoch::new([42; 16]);
/// Dedicated circuit id keeps the restart replay witness separate from neighboring fixtures.
pub(super) const RESTART_REPLAY_CIRCUIT_ID: OnionCircuitId = OnionCircuitId::new([41; 16]);

pub(super) fn session() -> DelegateeKey {
    DelegateeKey::new_with_seckey(&SecretKey::random()).expect("delegatee key")
}

pub(super) fn open_wire(recipient: &DelegateeKey, payload: &[u8]) -> OnionWireMessage {
    let cell = rings_codec::deserialize::<OnionWireCell>(payload).expect("decode encrypted cell");
    open_cell(recipient, cell.bucket, &cell.sealed).expect("open encrypted cell")
}

pub(super) fn return_edge(
    key: RelayReturnKey,
    previous: &DelegateeKey,
    previous_circuit_id: OnionCircuitId,
) -> RelayReturnEdge {
    RelayReturnEdge {
        key,
        previous_hop: previous.delegator_did(),
        previous_circuit_id,
        previous_delegatee_public_key: previous.delegatee_public_key(),
        // The return-table tests built on this edge never seal toward the next hop.
        next_delegatee_public_key: previous.delegatee_public_key(),
    }
}

pub(super) fn test_payload(label: &str) -> OnionCircuitPayload {
    OnionCircuitPayload::new(
        OnionServiceName::https(),
        Bytes::copy_from_slice(label.as_bytes()),
    )
}

pub(super) fn payload_for_service(service: &str, label: &str) -> OnionCircuitPayload {
    OnionCircuitPayload::try_new(service, Bytes::copy_from_slice(label.as_bytes()))
        .expect("valid payload service")
}

/// Hop of `session` at the fixture process epoch.
fn route_hop(session: &DelegateeKey) -> OnionRouteHop {
    OnionRouteHop::new(
        session.delegator_did(),
        session.delegatee_public_key(),
        TEST_PROCESS_EPOCH,
    )
}

/// The loop `guard, relay, symbol, back, guard` of a one-symbol pipeline.
pub(super) fn session_loop(
    guard: &DelegateeKey,
    relay: &DelegateeKey,
    symbol: &DelegateeKey,
    back: &DelegateeKey,
) -> OnionLoop<OnionRouteHop> {
    let mut relays = [guard, relay, back].into_iter().map(route_hop);
    OnionLoop::try_unfold(Vec::new(), route_hop(symbol), |_| {
        relays.next().ok_or(crate::error::Error::InvalidData)
    })
    .expect("session loop")
}

/// An `https` route whose Phase 1 view is `guard → relay → exit`; the loop returns through a
/// fresh relay.
pub(super) fn route(
    guard: &DelegateeKey,
    relay: &DelegateeKey,
    exit_session: &DelegateeKey,
) -> OnionRoute {
    route_for_service("https", guard, relay, exit_session)
}

/// A `service` route whose Phase 1 view is `guard → relay → exit`.
fn route_for_service(
    service: &str,
    guard: &DelegateeKey,
    relay: &DelegateeKey,
    exit_session: &DelegateeKey,
) -> OnionRoute {
    let public_key = exit_session
        .delegation()
        .delegator_verification_pubkey()
        .expect("verification key");
    let exit = OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did: exit_session.delegator_did(),
            public_key,
            delegatee_public_key: exit_session.delegatee_public_key(),
            process_epoch: TEST_PROCESS_EPOCH,
            node_type: OnlineNodeType::Native,
            network_id: TEST_NETWORK_ID,
            service: OnionServiceName::https(),
            policy: Default::default(),
            started_at_ms: 0,
            heartbeat_at_ms: 0,
            expires_at_ms: 1,
            version: "test".to_string(),
        },
        MessageSigner::new(exit_session, TEST_NETWORK_ID),
    )
    .expect("signed exit");
    OnionRoute::new(
        OnionServiceName::parse(service).expect("valid route service"),
        session_loop(guard, relay, exit_session, &session()),
        exit,
    )
    .expect("valid route")
}

/// Carry a forward cell through the relays of a route's Phase 1 view, as each relay would: open
/// the cell, peel its relay layer and seal the inner layer to the next hop in the same class.
/// Returns the cell the terminal receives.
pub(super) fn relay_to_terminal(relays: &[&DelegateeKey], payload: &Bytes) -> Bytes {
    relays.iter().fold(payload.clone(), |payload, relay| {
        let cell =
            rings_codec::deserialize::<OnionWireCell>(&payload).expect("decode encrypted cell");
        let OnionWireMessage::Forward(frame) = open_wire(relay, &payload) else {
            panic!("expected forward frame");
        };
        let OnionForwardLayer::Relay {
            next_circuit_id,
            next_delegatee_public_key,
            inner,
            ..
        } = decrypt_forward_layer(relay, frame.circuit_id, &frame.layer)
            .expect("decrypt relay layer")
        else {
            panic!("expected relay layer");
        };
        seal_message(
            &OnionWireMessage::Forward(OnionForwardFrame {
                circuit_id: next_circuit_id,
                layer: inner,
            }),
            next_delegatee_public_key,
            Some(cell.bucket),
        )
        .expect("seal next-hop cell")
    })
}

pub(super) fn decode_event(
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

pub(super) fn test_scope(delegatee_key: DelegateeKey) -> EffectScope {
    EffectScope::new(
        crate::test_support::test_scope(delegatee_key, ONION_CIRCUIT_NAMESPACE)
            .expect("onion circuit test scope"),
    )
}

pub(super) async fn peel_forward_cell(
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

/// Register `interpretation` for every world-facing symbol of `Σ`.
fn world_facing_algebra(
    interpretation: impl OnionInterpretation + Clone + 'static,
) -> Arc<OnionAlgebra> {
    Arc::new(
        OnionServiceName::world_facing().fold(OnionAlgebra::default(), |algebra, symbol| {
            algebra.register(symbol, interpretation.clone())
        }),
    )
}

/// Exit interpretation that consumes each forward nonce once and counts evaluations.
#[derive(Clone, Default)]
pub(super) struct RecordingExit {
    pub(super) exit_count: Arc<AtomicUsize>,
    exit_notify: Arc<tokio::sync::Notify>,
    forward_replays: Arc<Mutex<OnionForwardReplayPartitions>>,
}

#[async_trait::async_trait]
impl OnionInterpretation for RecordingExit {
    async fn evaluate(
        &self,
        _scope: &Scope,
        frame: OnionCircuitExitFrame,
    ) -> crate::error::Result<()> {
        let admission = lock(&self.forward_replays)?.consume(
            frame.from,
            OnionForwardReplayKey::new(frame.circuit_id, frame.forward_nonce),
            rings_core::utils::get_epoch_ms(),
        );
        assert_eq!(admission, ReplayAdmission::Consumed);
        self.exit_count.fetch_add(1, Ordering::SeqCst);
        self.exit_notify.notify_one();
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct RecordingHandler {
    clients: Arc<Mutex<Vec<(Did, OnionCircuitId, OnionAuthenticatedPayload)>>>,
    exit: RecordingExit,
    algebra: Arc<OnionAlgebra>,
}

impl Default for RecordingHandler {
    fn default() -> Self {
        let exit = RecordingExit::default();
        Self {
            clients: Arc::default(),
            algebra: world_facing_algebra(exit.clone()),
            exit,
        }
    }
}

impl RecordingHandler {
    pub(super) fn take_clients(&self) -> Vec<(Did, OnionCircuitId, OnionAuthenticatedPayload)> {
        std::mem::take(&mut self.clients.lock().expect("recorded clients"))
    }

    pub(super) fn exit_count(&self) -> usize {
        self.exit.exit_count.load(Ordering::SeqCst)
    }

    pub(super) async fn wait_for_exit_count(&self, expected: usize) {
        while self.exit_count() < expected {
            self.exit.exit_notify.notified().await;
        }
    }
}

#[async_trait::async_trait]
impl OnionCircuitHandler for RecordingHandler {
    fn algebra(&self) -> &OnionAlgebra {
        self.algebra.as_ref()
    }

    async fn handle_client(
        &self,
        _scope: &Scope,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: OnionAuthenticatedPayload,
    ) -> crate::error::Result<()> {
        lock(&self.clients)?.push((from, circuit_id, payload));
        Ok(())
    }

    async fn teardown(
        &self,
        _scope: &Scope,
        _teardown: crate::onion::circuit::OnionCircuitTeardown,
    ) -> crate::error::Result<()> {
        Ok(())
    }
}

/// Exit interpretation that blocks until the test releases it.
#[derive(Clone, Default)]
struct BlockingExit {
    started: Arc<AtomicBool>,
    started_notify: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl OnionInterpretation for BlockingExit {
    async fn evaluate(
        &self,
        _scope: &Scope,
        _frame: OnionCircuitExitFrame,
    ) -> crate::error::Result<()> {
        self.started.store(true, Ordering::SeqCst);
        self.started_notify.notify_waiters();
        self.release.notified().await;
        Ok(())
    }
}

#[derive(Clone)]
struct BlockingExitHandler {
    exit: BlockingExit,
    algebra: Arc<OnionAlgebra>,
}

impl Default for BlockingExitHandler {
    fn default() -> Self {
        let exit = BlockingExit::default();
        Self {
            algebra: world_facing_algebra(exit.clone()),
            exit,
        }
    }
}

impl BlockingExitHandler {
    async fn wait_until_started(&self) {
        while !self.exit.started.load(Ordering::SeqCst) {
            self.exit.started_notify.notified().await;
        }
    }

    fn release(&self) {
        self.exit.release.notify_one();
    }
}

#[async_trait::async_trait]
impl OnionCircuitHandler for BlockingExitHandler {
    fn algebra(&self) -> &OnionAlgebra {
        self.algebra.as_ref()
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

    async fn teardown(
        &self,
        _scope: &Scope,
        _teardown: crate::onion::circuit::OnionCircuitTeardown,
    ) -> crate::error::Result<()> {
        Ok(())
    }
}

#[test]
fn test_initial_forward_targets_first_hop_and_hides_payload() {
    let client = session();
    let first = session();
    let second = session();
    let exit = session();
    let route = route(&first, &second, &exit);
    let circuit_id = OnionCircuitId::new([9; 16]);

    let (to, payload) = encode_initial_forward(
        OnionClientReturn::new(client.delegatee_public_key()),
        &route,
        circuit_id,
        test_payload("probe"),
    )
    .expect("encode initial route");
    let decoded = open_wire(&first, &payload);

    assert_eq!(to, first.delegator_did());
    let OnionWireMessage::Forward(frame) = decoded else {
        panic!("expected forward frame");
    };
    assert_eq!(frame.circuit_id, circuit_id);
    assert!(!format!("{frame:?}").contains(&format!("{:?}", client.delegator_did())));
    assert!(!format!("{:?}", frame.layer).contains("probe"));
}

#[test]
fn test_relay_layer_uses_distinct_next_edge_circuit_id() {
    let client = session();
    let first = session();
    let exit = session();
    let route = route(&first, &session(), &exit);
    let first_circuit_id = OnionCircuitId::new([9; 16]);
    let (_, payload) = encode_initial_forward(
        OnionClientReturn::new(client.delegatee_public_key()),
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

#[test]
fn test_circuit_path_reuses_edge_ids_for_stream_payloads() {
    let client = session();
    let first = session();
    let exit = session();
    let route = route(&first, &session(), &exit);
    let first_circuit_id = OnionCircuitId::new([9; 16]);
    let client_return = OnionClientReturn::new(client.delegatee_public_key());
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

fn relay_next_circuit_id(
    relay: &DelegateeKey,
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
fn test_route_constructor_rejects_mismatched_exit_hop() {
    let first = session();
    let exit = session();
    let route = route(&session(), &session(), &exit);

    assert!(matches!(
        OnionRoute::new(
            route.service_name().clone(),
            session_loop(&session(), &session(), &first, &session()),
            route.exit().clone(),
        ),
        Err(crate::error::Error::OnionRouteError(
            OnionRouteError::ExitHopMismatch
        ))
    ));
}

#[test]
fn test_initial_forward_requires_route_payload_service_match() {
    let client = session();
    let exit = session();
    let route = route(&session(), &session(), &exit);
    let circuit_id = OnionCircuitId::new([9; 16]);

    assert!(matches!(
        encode_initial_forward(
            OnionClientReturn::new(client.delegatee_public_key()),
            &route,
            circuit_id,
            payload_for_service("tcp", "wrong-service"),
        ),
        Err(crate::error::Error::OnionRouteError(_))
    ));
}

#[test]
fn test_initial_forward_accepts_canonical_payload_for_mixed_case_route_service() {
    let client = session();
    let exit = session();
    let route = route_for_service("HTTPS", &session(), &session(), &exit);
    let circuit_id = OnionCircuitId::new([10; 16]);

    let result = encode_initial_forward(
        OnionClientReturn::new(client.delegatee_public_key()),
        &route,
        circuit_id,
        payload_for_service("https", "canonical-service"),
    );

    assert!(result.is_ok());
}

#[test]
fn test_hidden_cell_direction_defers_relay_capability_check_until_after_cell_decrypt() {
    let client = session();
    let relay = session();
    let exit = session();
    let route = route(&relay, &session(), &exit);
    let circuit_id = OnionCircuitId::new([1; 16]);
    let (_, payload) = encode_initial_forward(
        OnionClientReturn::new(client.delegatee_public_key()),
        &route,
        circuit_id,
        test_payload("tcp-shutdown"),
    )
    .expect("encode forward");
    let protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::Client);
    let event = decode_event(
        &protocol,
        client.delegator_did(),
        relay.delegator_did(),
        &payload,
    );

    let transition = protocol.step(
        Ctx {
            did: relay.delegator_did(),
            state: &protocol.init(),
        },
        event,
    );

    assert!(matches!(transition.effects.as_slice(), [
        OnionCircuitEffect::DecryptCell { .. }
    ]));

    let cell = rings_codec::deserialize::<OnionWireCell>(&payload).expect("decode encrypted cell");
    let message = open_cell(&relay, cell.bucket, &cell.sealed).expect("open encrypted cell");
    let event = super::super::codec::OnionCircuitEvent {
        input: OnionCircuitInput::CellReady {
            from: client.delegator_did(),
            received_at_ms: 1,
            bucket: cell.bucket,
            message,
        },
    };
    let transition = protocol.step(
        Ctx {
            did: relay.delegator_did(),
            state: &transition.state,
        },
        event,
    );
    assert!(transition.effects.is_empty());
}

#[tokio::test]
async fn test_relay_capability_does_not_execute_exit_layer() {
    let client = session();
    let guard = session();
    let previous = session();
    let relay = session();
    let route = route(&guard, &previous, &relay);
    let circuit_id = OnionCircuitId::new([4; 16]);
    let (_, payload) = encode_initial_forward(
        OnionClientReturn::new(client.delegatee_public_key()),
        &route,
        circuit_id,
        test_payload("tcp-shutdown"),
    )
    .expect("encode exit layer");
    let payload = relay_to_terminal(&[&guard, &previous], &payload);
    let protocol = OnionCircuitProtocol::new(OnionCircuitCapabilities::Relay);
    let shell = OnionCircuitShell::new(relay.clone(), RecordingHandler::default());
    let scope = test_scope(relay.clone());
    let state = protocol.init();
    let event = decode_event(
        &protocol,
        previous.delegator_did(),
        relay.delegator_did(),
        &payload,
    );
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

    assert!(transition.effects.is_empty());
}

#[tokio::test]
async fn test_exit_effect_releases_transition_turn_before_adapter_io_completes() {
    let client = session();
    let exit = session();
    let handler = BlockingExitHandler::default();
    let shell = OnionCircuitShell::new(exit.clone(), handler.clone());
    let scope = test_scope(exit.clone());
    let effect = OnionCircuitEffect::Exit {
        from: client.delegator_did(),
        circuit_id: OnionCircuitId::new([27; 16]),
        return_peer: client.delegator_did(),
        return_delegatee_public_key: client.delegatee_public_key(),
        client: OnionClientReturn::new(client.delegatee_public_key()),
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

/// Law: without a runtime the exit effect is refused and its adapter never starts.
#[test]
fn test_exit_effect_without_a_runtime_is_refused_before_the_adapter_starts() {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let client = session();
    let exit = session();
    let handler = BlockingExitHandler::default();
    let shell = OnionCircuitShell::new(exit.clone(), handler.clone());
    let scope = runtime.block_on(async { test_scope(exit.clone()) });
    let effect = OnionCircuitEffect::Exit {
        from: client.delegator_did(),
        circuit_id: OnionCircuitId::new([29; 16]),
        return_peer: client.delegator_did(),
        return_delegatee_public_key: client.delegatee_public_key(),
        client: OnionClientReturn::new(client.delegatee_public_key()),
        forward_nonce: OnionForwardNonce::new([30; 16]),
        forward_sequence: OnionForwardSequence::FIRST,
        payload: test_payload("exit-without-runtime"),
    };

    let refused = crate::test_support::without_runtime(|| {
        futures::executor::block_on(shell.run(&scope, effect))
    });

    assert!(matches!(
        refused,
        Err(crate::error::Error::RuntimeUnavailable(_))
    ));
    assert!(!handler.exit.started.load(Ordering::SeqCst));
}

/// Law: a send refused for want of a runtime claims no peer lane, so the next send to the
/// same peer still starts its own drain.
#[test]
fn test_send_effect_without_a_runtime_leaves_the_peer_lane_unclaimed() {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let local = session();
    let peer = session();
    let hook = Arc::new(OnionSendTestHook::default());
    let shell = OnionCircuitShell::new_with_send_test_hook(
        local.clone(),
        RecordingHandler::default(),
        Arc::clone(&hook),
    );
    let scope = runtime.block_on(async { test_scope(local.clone()) });
    let send = |tag: u8| {
        let message = OnionWireMessage::Backward(OnionBackwardFrame {
            circuit_id: OnionCircuitId::new([tag; 16]),
            payload: encrypt_client_payload(
                OnionReturnId::new([tag; 16]),
                test_payload("lane"),
                local.delegatee_public_key(),
                MessageSigner::new(&local, TEST_NETWORK_ID),
            )
            .expect("encrypt lane fixture"),
        });
        let effect = OnionCircuitEffect::SealAndSend {
            to: peer.delegator_did(),
            recipient: peer.delegatee_public_key(),
            bucket: OnionCellBucket::KiB4,
            encoded_message: encode_message(&message).expect("encode lane fixture"),
            cell: OnionRelayedCell::Data {
                circuit_id: OnionCircuitId::new([7; 16]),
            },
        };
        (message, effect)
    };

    let (_, refused_effect) = send(31);
    let refused = crate::test_support::without_runtime(|| {
        futures::executor::block_on(shell.run(&scope, refused_effect))
    });
    assert!(matches!(
        refused,
        Err(crate::error::Error::RuntimeUnavailable(_))
    ));

    let (message, effect) = send(32);
    runtime.block_on(async {
        shell
            .run(&scope, effect)
            .await
            .expect("enqueue after refusal");
        hook.release();
        let observed =
            tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_for_observed(1))
                .await
                .expect("the lane drains the send that follows a refusal")
                .expect("observed sends");
        let observed = observed
            .iter()
            .map(|payload| open_wire(&peer, payload))
            .collect::<Vec<_>>();
        assert_eq!(observed, [message]);
    });
}

#[tokio::test]
async fn test_send_effect_releases_transition_turn_and_preserves_peer_order() {
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
                local.delegatee_public_key(),
                MessageSigner::new(&local, TEST_NETWORK_ID),
            )
            .expect("encrypt ordered fixture"),
        })
    });
    for message in &messages {
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            shell.run(&scope, OnionCircuitEffect::SealAndSend {
                to: peer.delegator_did(),
                recipient: peer.delegatee_public_key(),
                bucket: OnionCellBucket::KiB4,
                encoded_message: encode_message(message).expect("encode ordered fixture"),
                cell: OnionRelayedCell::Data {
                    circuit_id: OnionCircuitId::new([7; 16]),
                },
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

#[tokio::test]
async fn test_endpoint_send_awaits_the_same_paced_link_lane_and_emits_cover() {
    let local = session();
    let peer = session();
    let hook = Arc::new(OnionSendTestHook::default());
    let link_sender = OnionLinkSender::with_test_hook(Arc::clone(&hook));
    let scope = test_scope(local.clone()).lifecycle();
    let payload = seal_message(
        &OnionWireMessage::Backward(OnionBackwardFrame {
            circuit_id: OnionCircuitId::new([41; 16]),
            payload: encrypt_client_payload(
                OnionReturnId::new([42; 16]),
                test_payload("endpoint-shaped"),
                local.delegatee_public_key(),
                MessageSigner::new(&local, TEST_NETWORK_ID),
            )
            .expect("encrypt endpoint fixture"),
        }),
        peer.delegatee_public_key(),
        None,
    )
    .expect("seal endpoint fixture");

    let send = tokio::spawn(async move {
        link_sender
            .send_sealed(
                scope,
                OnionLink::new(peer.delegator_did(), peer.delegatee_public_key()),
                payload,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_until_blocked())
        .await
        .expect("endpoint cell reached the shared pacing hook");
    assert!(
        !send.is_finished(),
        "endpoint returned before its real overlay send"
    );

    hook.release();
    assert!(send.await.expect("endpoint task joined").is_err());
    tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_for_covers(3))
        .await
        .expect("one endpoint cell fills the remaining fixed batch slots");
    assert_eq!(hook.cover_count(), 3);
}
