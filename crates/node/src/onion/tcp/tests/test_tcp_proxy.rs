use rings_core::ecc::SecretKey;

use super::super::*;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitService;
use crate::onion::OnionExitTransport;
use crate::onion::OnionServiceName;
use crate::online::OnlineNodeType;

fn did() -> Did {
    SecretKey::random().address().into()
}

fn session() -> SessionSk {
    SessionSk::new_with_seckey(&SecretKey::random()).expect("session key")
}

fn exit_descriptor(session: &SessionSk) -> OnionExitDescriptor {
    OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did: session.account_did(),
            public_key: session
                .session()
                .account_verification_pubkey()
                .expect("verification key"),
            session_public_key: session.session_public_key(),
            node_type: OnlineNodeType::Native,
            network_id: 1,
            service: OnionExitService::tcp(),
            policy: OnionExitPolicy::default(),
            started_at_ms: 0,
            heartbeat_at_ms: 0,
            expires_at_ms: 1,
            version: "test".to_string(),
        },
        session,
    )
    .expect("signed exit")
}

fn runtime() -> OnionTcpRuntime {
    OnionTcpRuntime::new(session(), None)
}

fn dummy_authenticated_payload(
    return_id: OnionReturnId,
    session: &SessionSk,
) -> OnionAuthenticatedPayload {
    dummy_authenticated_payload_for_service(return_id, session, OnionServiceName::tcp())
}

fn dummy_authenticated_payload_for_service(
    return_id: OnionReturnId,
    session: &SessionSk,
    service: OnionServiceName,
) -> OnionAuthenticatedPayload {
    OnionAuthenticatedPayload::new_signed(
        return_id,
        encode_tcp_payload(&service, OnionTcpPayload::Close).expect("encode payload"),
        session,
    )
    .expect("signed payload")
}

fn insert_test_client_stream(
    runtime: &OnionTcpRuntime,
    expected: Did,
    exit: OnionExitDescriptor,
    return_id: OnionReturnId,
    tx: mpsc::Sender<TcpInbound>,
) -> Result<TcpStreamKey> {
    insert_test_client_stream_for_service(
        runtime,
        OnionServiceName::tcp(),
        expected,
        exit,
        return_id,
        tx,
    )
}

fn insert_test_client_stream_for_service(
    runtime: &OnionTcpRuntime,
    service: OnionServiceName,
    expected: Did,
    exit: OnionExitDescriptor,
    return_id: OnionReturnId,
    tx: mpsc::Sender<TcpInbound>,
) -> Result<TcpStreamKey> {
    let (open_tx, _open_rx) = tokio::sync::oneshot::channel();
    runtime.insert_client_stream(service, expected, exit, return_id, open_tx, tx)
}

#[test]
fn tcp_duplex_state_closes_only_after_both_halves_close() {
    let mut state = TcpDuplexState::open();
    assert!(state.should_announce_terminal());

    state.close_read();
    assert!(!state.can_read());
    assert!(state.can_write());
    assert!(!state.is_closed());

    state.close_write();
    assert!(state.is_closed());
    assert!(state.should_announce_terminal());
}

#[test]
fn tcp_duplex_state_suppresses_terminal_after_remote_close() {
    let mut state = TcpDuplexState::open();

    state.observe_remote_terminal();

    assert!(state.is_closed());
    assert!(!state.should_announce_terminal());
}

#[test]
fn client_stream_accepts_only_expected_return_peer() -> Result<()> {
    let runtime = runtime();
    let expected = did();
    let attacker = did();
    let exit = session();
    let (tx, _rx) = mpsc::channel(1);
    let key = insert_test_client_stream(
        &runtime,
        expected,
        exit_descriptor(&exit),
        OnionReturnId::new([7; 16]),
        tx,
    )?;

    assert!(runtime.client_inbound_sender(key, expected).is_ok());
    assert!(matches!(
        runtime.client_inbound_sender(key, attacker),
        Err(Error::OnionRouteError(_))
    ));
    Ok(())
}

#[test]
fn client_stream_rejects_payload_from_wrong_exit_session() -> Result<()> {
    let runtime = runtime();
    let expected = did();
    let selected_exit = session();
    let wrong_exit = session();
    let return_id = OnionReturnId::new([9; 16]);
    let (tx, _rx) = mpsc::channel(1);
    let key = insert_test_client_stream(
        &runtime,
        expected,
        exit_descriptor(&selected_exit),
        return_id,
        tx,
    )?;

    assert!(matches!(
        runtime.verify_client_payload(
            key,
            expected,
            dummy_authenticated_payload(return_id, &wrong_exit),
        ),
        Err(Error::OnionRouteError(_))
    ));
    Ok(())
}

#[test]
fn client_stream_rejects_replayed_backward_nonce() -> Result<()> {
    let runtime = runtime();
    let expected = did();
    let exit = session();
    let return_id = OnionReturnId::new([8; 16]);
    let (tx, _rx) = mpsc::channel(1);
    let key = insert_test_client_stream(&runtime, expected, exit_descriptor(&exit), return_id, tx)?;
    let payload = dummy_authenticated_payload(return_id, &exit);

    assert!(runtime
        .verify_client_payload(key, expected, payload.clone())
        .is_ok());
    assert!(matches!(
        runtime.verify_client_payload(key, expected, payload),
        Err(Error::OnionRouteError(_))
    ));
    Ok(())
}

#[test]
fn exit_runtime_rejects_replayed_forward_nonce() -> Result<()> {
    let runtime = runtime();
    let circuit_id = OnionCircuitId::new([1; 16]);
    let nonce = OnionForwardNonce::new([2; 16]);

    assert!(runtime.consume_forward_nonce(circuit_id, nonce).is_ok());
    assert!(matches!(
        runtime.consume_forward_nonce(circuit_id, nonce),
        Err(Error::OnionRouteError(_))
    ));
    Ok(())
}

#[test]
fn tcp_payload_uses_selected_route_service() -> Result<()> {
    let service = OnionServiceName::parse("web")?;
    let payload = encode_tcp_payload(&service, OnionTcpPayload::Close)?;

    assert!(payload.is_service(&service));
    assert!(!payload.is_service(&OnionServiceName::tcp()));
    Ok(())
}

#[test]
fn native_tcp_exit_config_rejects_empty_or_non_tcp_services() {
    assert!(matches!(
        NativeOnionTcpExitConfig::new(Vec::new(), OnionExitPolicy::default()),
        Err(Error::InvalidConfig(_))
    ));
    assert!(matches!(
        NativeOnionTcpExitConfig::new(vec![OnionExitService::https()], OnionExitPolicy::default()),
        Err(Error::InvalidConfig(_))
    ));
}

#[test]
fn exit_runtime_accepts_only_installed_tcp_services() -> Result<()> {
    let service = OnionServiceName::parse("web")?;
    let config = NativeOnionTcpExitConfig::new(
        vec![OnionExitService::new("web", OnionExitTransport::Tcp)?],
        OnionExitPolicy::default(),
    )?;
    let runtime = OnionTcpRuntime::new(session(), Some(config));
    let custom_payload = encode_tcp_payload(&service, OnionTcpPayload::Close)?;
    let tcp_payload = encode_tcp_payload(&OnionServiceName::tcp(), OnionTcpPayload::Close)?;

    assert!(matches!(
        runtime.decode_exit_payload(custom_payload)?,
        Some((accepted, OnionTcpPayload::Close)) if accepted == service
    ));
    assert!(runtime.decode_exit_payload(tcp_payload)?.is_none());
    Ok(())
}

#[test]
fn client_stream_rejects_backward_payload_for_wrong_service() -> Result<()> {
    let runtime = runtime();
    let expected = did();
    let exit = session();
    let return_id = OnionReturnId::new([10; 16]);
    let (tx, _rx) = mpsc::channel(1);
    let key = insert_test_client_stream_for_service(
        &runtime,
        OnionServiceName::parse("web")?,
        expected,
        exit_descriptor(&exit),
        return_id,
        tx,
    )?;

    assert!(matches!(
        runtime.verify_client_payload(
            key,
            expected,
            dummy_authenticated_payload_for_service(return_id, &exit, OnionServiceName::tcp()),
        ),
        Err(Error::OnionRouteError(
            OnionRouteError::PayloadServiceMismatch { .. }
        ))
    ));
    Ok(())
}

#[test]
fn exit_limiter_enforces_streams_per_circuit() {
    let runtime = runtime();
    let policy = OnionExitPolicy {
        max_streams_per_circuit: 1,
        ..OnionExitPolicy::default()
    };
    let circuit_id = OnionCircuitId::new([1; 16]);
    let return_peer = did();

    let lease = runtime
        .admit_exit_stream(&policy, circuit_id, return_peer, 0)
        .expect("first stream admitted");
    assert!(matches!(
        runtime.admit_exit_stream(&policy, circuit_id, return_peer, 0),
        Err(Error::NoPermission)
    ));
    drop(lease);
    assert!(runtime
        .admit_exit_stream(&policy, circuit_id, return_peer, 0)
        .is_ok());
}

#[test]
fn exit_stream_rejects_duplicate_live_circuit() {
    let runtime = runtime();
    let key = TcpStreamKey {
        circuit_id: OnionCircuitId::new([3; 16]),
    };
    let expected = did();
    let (first_tx, _first_rx) = mpsc::channel(1);
    let (second_tx, _second_rx) = mpsc::channel(1);

    assert!(runtime
        .insert_exit_stream(key, OnionServiceName::tcp(), expected, first_tx)
        .is_ok());
    assert!(matches!(
        runtime.insert_exit_stream(key, OnionServiceName::tcp(), expected, second_tx),
        Err(Error::OnionRouteError(_))
    ));
}

#[test]
fn exit_limiter_counts_distinct_circuit_ids() {
    let runtime = runtime();
    let policy = OnionExitPolicy {
        max_circuits: 1,
        ..OnionExitPolicy::default()
    };
    let return_peer = did();
    let first = OnionCircuitId::new([1; 16]);
    let second = OnionCircuitId::new([2; 16]);

    let lease = runtime
        .admit_exit_stream(&policy, first, return_peer, 0)
        .expect("first circuit admitted");
    assert!(matches!(
        runtime.admit_exit_stream(&policy, second, return_peer, 0),
        Err(Error::NoPermission)
    ));
    drop(lease);
    assert!(runtime
        .admit_exit_stream(&policy, second, return_peer, 0)
        .is_ok());
}

#[tokio::test]
async fn install_rejects_duplicate_namespace_instead_of_splitting_runtime() -> Result<()> {
    let processor = Arc::new(crate::tests::native::prepare_processor().await);
    let session_sk = processor.session_sk().clone();
    let extensions = Extensions::new(processor);
    let _handle = NativeOnionCircuitHandle::install(&extensions, session_sk.clone(), false, None)?;

    assert!(matches!(
        NativeOnionCircuitHandle::install(&extensions, session_sk, false, None),
        Err(Error::ExtensionError(_))
    ));
    Ok(())
}
