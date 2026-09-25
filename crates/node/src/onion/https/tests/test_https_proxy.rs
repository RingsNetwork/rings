use std::future::Future;
#[cfg(rings_native)]
use std::sync::atomic::AtomicU64;
#[cfg(rings_native)]
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
#[cfg(rings_native)]
use std::time::Duration;

use rings_core::delegation::DelegateeKey;
use rings_core::ecc::SecretKey;
#[cfg(rings_native)]
use rings_core::message::MessageSigner;
#[cfg(rings_native)]
use tokio::io::AsyncReadExt;
#[cfg(rings_native)]
use tokio::io::AsyncWriteExt;
#[cfg(rings_native)]
use tokio::net::TcpListener;

use super::super::pending::PendingOnionHttpsRequest;
use super::super::*;
use crate::onion::circuit::OnionAuthenticatedPayload;
use crate::onion::circuit::OnionReturnId;
use crate::onion::proxy::OnionProxyProtocol;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionLoop;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteHop;
use crate::onion::OnionServiceName;
use crate::online::OnlineNodeType;
use crate::tests::TEST_NETWORK_ID;

fn did() -> Did {
    SecretKey::random().address().into()
}

fn session() -> DelegateeKey {
    DelegateeKey::new_with_seckey(&SecretKey::random()).expect("delegatee key")
}

fn exit_descriptor(session: &DelegateeKey) -> OnionExitDescriptor {
    OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did: session.delegator_did(),
            public_key: session
                .delegation()
                .delegator_verification_pubkey()
                .expect("verification key"),
            delegatee_public_key: session.delegatee_public_key(),
            process_epoch: crate::onion::OnionProcessEpoch::new([23; 16]),
            node_type: OnlineNodeType::Browser,
            network_id: TEST_NETWORK_ID,
            service: OnionServiceName::https(),
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

#[test]
fn test_normalizes_empty_request_defaults() {
    let request = OnionHttpsClientRequest {
        method: String::new(),
        path: Some(String::new()),
        headers: Vec::new(),
        body: Vec::new(),
    };
    let target = OnionProxyTarget::parse_authority("Example.COM:443").unwrap();
    let wire = OnionHttpsCall::with_default_path(request, default_path().as_str())
        .unwrap()
        .addressed_to(&target);

    assert_eq!(wire, OnionHttpsRequest {
        target: "example.com:443".to_string(),
        method: "GET".to_string(),
        path: "/".to_string(),
        headers: Vec::new(),
        body: Vec::new(),
    });
}

#[test]
fn test_call_from_url_uses_https_url_target_and_path() -> Result<()> {
    let (target, call) = OnionHttpsCall::from_url(
        "https://Example.COM/search?q=rust#ignored",
        Default::default(),
    )?;
    let wire = call.addressed_to(&target);

    assert_eq!(target.authority(), "example.com:443");
    assert_eq!(wire.target, "example.com:443");
    assert_eq!(wire.method, "GET");
    assert_eq!(wire.path, "/search?q=rust");
    Ok(())
}

#[test]
fn test_call_from_url_preserves_explicit_port_and_path_override() -> Result<()> {
    let request = OnionHttpsClientRequest {
        path: Some("?override=1".to_string()),
        ..OnionHttpsClientRequest::default()
    };
    let (target, call) = OnionHttpsCall::from_url("https://Example.COM:8443/original", request)?;
    let wire = call.addressed_to(&target);

    assert_eq!(target.authority(), "example.com:8443");
    assert_eq!(wire.target, "example.com:8443");
    assert_eq!(wire.path, "/?override=1");
    Ok(())
}

#[test]
fn test_call_from_url_rejects_non_https_urls() {
    assert!(matches!(
        OnionHttpsCall::from_url("http://example.com/", Default::default()),
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

const TEST_AUTHORITY: &str = "example.com:443";

fn runtime() -> OnionHttpsRuntime {
    OnionHttpsRuntime::new(session().delegatee_public_key())
}

fn client() -> Arc<OnionHttpsClient> {
    Arc::new(OnionHttpsClient::new(
        session().delegatee_public_key(),
        OnionLinkSender::default(),
    ))
}

/// HTTPS route to [`TEST_AUTHORITY`] over the loop `guard, relay, exit, back, guard`; its first
/// hop, and so expected return peer, is `guard`.
fn https_route(exit: &DelegateeKey, guard: &DelegateeKey) -> OnionProxyRoute {
    let descriptor = exit_descriptor(exit);
    let symbol = OnionRouteHop::of_symbol(&descriptor);
    let hop = |session: &DelegateeKey| {
        OnionRouteHop::new(
            session.delegator_did(),
            session.delegatee_public_key(),
            symbol.process_epoch,
        )
    };
    let mut relays = [hop(guard), hop(&session()), hop(&session())].into_iter();
    let hops = OnionLoop::try_unfold(Vec::new(), symbol, |_| {
        relays.next().ok_or(Error::InvalidData)
    })
    .expect("HTTPS loop");
    let route = OnionRoute::new(OnionServiceName::https(), hops, descriptor).expect("HTTPS route");
    OnionProxyRoute {
        protocol: OnionProxyProtocol::HttpsProxy,
        target: OnionProxyTarget::parse_authority(TEST_AUTHORITY).expect("test target"),
        route,
    }
}

/// `GET /` with no headers or body.
fn get_root() -> OnionHttpsCall {
    OnionHttpsCall::with_default_path(OnionHttpsClientRequest::default(), "/").expect("GET / call")
}

/// `payload` signed by `exit` for `return_id`.
fn exit_payload(
    return_id: OnionReturnId,
    exit: &DelegateeKey,
    payload: OnionHttpsPayload,
) -> OnionAuthenticatedPayload {
    OnionAuthenticatedPayload::new_signed(
        return_id,
        encode_https_payload(payload).expect("encode payload"),
        MessageSigner::new(exit, TEST_NETWORK_ID),
    )
    .expect("signed payload")
}

fn ok_response() -> OnionHttpsResponse {
    OnionHttpsResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/plain".to_string())],
        body: b"ok".to_vec(),
    }
}

/// Begin one request to `exit` through `guard` and return its circuit, the return id the exit must
/// sign, and the pending response.
fn begin(
    client: &Arc<OnionHttpsClient>,
    exit: &DelegateeKey,
    guard: &DelegateeKey,
) -> (OnionCircuitId, OnionReturnId, PendingOnionHttpsRequest) {
    let response = client
        .begin(&https_route(exit, guard), get_root())
        .expect("begin request")
        .into_response();
    let id = response.circuit_id();
    let return_id = client.pending_return_id(id).expect("circuit is pending");
    (id, return_id, response)
}

/// Resolve `future` in one poll: every outcome under test is decided before it is polled.
fn poll_decided<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let mut context = Context::from_waker(futures::task::noop_waker_ref());
    let Poll::Ready(output) = future.as_mut().poll(&mut context) else {
        panic!("outcome was not decided before polling");
    };
    output
}

/// Claim the request owning `(id, from)` and resolve it with `payload`.
fn resolve_from(
    client: &OnionHttpsClient,
    from: Did,
    id: OnionCircuitId,
    payload: OnionAuthenticatedPayload,
) -> Result<()> {
    client
        .claim(from, id)?
        .expect("request owns (id, from)")
        .resolve(payload, TEST_NETWORK_ID);
    Ok(())
}

/// A deadline that never fires.
fn no_deadline() -> impl Future<Output = Result<()>> {
    futures::future::pending()
}

/// A deadline that has already fired.
fn expired_deadline() -> impl Future<Output = Result<()>> {
    futures::future::ready(Ok(()))
}

#[test]
fn test_client_request_resolves_with_authenticated_exit_response() -> Result<()> {
    let client = client();
    let exit = session();
    let guard = session();
    let (id, return_id, response) = begin(&client, &exit, &guard);
    let reply = exit_payload(return_id, &exit, OnionHttpsPayload::Response(ok_response()));

    resolve_from(&client, guard.delegator_did(), id, reply)?;
    assert_eq!(client.pending_len(), 0);
    assert_eq!(poll_decided(response.within(no_deadline()))?, ok_response());
    Ok(())
}

#[test]
fn test_client_request_surfaces_exit_failure() -> Result<()> {
    let client = client();
    let exit = session();
    let guard = session();
    let (id, return_id, response) = begin(&client, &exit, &guard);
    let failure = OnionExitFailure::InvalidTarget("denied".to_string());
    let reply = exit_payload(return_id, &exit, OnionHttpsPayload::Error(failure.clone()));

    resolve_from(&client, guard.delegator_did(), id, reply)?;

    assert!(matches!(
        poll_decided(response.within(no_deadline())),
        Err(Error::OnionRouteError(OnionRouteError::ExitFailure(reported))) if reported == failure
    ));
    Ok(())
}

#[test]
fn test_client_request_ignores_payload_from_wrong_return_peer() -> Result<()> {
    let client = client();
    let (id, _, response) = begin(&client, &session(), &session());

    // The request owns (id, guard), not (id, other): no claim is taken and the request waits.
    assert!(client.claim(did(), id)?.is_none());
    assert_eq!(client.pending_len(), 1);
    assert!(matches!(
        poll_decided(response.within(expired_deadline())),
        Err(Error::OnionProxyRequestTimedOut)
    ));
    assert_eq!(client.pending_len(), 0);
    Ok(())
}

#[test]
fn test_client_request_rejects_payload_signed_by_another_exit() -> Result<()> {
    let client = client();
    let exit = session();
    let guard = session();
    let (id, return_id, response) = begin(&client, &exit, &guard);
    let forged = exit_payload(
        return_id,
        &session(),
        OnionHttpsPayload::Response(ok_response()),
    );

    resolve_from(&client, guard.delegator_did(), id, forged)?;

    assert_eq!(client.pending_len(), 0);
    assert!(matches!(
        poll_decided(response.within(no_deadline())),
        Err(Error::OnionRouteError(
            OnionRouteError::BackwardSignerMismatch
        ))
    ));
    Ok(())
}

#[test]
fn test_client_request_rejects_payload_for_another_return_id() -> Result<()> {
    let client = client();
    let exit = session();
    let guard = session();
    let (id, _, response) = begin(&client, &exit, &guard);
    let misdirected = exit_payload(
        OnionReturnId::new([9; 16]),
        &exit,
        OnionHttpsPayload::Response(ok_response()),
    );

    resolve_from(&client, guard.delegator_did(), id, misdirected)?;

    assert!(matches!(
        poll_decided(response.within(no_deadline())),
        Err(Error::OnionRouteError(
            OnionRouteError::BackwardReturnIdMismatch
        ))
    ));
    Ok(())
}

#[test]
fn test_client_request_reports_authenticated_request_as_unexpected_backward_payload() -> Result<()>
{
    let client = client();
    let exit = session();
    let guard = session();
    let (id, return_id, response) = begin(&client, &exit, &guard);
    let reply = exit_payload(
        return_id,
        &exit,
        OnionHttpsPayload::Request(get_root().addressed_to(&https_route(&exit, &guard).target)),
    );

    resolve_from(&client, guard.delegator_did(), id, reply)?;

    assert!(matches!(
        poll_decided(response.within(no_deadline())),
        Err(Error::OnionRouteError(
            OnionRouteError::UnexpectedBackwardPayload
        ))
    ));
    Ok(())
}

#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_dropping_waiting_request_cancels_and_releases_its_circuit() {
    let client = client();
    let exit = session();
    let guard = session();
    let (id, _, response) = begin(&client, &exit, &guard);
    let mut waiting = Box::pin(response.within(no_deadline()));
    let mut context = Context::from_waker(futures::task::noop_waker_ref());

    assert!(waiting.as_mut().poll(&mut context).is_pending());
    assert_eq!(client.pending_len(), 1);
    drop(waiting);
    assert_eq!(client.pending_len(), 0);

    // A late reply finds no claim, so it falls through to the next adapter.
    assert!(matches!(client.claim(guard.delegator_did(), id), Ok(None)));
}

#[test]
fn test_client_request_times_out_and_releases_its_circuit() {
    let client = client();
    let (_, _, response) = begin(&client, &session(), &session());

    assert!(matches!(
        poll_decided(response.within(expired_deadline())),
        Err(Error::OnionProxyRequestTimedOut)
    ));
    assert_eq!(client.pending_len(), 0);
}

/// The native circuit handler routes an HTTPS circuit's backward payload to the shared client.
#[cfg(rings_native)]
#[tokio::test]
async fn test_native_circuit_handler_delivers_https_circuits_to_the_shared_client() -> Result<()> {
    use crate::extension::ext::Extensions;
    use crate::onion::circuit::OnionCircuitHandler;
    use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
    use crate::onion::native::native_onion_runtimes;
    use crate::onion::native::NativeOnionCircuitHandler;

    let processor = Arc::new(crate::tests::native::prepare_processor().await);
    let scope = Scope::new(
        Extensions::new(processor).core(),
        ONION_CIRCUIT_NAMESPACE.to_string(),
    );
    let local = session();
    let (tcp, https) = native_onion_runtimes(local.clone(), TEST_NETWORK_ID, None);
    let handler = NativeOnionCircuitHandler::new(
        tcp,
        Arc::clone(&https),
        MessageSigner::new(local, TEST_NETWORK_ID),
    );
    let exit = session();
    let guard = session();
    let (id, return_id, response) = begin(https.client(), &exit, &guard);
    let reply = exit_payload(return_id, &exit, OnionHttpsPayload::Response(ok_response()));

    handler
        .handle_client(&scope, guard.delegator_did(), id, reply)
        .await?;

    assert_eq!(https.client().pending_len(), 0);
    assert_eq!(poll_decided(response.within(no_deadline()))?, ok_response());
    Ok(())
}

/// The browser circuit handler routes an HTTPS circuit's backward payload to the shared client.
#[cfg(rings_browser)]
#[wasm_bindgen_test::wasm_bindgen_test]
async fn test_browser_circuit_handler_delivers_https_circuits_to_the_shared_client() {
    use crate::extension::ext::Extensions;
    use crate::onion::circuit::OnionCircuitHandler;
    use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;

    let processor = Arc::new(crate::tests::wasm::prepare_processor().await);
    let scope = Scope::new(
        Extensions::new(processor).core(),
        ONION_CIRCUIT_NAMESPACE.to_string(),
    );
    let local = session();
    let runtime = Arc::new(OnionHttpsRuntime::new(local.delegatee_public_key()));
    let handler = BrowserOnionCircuitHandler::new(
        Arc::clone(&runtime),
        MessageSigner::new(local, TEST_NETWORK_ID),
    );
    let exit = session();
    let guard = session();
    let (id, return_id, response) = begin(runtime.client(), &exit, &guard);
    let reply = exit_payload(return_id, &exit, OnionHttpsPayload::Response(ok_response()));

    handler
        .handle_client(&scope, guard.delegator_did(), id, reply)
        .await
        .expect("browser handler accepts the backward payload");

    assert_eq!(runtime.client().pending_len(), 0);
    assert_eq!(
        poll_decided(response.within(no_deadline())).expect("exit response"),
        ok_response()
    );
}

#[test]
fn test_forward_nonce_is_consumed_once_for_https_exit_requests() {
    let runtime = runtime();
    let peer = Did::from(99_u32);
    let circuit_id = OnionCircuitId::new([1; 16]);
    let nonce = OnionForwardNonce::new([2; 16]);

    assert!(runtime
        .forward_replays
        .consume_forward_nonce(peer, circuit_id, nonce)
        .is_ok());
    assert!(matches!(
        runtime
            .forward_replays
            .consume_forward_nonce(peer, circuit_id, nonce),
        Err(Error::OnionRouteError(_))
    ));
}

#[test]
fn test_exit_limiter_rejects_bytes_over_policy_window() {
    let runtime = runtime();
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
    let runtime = runtime();
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
    let runtime = runtime();
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
    let egress = NativeHttpsEgress {
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
    let egress = NativeHttpsEgress {
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
fn test_native_transport_headers_are_not_caller_controlled() {
    for name in [
        "Host",
        ":authority",
        "Connection",
        "Proxy-Connection",
        "Keep-Alive",
        "Proxy-Authenticate",
        "Proxy-Authorization",
        "TE",
        "Trailer",
        "Transfer-Encoding",
        "Content-Length",
        "Upgrade",
        "Expect",
    ] {
        assert!(is_native_transport_managed_header(name), "{name}");
    }
    for name in ["Accept", "Authorization", "Content-Type", "X-Application"] {
        assert!(!is_native_transport_managed_header(name), "{name}");
    }
}

#[cfg(rings_native)]
#[tokio::test]
async fn test_native_fetch_uses_validated_url_authority_instead_of_caller_host() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let expected_host = format!("allowed.example:{}", address.port());
    let expected_host_for_server = expected_host.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let length = stream.read(&mut chunk).await.unwrap();
            assert_ne!(length, 0, "request headers ended before their delimiter");
            request.extend_from_slice(&chunk[..length]);
        }
        let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
        assert!(request.starts_with("post /probe http/1.1\r\n"));
        assert!(request.contains(&format!("\r\nhost: {expected_host_for_server}\r\n")));
        assert!(request.contains("\r\nx-application: preserved\r\n"));
        assert!(request.contains("\r\ncontent-length: 2\r\n"));
        assert!(!request.contains("blocked.example"));
        assert!(!request.contains("content-length: 999"));
        assert!(!request.contains("proxy-connection:"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
    });
    let request = OnionHttpsRequest {
        target: expected_host.clone(),
        method: "POST".to_string(),
        path: "/probe".to_string(),
        headers: vec![
            ("Host".to_string(), "blocked.example".to_string()),
            (":authority".to_string(), "blocked.example".to_string()),
            ("Content-Length".to_string(), "999".to_string()),
            ("Proxy-Connection".to_string(), "keep-alive".to_string()),
            ("X-Application".to_string(), "preserved".to_string()),
        ],
        body: b"ok".to_vec(),
    };
    let egress = NativeHttpsEgress {
        host: "allowed.example".to_string(),
        addresses: vec![address],
    };

    let response = native_fetch_with_timeout(
        &format!("http://{expected_host}/probe"),
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

#[cfg(rings_native)]
#[test]
fn test_native_egress_selection_pins_public_addresses_and_denies_the_rest() {
    let target = OnionProxyTarget::parse_authority("example.com:443").unwrap();
    let public = "8.8.8.8:443".parse().unwrap();

    assert_eq!(
        select_native_https_egress(&target, vec![public]).unwrap(),
        NativeHttpsEgress {
            host: "example.com".to_string(),
            addresses: vec![public],
        },
    );
    for (authority, address) in [
        ("localhost:443", "127.0.0.1:443"),
        ("internal.example:443", "10.0.0.1:443"),
        ("example.com:443", "198.18.1.113:443"),
        ("198.18.1.113:443", "198.18.1.113:443"),
    ] {
        let target = OnionProxyTarget::parse_authority(authority).unwrap();
        assert!(matches!(
            select_native_https_egress(&target, vec![address.parse().unwrap()]),
            Err(Error::NoPermission),
        ));
    }
}

#[test]
fn test_runtime_exit_policy_starts_empty_then_sets() -> Result<()> {
    let runtime = runtime();
    let policy = OnionExitPolicy::from_target_strings(vec!["example.com:443".to_string()], vec![])?;

    assert_eq!(runtime.exit_policy(), None);
    runtime.set_exit_policy(Some(policy.clone()));
    assert_eq!(runtime.exit_policy(), Some(policy));
    Ok(())
}
