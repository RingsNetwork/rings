#[cfg(rings_native)]
use std::sync::atomic::AtomicU64;
#[cfg(rings_native)]
use std::sync::atomic::Ordering;
#[cfg(rings_native)]
use std::time::Duration;

use rings_core::ecc::SecretKey;
#[cfg(rings_native)]
use rings_core::message::MessageSigner;
use rings_core::session::SessionSk;
#[cfg(rings_native)]
use tokio::io::AsyncReadExt;
#[cfg(rings_native)]
use tokio::io::AsyncWriteExt;
#[cfg(rings_native)]
use tokio::net::TcpListener;

use super::super::*;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitService;
use crate::online::OnlineNodeType;
use crate::tests::TEST_NETWORK_ID;

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
            network_id: TEST_NETWORK_ID,
            service: OnionExitService::https(),
            policy: OnionExitPolicy::default(),
            started_at_ms: 0,
            heartbeat_at_ms: 0,
            expires_at_ms: 1,
            version: "test".to_string(),
        },
        MessageSigner::new(session, TEST_NETWORK_ID),
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
        MessageSigner::new(session, TEST_NETWORK_ID),
    )
    .expect("signed payload")
}

#[test]
fn test_normalizes_empty_request_defaults() {
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
fn test_client_request_from_url_uses_https_url_target_and_path() -> Result<()> {
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
fn test_client_request_from_url_preserves_explicit_port_and_path_override() -> Result<()> {
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
fn test_client_request_from_url_rejects_non_https_urls() {
    assert!(matches!(
        client_request_from_url("http://example.com/", Default::default()),
        Err(Error::HttpRequestError(_))
    ));
}

#[test]
fn test_rejects_relative_path_without_slash() {
    assert!(matches!(
        normalize_path("index.html"),
        Err(Error::HttpRequestError(_))
    ));
}

#[test]
fn test_default_body_limit_applies_when_policy_is_unlimited() {
    assert_eq!(
        https_response_body_limit(None),
        DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES
    );
    assert_eq!(https_response_body_limit(Some(7)), 7);
    assert_eq!(
        https_response_body_limit(Some(DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES + 1)),
        DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES
    );
}

#[test]
fn test_checked_status_code_rejects_invalid_js_status_values() {
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

#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_dropping_pending_request_future_removes_waiting_circuit() {
    let runtime = Arc::new(OnionHttpsRuntime::new());
    let exit = session();
    let return_id = OnionReturnId::new([3; 16]);
    let (_, pending_request) = runtime
        .begin_request(did(), exit_descriptor(&exit), return_id)
        .unwrap();
    let mut pending_request = Box::pin(pending_request);
    let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());

    assert_eq!(runtime.pending_len(), 1);
    assert!(std::future::Future::poll(pending_request.as_mut(), &mut context).is_pending());
    drop(pending_request);
    assert_eq!(runtime.pending_len(), 0);
}

#[test]
fn test_pending_request_completes_only_from_expected_return_peer() {
    let runtime = Arc::new(OnionHttpsRuntime::new());
    let expected = did();
    let other = did();
    let exit = session();
    let return_id = OnionReturnId::new([1; 16]);
    let (id, pending_request) = runtime
        .begin_request(expected, exit_descriptor(&exit), return_id)
        .unwrap();

    runtime.complete_payload(other, id, dummy_authenticated_payload(return_id, &exit));
    assert_eq!(runtime.pending_len(), 1);
    drop(pending_request);
}

#[test]
fn test_pending_request_rejects_payload_from_wrong_exit_session() {
    let runtime = Arc::new(OnionHttpsRuntime::new());
    let expected = did();
    let selected_exit = session();
    let wrong_exit = session();
    let return_id = OnionReturnId::new([2; 16]);
    let (id, mut pending_request) = runtime
        .begin_request(expected, exit_descriptor(&selected_exit), return_id)
        .unwrap();

    runtime.complete_payload(
        expected,
        id,
        dummy_authenticated_payload(return_id, &wrong_exit),
    );

    assert_eq!(runtime.pending_len(), 0);
    assert!(matches!(pending_request.try_recv(), Ok(Some(Err(_)))));
}

#[test]
fn test_pending_request_reports_authenticated_request_as_unexpected_backward_payload() {
    let runtime = Arc::new(OnionHttpsRuntime::new());
    let expected = did();
    let exit = session();
    let return_id = OnionReturnId::new([4; 16]);
    let (id, mut pending_request) = runtime
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
        MessageSigner::new(&exit, TEST_NETWORK_ID),
    )
    .unwrap();

    runtime.complete_payload(expected, id, payload);

    assert_eq!(runtime.pending_len(), 0);
    assert!(matches!(
        pending_request.try_recv(),
        Ok(Some(Err(Error::OnionRouteError(
            OnionRouteError::UnexpectedBackwardPayload
        ))))
    ));
}

#[test]
fn test_forward_nonce_is_consumed_once_for_https_exit_requests() {
    let runtime = OnionHttpsRuntime::new();
    let peer = Did::from(99_u32);
    let circuit_id = OnionCircuitId::new([1; 16]);
    let nonce = OnionForwardNonce::new([2; 16]);

    assert!(runtime
        .consume_forward_nonce(peer, circuit_id, nonce)
        .is_ok());
    assert!(matches!(
        runtime.consume_forward_nonce(peer, circuit_id, nonce),
        Err(Error::OnionRouteError(_))
    ));
}

#[test]
fn test_exit_limiter_rejects_bytes_over_policy_window() {
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
fn test_exit_limiter_enforces_streams_per_circuit() {
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
fn test_exit_limiter_counts_distinct_circuit_ids() {
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

#[cfg(rings_native)]
#[tokio::test]
async fn test_native_fetch_times_out_stalled_response() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let request = OnionHttpsRequest {
        target: format!("{address}"),
        method: "GET".to_string(),
        path: "/".to_string(),
        headers: Vec::new(),
        body: Vec::new(),
    };
    let egress = NativeHttpsEgress::Direct {
        host: address.ip().to_string(),
        addresses: vec![address],
    };

    let result = native_fetch_with_timeout(
        &format!("http://{address}/"),
        &request,
        DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES,
        Duration::from_millis(25),
        &egress,
        |_| Ok(()),
    )
    .await;

    server.abort();
    assert!(
        matches!(result, Err(Error::HttpRequestError(message)) if message.contains("timed out"))
    );
}

#[cfg(rings_native)]
#[tokio::test]
async fn test_native_fetch_records_response_bytes_as_chunks_arrive() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).await.unwrap();
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
            )
            .await
            .unwrap();
    });
    let request = OnionHttpsRequest {
        target: format!("{address}"),
        method: "GET".to_string(),
        path: "/".to_string(),
        headers: Vec::new(),
        body: Vec::new(),
    };
    let recorded = std::sync::Arc::new(AtomicU64::new(0));
    let recorded_for_fetch = recorded.clone();
    let egress = NativeHttpsEgress::Direct {
        host: address.ip().to_string(),
        addresses: vec![address],
    };

    let response = native_fetch_with_timeout(
        &format!("http://{address}/"),
        &request,
        DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES,
        Duration::from_secs(1),
        &egress,
        move |bytes| {
            recorded_for_fetch.fetch_add(bytes, Ordering::SeqCst);
            Ok(())
        },
    )
    .await
    .unwrap();

    server.await.unwrap();
    assert_eq!(response.body, b"abcde");
    assert_eq!(recorded.load(Ordering::SeqCst), 5);
}

#[cfg(rings_native)]
#[test]
fn test_native_egress_selection_pins_public_addresses_and_proxies_only_synthetic_dns() {
    let target = OnionProxyTarget::parse_authority("example.com:443").unwrap();
    let public = "8.8.8.8:443".parse().unwrap();
    let synthetic = "198.18.1.113:443".parse().unwrap();

    assert_eq!(
        select_native_https_egress(
            &target,
            vec![public],
            Some("http://127.0.0.1:6152".to_string()),
        )
        .unwrap(),
        NativeHttpsEgress::Direct {
            host: "example.com".to_string(),
            addresses: vec![public],
        },
    );
    assert_eq!(
        select_native_https_egress(
            &target,
            vec![synthetic],
            Some("http://127.0.0.1:6152".to_string()),
        )
        .unwrap(),
        NativeHttpsEgress::Proxy("http://127.0.0.1:6152".to_string()),
    );
    assert!(matches!(
        select_native_https_egress(&target, vec![synthetic], None),
        Err(Error::NoPermission),
    ));
}

#[cfg(rings_native)]
#[test]
fn test_native_proxy_fallback_rejects_private_loopback_and_literal_synthetic_targets() {
    let proxy = Some("http://127.0.0.1:6152".to_string());
    for (authority, address) in [
        ("localhost:443", "127.0.0.1:443"),
        ("internal.example:443", "10.0.0.1:443"),
        ("198.18.1.113:443", "198.18.1.113:443"),
    ] {
        let target = OnionProxyTarget::parse_authority(authority).unwrap();
        assert!(matches!(
            select_native_https_egress(&target, vec![address.parse().unwrap()], proxy.clone()),
            Err(Error::NoPermission),
        ));
    }
}

#[cfg(rings_native)]
#[cfg(rings_native)]
#[tokio::test]
async fn test_native_proxy_egress_delegates_target_resolution() {
    let proxy = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = proxy.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        let request_len = stream.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..request_len]);
        assert!(request.starts_with("GET http://unresolvable.invalid/probe HTTP/1.1"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
    });
    let request = OnionHttpsRequest {
        target: "unresolvable.invalid:80".to_string(),
        method: "GET".to_string(),
        path: "/probe".to_string(),
        headers: Vec::new(),
        body: Vec::new(),
    };
    let egress = NativeHttpsEgress::Proxy(format!("http://{proxy_address}"));

    let response = native_fetch_with_timeout(
        "http://unresolvable.invalid/probe",
        &request,
        DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES,
        Duration::from_secs(1),
        &egress,
        |_| Ok(()),
    )
    .await
    .unwrap();

    server.await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"ok");
}

#[test]
fn test_runtime_exit_policy_starts_empty_then_sets() -> Result<()> {
    let runtime = OnionHttpsRuntime::new();
    let policy = OnionExitPolicy::from_target_strings(vec!["example.com:443".to_string()], vec![])?;

    assert_eq!(runtime.exit_policy(), None);
    runtime.set_exit_policy(Some(policy.clone()));
    assert_eq!(runtime.exit_policy(), Some(policy));
    Ok(())
}
