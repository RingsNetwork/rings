use rings_core::ecc::SecretKey;
use rings_core::session::SessionSk;

use super::super::*;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitService;
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
            node_type: OnlineNodeType::Browser,
            network_id: 1,
            service: OnionExitService::https(),
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

fn dummy_authenticated_payload(
    return_id: OnionReturnId,
    session: &SessionSk,
) -> OnionAuthenticatedPayload {
    OnionAuthenticatedPayload::new_signed(
        return_id,
        encode_https_payload(OnionHttpsPayload::Error(OnionExitFailure::InvalidTarget(
            "wrong peer".to_string(),
        )))
        .expect("encode payload"),
        session,
    )
    .expect("signed payload")
}

#[test]
fn normalizes_empty_request_defaults() {
    let request = OnionHttpsClientRequest {
        method: String::new(),
        path: Some(String::new()),
        headers: Vec::new(),
        body: Vec::new(),
    };
    let target = OnionProxyTarget::parse_authority("Example.COM:443").unwrap();
    let wire = client_request_with_default_path(&target, request, default_path().as_str()).unwrap();

    assert_eq!(wire, OnionHttpsRequest {
        target: "example.com:443".to_string(),
        method: "GET".to_string(),
        path: "/".to_string(),
        headers: Vec::new(),
        body: Vec::new(),
    });
}

#[test]
fn client_request_from_url_uses_https_url_target_and_path() -> Result<()> {
    let (target, wire) = client_request_from_url(
        "https://Example.COM/search?q=rust#ignored",
        Default::default(),
    )?;

    assert_eq!(target.authority(), "example.com:443");
    assert_eq!(wire.target, "example.com:443");
    assert_eq!(wire.method, "GET");
    assert_eq!(wire.path, "/search?q=rust");
    Ok(())
}

#[test]
fn client_request_from_url_preserves_explicit_port_and_path_override() -> Result<()> {
    let request = OnionHttpsClientRequest {
        path: Some("?override=1".to_string()),
        ..OnionHttpsClientRequest::default()
    };
    let (target, wire) = client_request_from_url("https://Example.COM:8443/original", request)?;

    assert_eq!(target.authority(), "example.com:8443");
    assert_eq!(wire.target, "example.com:8443");
    assert_eq!(wire.path, "/?override=1");
    Ok(())
}

#[test]
fn client_request_from_url_rejects_non_https_urls() {
    assert!(matches!(
        client_request_from_url("http://example.com/", Default::default()),
        Err(Error::HttpRequestError(_))
    ));
}

#[test]
fn rejects_relative_path_without_slash() {
    assert!(matches!(
        normalize_path("index.html"),
        Err(Error::HttpRequestError(_))
    ));
}

#[test]
fn default_body_limit_applies_when_policy_is_unlimited() {
    assert_eq!(
        https_response_body_limit(None),
        DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES
    );
    assert_eq!(https_response_body_limit(Some(7)), 7);
}

#[test]
fn checked_status_code_rejects_invalid_js_status_values() {
    assert_eq!(checked_status_code(200.0).expect("status"), 200);
    assert!(matches!(
        checked_status_code(99.0),
        Err(Error::HttpRequestError(_))
    ));
    assert!(matches!(
        checked_status_code(200.5),
        Err(Error::HttpRequestError(_))
    ));
    assert!(matches!(
        checked_status_code(f64::NAN),
        Err(Error::HttpRequestError(_))
    ));
}

#[test]
fn cancel_request_removes_pending_request() {
    let runtime = OnionHttpsRuntime::new();
    let exit = session();
    let return_id = OnionReturnId::new([3; 16]);
    let (id, _receiver) = runtime
        .begin_request(did(), exit_descriptor(&exit), return_id)
        .unwrap();

    assert_eq!(runtime.pending_len(), 1);
    runtime.cancel_request(id);
    assert_eq!(runtime.pending_len(), 0);
}

#[test]
fn pending_request_completes_only_from_expected_return_peer() {
    let runtime = OnionHttpsRuntime::new();
    let expected = did();
    let other = did();
    let exit = session();
    let return_id = OnionReturnId::new([1; 16]);
    let (id, receiver) = runtime
        .begin_request(expected, exit_descriptor(&exit), return_id)
        .unwrap();

    runtime.complete_payload(other, id, dummy_authenticated_payload(return_id, &exit));
    assert_eq!(runtime.pending_len(), 1);
    drop(receiver);
    runtime.cancel_request(id);
}

#[test]
fn pending_request_rejects_payload_from_wrong_exit_session() {
    let runtime = OnionHttpsRuntime::new();
    let expected = did();
    let selected_exit = session();
    let wrong_exit = session();
    let return_id = OnionReturnId::new([2; 16]);
    let (id, mut receiver) = runtime
        .begin_request(expected, exit_descriptor(&selected_exit), return_id)
        .unwrap();

    runtime.complete_payload(
        expected,
        id,
        dummy_authenticated_payload(return_id, &wrong_exit),
    );

    assert_eq!(runtime.pending_len(), 0);
    assert!(matches!(receiver.try_recv(), Ok(Some(Err(_)))));
}

#[test]
fn pending_request_reports_authenticated_request_as_unexpected_backward_payload() {
    let runtime = OnionHttpsRuntime::new();
    let expected = did();
    let exit = session();
    let return_id = OnionReturnId::new([4; 16]);
    let (id, mut receiver) = runtime
        .begin_request(expected, exit_descriptor(&exit), return_id)
        .unwrap();
    let request_payload = OnionHttpsPayload::Request(OnionHttpsRequest {
        target: "example.com:443".to_string(),
        method: "GET".to_string(),
        path: "/".to_string(),
        headers: Vec::new(),
        body: Vec::new(),
    });
    let payload = OnionAuthenticatedPayload::new_signed(
        return_id,
        encode_https_payload(request_payload).unwrap(),
        &exit,
    )
    .unwrap();

    runtime.complete_payload(expected, id, payload);

    assert_eq!(runtime.pending_len(), 0);
    assert!(matches!(
        receiver.try_recv(),
        Ok(Some(Err(Error::OnionRouteError(
            OnionRouteError::UnexpectedBackwardPayload
        ))))
    ));
}

#[test]
fn forward_nonce_is_consumed_once_for_https_exit_requests() {
    let runtime = OnionHttpsRuntime::new();
    let circuit_id = OnionCircuitId::new([1; 16]);
    let nonce = OnionForwardNonce::new([2; 16]);

    assert!(runtime.consume_forward_nonce(circuit_id, nonce).is_ok());
    assert!(matches!(
        runtime.consume_forward_nonce(circuit_id, nonce),
        Err(Error::OnionRouteError(_))
    ));
}

#[test]
fn exit_limiter_rejects_bytes_over_policy_window() {
    let runtime = OnionHttpsRuntime::new();
    let policy = OnionExitPolicy {
        max_bytes_per_minute: 8,
        ..OnionExitPolicy::default()
    };
    let circuit_id = OnionCircuitId::new([1; 16]);
    let return_peer = did();
    let _lease = runtime
        .admit_exit_request(&policy, circuit_id, return_peer, 4)
        .unwrap();

    assert!(runtime.record_exit_bytes(&policy, 4).is_ok());
    assert!(matches!(
        runtime.record_exit_bytes(&policy, 1),
        Err(Error::NoPermission)
    ));
}

#[test]
fn exit_limiter_enforces_streams_per_circuit() {
    let runtime = OnionHttpsRuntime::new();
    let policy = OnionExitPolicy {
        max_streams_per_circuit: 1,
        ..OnionExitPolicy::default()
    };
    let circuit_id = OnionCircuitId::new([1; 16]);
    let return_peer = did();

    let lease = runtime
        .admit_exit_request(&policy, circuit_id, return_peer, 0)
        .expect("first stream admitted");
    assert!(matches!(
        runtime.admit_exit_request(&policy, circuit_id, return_peer, 0),
        Err(Error::NoPermission)
    ));
    drop(lease);
    assert!(runtime
        .admit_exit_request(&policy, circuit_id, return_peer, 0)
        .is_ok());
}

#[test]
fn exit_limiter_counts_distinct_circuit_ids() {
    let runtime = OnionHttpsRuntime::new();
    let policy = OnionExitPolicy {
        max_circuits: 1,
        ..OnionExitPolicy::default()
    };
    let return_peer = did();
    let first = OnionCircuitId::new([1; 16]);
    let second = OnionCircuitId::new([2; 16]);

    let lease = runtime
        .admit_exit_request(&policy, first, return_peer, 0)
        .expect("first circuit admitted");
    assert!(matches!(
        runtime.admit_exit_request(&policy, second, return_peer, 0),
        Err(Error::NoPermission)
    ));
    drop(lease);
    assert!(runtime
        .admit_exit_request(&policy, second, return_peer, 0)
        .is_ok());
}

#[test]
fn runtime_exit_policy_starts_empty_then_sets() -> Result<()> {
    let runtime = OnionHttpsRuntime::new();
    let policy = OnionExitPolicy::from_target_strings(vec!["example.com:443".to_string()], vec![])?;

    assert_eq!(runtime.exit_policy(), None);
    runtime.set_exit_policy(Some(policy.clone()));
    assert_eq!(runtime.exit_policy(), Some(policy));
    Ok(())
}
