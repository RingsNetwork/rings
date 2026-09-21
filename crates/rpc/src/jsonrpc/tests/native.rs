//! Controlled HTTP/TLS servers witness credential delivery and redirect refusal.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::rustls;
use tokio_rustls::TlsAcceptor;

use crate::jsonrpc::Client;
use crate::jsonrpc::RpcError;
use crate::protos::rings_node::NodeDidRequest;

/// Test failures preserve source errors across the spawned server boundary.
type TestError = Box<dyn std::error::Error>;
/// Fallible fixture setup and network observations.
type TestResult<T = ()> = std::result::Result<T, TestError>;
/// Bounded server task yielding the request it actually observed.
type ServerTask = JoinHandle<std::io::Result<String>>;
/// Every credential in these tests is synthetic.
const TOKEN: &str = "rpc-795-test-token";
/// Successful nodeDid response for the request emitted by these fixtures.
const BODY: &str = r#"{"jsonrpc":"2.0","result":{"did":"fixture"},"id":1}"#;

/// Create a locally trusted certificate without disabling certificate verification.
fn tls_fixture() -> TestResult<(TlsAcceptor, reqwest::Certificate)> {
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
    let root = reqwest::Certificate::from_der(certificate.cert.der())?;
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.key_pair.serialize_der());
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![certificate.cert.der().clone()], key.into())?;
    Ok((TlsAcceptor::from(Arc::new(config)), root))
}

/// Read the complete request headers and return a configured HTTP response.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    reply: String,
) -> std::io::Result<String> {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await?;
        request.extend_from_slice(&byte);
        if request.len() > 16_384 {
            return Err(std::io::Error::other("fixture headers exceed bound"));
        }
    }
    stream.write_all(reply.as_bytes()).await?;
    stream.shutdown().await?;
    String::from_utf8(request).map_err(std::io::Error::other)
}

/// Serve exactly one request, timing out if the client does not reach the intended listener.
fn serve(listener: TcpListener, tls: Option<TlsAcceptor>, reply: String) -> ServerTask {
    tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async move {
            let (stream, _) = listener.accept().await?;
            match tls {
                Some(acceptor) => exchange(acceptor.accept(stream).await?, reply).await,
                None => exchange(stream, reply).await,
            }
        })
        .await?
    })
}

/// A deterministic JSON-RPC success reply with a closed connection.
fn success_reply() -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}", BODY.len())
}

/// Credential construction rejects a remote HTTP endpoint before any connection is possible.
#[tokio::test]
async fn remote_http_is_rejected_even_with_a_pinned_custom_transport() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let builder = reqwest::Client::builder().resolve("remote.example", address);
    let client = Client::with_http_client_builder(
        &format!("http://remote.example:{}", address.port()),
        builder,
    )?;
    assert!(matches!(
        client.with_bearer_token(TOKEN),
        Err(RpcError::InsecureAuthenticatedEndpoint)
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

/// Literal IPv4 and IPv6 loopback listeners receive the credential over direct HTTP.
#[tokio::test]
async fn loopback_http_sends_credentials_directly() -> TestResult {
    for bind in ["127.0.0.1:0", "[::1]:0"] {
        let listener = TcpListener::bind(bind).await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let server = serve(listener, None, success_reply());
        let result = Client::new(&endpoint)?
            .with_bearer_token(TOKEN)?
            .node_did(&NodeDidRequest {})
            .await?;
        assert_eq!(result.did, "fixture");
        assert!(server
            .await??
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {TOKEN}")));
    }
    Ok(())
}

/// HTTPS retains normal certificate verification while allowing an explicitly trusted test root.
#[tokio::test]
async fn https_with_trusted_certificate_succeeds() -> TestResult {
    let (tls, root) = tls_fixture()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = serve(listener, Some(tls), success_reply());
    let builder = reqwest::Client::builder()
        .add_root_certificate(root)
        .resolve("localhost", address);
    let client = Client::with_http_client_builder(
        &format!("https://localhost:{}", address.port()),
        builder,
    )?
    .with_bearer_token(TOKEN)?;
    assert_eq!(client.node_did(&NodeDidRequest {}).await?.did, "fixture");
    assert!(server.await??.contains(TOKEN));
    Ok(())
}

/// Redirects never reach the next listener, even when a caller supplies a permissive builder.
#[tokio::test]
async fn https_downgrade_and_http_redirects_are_not_followed() -> TestResult {
    for use_tls in [false, true] {
        let destination = TcpListener::bind("127.0.0.1:0").await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (tls, root) = tls_fixture()?;
        let reply = format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", destination.local_addr()?);
        let server = serve(listener, use_tls.then_some(tls), reply);
        let builder = reqwest::Client::builder()
            .add_root_certificate(root)
            .resolve("localhost", address)
            .redirect(reqwest::redirect::Policy::limited(10));
        let endpoint = if use_tls {
            format!("https://localhost:{}", address.port())
        } else {
            format!("http://{address}")
        };
        let client =
            Client::with_http_client_builder(&endpoint, builder)?.with_bearer_token(TOKEN)?;
        assert!(matches!(
            client.node_did(&NodeDidRequest {}).await,
            Err(RpcError::RedirectRejected)
        ));
        assert!(server.await??.contains(TOKEN));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), destination.accept())
                .await
                .is_err()
        );
    }
    Ok(())
}

/// A configured proxy cannot intercept the loopback HTTP credential exception.
#[tokio::test]
async fn loopback_credentials_bypass_caller_proxy() -> TestResult {
    let proxy = TcpListener::bind("127.0.0.1:0").await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let server = serve(listener, None, success_reply());
    let builder = reqwest::Client::builder().proxy(reqwest::Proxy::all(format!(
        "http://{}",
        proxy.local_addr()?
    ))?);
    let client = Client::with_http_client_builder(&endpoint, builder)?.with_bearer_token(TOKEN)?;
    assert_eq!(client.node_did(&NodeDidRequest {}).await?.did, "fixture");
    assert!(server.await??.contains(TOKEN));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), proxy.accept())
            .await
            .is_err()
    );
    Ok(())
}

/// The default TLS configuration rejects an untrusted certificate before HTTP credentials.
#[tokio::test]
async fn https_rejects_untrusted_certificate() -> TestResult {
    let (tls, _) = tls_fixture()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = serve(listener, Some(tls), success_reply());
    let builder = reqwest::Client::builder().resolve("localhost", address);
    let client = Client::with_http_client_builder(
        &format!("https://localhost:{}", address.port()),
        builder,
    )?
    .with_bearer_token(TOKEN)?;
    assert!(client.node_did(&NodeDidRequest {}).await.is_err());
    assert!(
        server.await?.is_err(),
        "no HTTP exchange may follow a rejected TLS handshake"
    );
    Ok(())
}

/// Public unauthenticated RPC still permits HTTP and retains caller DNS pinning.
#[tokio::test]
async fn unauthenticated_http_retains_dns_pin() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = serve(listener, None, success_reply());
    let builder = reqwest::Client::builder().resolve("remote.example", address);
    let client = Client::with_http_client_builder(
        &format!("http://remote.example:{}", address.port()),
        builder,
    )?;
    assert_eq!(client.node_did(&NodeDidRequest {}).await?.did, "fixture");
    assert!(!server
        .await??
        .to_ascii_lowercase()
        .contains("authorization:"));
    Ok(())
}
