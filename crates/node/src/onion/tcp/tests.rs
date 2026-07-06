use rings_core::ecc::SecretKey;

use super::*;

fn did() -> Did {
    SecretKey::random().address().into()
}

fn client_return() -> OnionClientReturn {
    let session_sk = SessionSk::new_with_seckey(&SecretKey::random()).expect("session key");
    OnionClientReturn::new(session_sk.session_public_key())
}

fn runtime() -> OnionTcpRuntime {
    OnionTcpRuntime::new(client_return(), None)
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
    let (tx, _rx) = mpsc::channel(1);
    let key = runtime.insert_client_stream(expected, tx)?;

    assert!(runtime.client_inbound_sender(key, expected).is_ok());
    assert!(matches!(
        runtime.client_inbound_sender(key, attacker),
        Err(Error::OnionRouteError(_))
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

    assert!(runtime.insert_exit_stream(key, expected, first_tx).is_ok());
    assert!(matches!(
        runtime.insert_exit_stream(key, expected, second_tx),
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
