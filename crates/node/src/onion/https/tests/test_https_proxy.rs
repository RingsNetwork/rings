use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use super::super::*;
use crate::onion::exit_accounting::OnionExitAccounting;

/// The request a normalized call becomes.
fn wire(call: OnionHttpsCall) -> OnionHttpsRequest {
    call.into_request()
}

#[test]
fn test_normalizes_empty_request_defaults() {
    let request = OnionHttpsClientRequest {
        method: String::new(),
        path: Some(String::new()),
        headers: Vec::new(),
        body: Vec::new(),
    };

    assert_eq!(
        wire(OnionHttpsCall::with_default_path(request, default_path().as_str()).unwrap()),
        OnionHttpsRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    );
}

#[test]
fn test_call_from_url_uses_https_url_target_and_path() -> Result<()> {
    let (target, call) = OnionHttpsCall::from_url(
        "https://Example.COM/search?q=rust#ignored",
        Default::default(),
    )?;
    let request = wire(call);

    assert_eq!(target.authority(), "example.com:443");
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/search?q=rust");
    Ok(())
}

#[test]
fn test_call_from_url_preserves_explicit_port_and_path_override() -> Result<()> {
    let request = OnionHttpsClientRequest {
        path: Some("?override=1".to_string()),
        ..OnionHttpsClientRequest::default()
    };
    let (target, call) = OnionHttpsCall::from_url("https://Example.COM:8443/original", request)?;

    assert_eq!(target.authority(), "example.com:8443");
    assert_eq!(wire(call).path, "/?override=1");
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
}

/// Read the whole world-to-client stream of a fetch in reads of at most `max` bytes.
async fn read_outcome(reader: &mut OnionHttpsReader, max: usize) -> Vec<u8> {
    let mut outcome = Vec::new();
    while let Some(bytes) = reader.read(max).await.expect("fetch read") {
        assert!(bytes.len() <= max);
        outcome.extend_from_slice(&bytes);
    }
    outcome
}

/// Law (fetch at fin): the world fetches only once its request stream ends, and a request that
/// does not decode yields an encoded exit failure, not a fetch.
#[tokio::test]
async fn test_fetch_world_answers_a_malformed_request_with_an_encoded_failure() {
    let world = OnionHttpsWorld::new(OnionExitPolicy::default(), OnionExitAccounting::default());
    let target = OnionProxyTarget::parse_authority("example.com:443").unwrap();
    let (mut reader, mut writer) = world.open(&target).await.unwrap();

    writer.write(Bytes::from_static(b"\xff")).await.unwrap();
    writer.shutdown().await.unwrap();

    assert!(matches!(
        decode_outcome(&read_outcome(&mut reader, 3).await).unwrap(),
        OnionHttpsOutcome::Error(OnionExitFailure::MalformedRequest)
    ));
}

/// Law (session end without fin): a fetch whose writer is dropped before `fin` has no request and
/// ends its stream empty.
#[tokio::test]
async fn test_fetch_world_without_fin_ends_empty() {
    let world = OnionHttpsWorld::new(OnionExitPolicy::default(), OnionExitAccounting::default());
    let target = OnionProxyTarget::parse_authority("example.com:443").unwrap();
    let (mut reader, writer) = world.open(&target).await.unwrap();

    drop(writer);

    assert_eq!(reader.read(16).await.unwrap(), None);
}

/// Law (request bound): the writer refuses a request past [`MAX_HTTPS_REQUEST_BYTES`].
#[tokio::test]
async fn test_fetch_world_bounds_the_request() {
    let world = OnionHttpsWorld::new(OnionExitPolicy::default(), OnionExitAccounting::default());
    let target = OnionProxyTarget::parse_authority("example.com:443").unwrap();
    let (_reader, mut writer) = world.open(&target).await.unwrap();

    writer
        .write(Bytes::from(vec![0; MAX_HTTPS_REQUEST_BYTES]))
        .await
        .unwrap();

    assert!(writer.write(Bytes::from_static(b"x")).await.is_err());
}

/// Law (codec): an outcome round-trips through its stream encoding.
#[test]
fn test_outcome_round_trips() {
    let outcome = OnionHttpsOutcome::Response(OnionHttpsResponse {
        status: 204,
        headers: vec![("a".to_string(), "b".to_string())],
        body: b"body".to_vec(),
    });

    assert_eq!(
        decode_outcome(&encode_outcome(&outcome).unwrap()).unwrap(),
        outcome
    );
}

#[tokio::test]
async fn test_native_fetch_times_out_stalled_response() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let request = OnionHttpsRequest {
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

#[tokio::test]
async fn test_native_fetch_reads_a_chunked_body_whole() {
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
        method: "GET".to_string(),
        path: "/".to_string(),
        headers: Vec::new(),
        body: Vec::new(),
    };
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
        |_| Ok(()),
    )
    .await
    .unwrap();

    server.await.unwrap();
    assert_eq!(response.body, b"abcde");
}

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

/// Render bytes as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The `https` symbol's carry payloads are wire bytes (#834 L10): one request, one response and
/// every failure, each pinned byte for byte, so a reordered field or variant is a visible cutover.
#[test]
fn test_https_session_encodings_are_pinned() {
    let request = OnionHttpsRequest {
        method: "POST".to_string(),
        path: "/p?q".to_string(),
        headers: vec![("a".to_string(), "b".to_string())],
        body: b"xy".to_vec(),
    };
    let response = OnionHttpsOutcome::Response(OnionHttpsResponse {
        status: 201,
        headers: vec![("c".to_string(), "d".to_string())],
        body: b"z".to_vec(),
    });
    let pinned = [
        (
            hex(&encode_request(&request).expect("encode")),
            "04504f5354042f703f710101610162027879",
        ),
        (
            hex(&encode_outcome(&response).expect("encode")),
            "00c9010101630164017a",
        ),
        (
            hex(&encode_outcome(&OnionHttpsOutcome::Error(
                OnionExitFailure::PermissionDenied,
            ))
            .expect("encode")),
            "0100",
        ),
        (
            hex(&encode_outcome(&OnionHttpsOutcome::Error(
                OnionExitFailure::MalformedRequest,
            ))
            .expect("encode")),
            "0101",
        ),
        (
            hex(
                &encode_outcome(&OnionHttpsOutcome::Error(OnionExitFailure::Internal))
                    .expect("encode"),
            ),
            "0102",
        ),
    ];

    for (encoded, golden) in pinned {
        assert_eq!(encoded, golden);
    }
    assert_eq!(
        decode_outcome(&encode_outcome(&response).expect("encode")).expect("decode"),
        response
    );
}

/// One budget across concurrent fetches (#895 D-N1): each fetch records its headers and body
/// chunks as they stream against the shared accounting, so two fetches whose bodies together
/// exceed the window's budget cannot both complete, and no more than the budget is recorded.
#[tokio::test]
async fn test_concurrent_fetches_share_one_byte_budget() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = vec![b'b'; 3_000];
    let server = tokio::spawn(async move {
        let mut served = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let body = body.clone();
            served.push(tokio::spawn(async move {
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request).await;
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }));
        }
        for serving in served {
            let _ = serving.await;
        }
    });
    let policy = OnionExitPolicy {
        max_bytes_per_minute: 4_000,
        ..OnionExitPolicy::default()
    };
    let accounting = OnionExitAccounting::default();
    let fetch = || {
        let request = OnionHttpsRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let egress = NativeHttpsEgress {
            host: address.ip().to_string(),
            addresses: vec![address],
        };
        let accounting = accounting.clone();
        let policy = policy.clone();
        async move {
            native_fetch_with_timeout(
                &format!("http://{address}/"),
                &request,
                DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES,
                Duration::from_secs(5),
                &egress,
                |bytes| accounting.record_bytes(&policy, bytes, 0),
            )
            .await
        }
    };

    let (first, second) = tokio::join!(fetch(), fetch());
    server.await.unwrap();

    assert!(
        first.is_err() || second.is_err(),
        "both fetches fit a budget below their sum"
    );
    assert!(
        first.is_ok() || second.is_ok(),
        "the budget admits one whole fetch"
    );
    assert!(accounting
        .remaining_bytes(&policy, 0)
        .unwrap()
        .is_some_and(|left| left < 4_000));
}
