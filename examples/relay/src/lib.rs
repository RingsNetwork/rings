//! Core, UI-free helpers for the Rings TCP/UDP relay examples.
//!
//! A relay turns a Rings overlay link into a transparent byte pipe: a **server** node
//! exposes a local socket address under a service name, and a **client** node binds a
//! local listener that forwards every connection/flow to that service across the
//! overlay. The same code path drives TCP and UDP — only the `TransportKind` differs.
//!
//! These helpers are shared by the runnable examples (`tcp.rs`, `udp.rs`) and the
//! integration test, so the demo and the test exercise exactly the same flow.

use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::session::SessionSk;
use rings_core::storage::MemStorage;
use rings_node::extension::protocols::relay::RelayHandle;
use rings_node::processor::Processor;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::UdpSocket;

/// A boxed error result for the example/test glue.
pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Build a fresh in-memory node and start its message loop.
///
/// The relay is an opt-in extension that owns its engine, so we install it explicitly
/// (`RelayHandle::install`) rather than baking it into the provider; `set_backend()` then
/// installs the extension backend so inbound overlay envelopes reach the protocol registry.
/// Returns the processor (for the handshake), the provider (generic node capabilities), and
/// the relay handle (open tunnels / register services).
pub async fn spawn_node() -> (Arc<Processor>, Arc<Provider>, RelayHandle) {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).expect("session sk");
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk,
        3,
    );
    let processor = Arc::new(
        ProcessorBuilder::from_config(&config)
            .expect("processor builder")
            .storage(Box::new(MemStorage::new()))
            .build()
            .expect("build processor"),
    );
    let provider = Arc::new(Provider::from_processor(processor.clone()));
    let relay = RelayHandle::install(&provider.extensions()).expect("install relay");
    provider.set_backend().expect("install backend");

    let listening = processor.clone();
    tokio::spawn(async move { listening.listen().await });

    (processor, provider, relay)
}

/// Establish an overlay link between two nodes via the offer/answer handshake, then
/// wait until the connection reports `connected` (or time out).
pub async fn connect(a: &Arc<Processor>, b: &Arc<Processor>) -> Result<()> {
    let offer = a.swarm.create_offer(b.swarm.did()).await?;
    let answer = b.swarm.answer_offer(offer).await?;
    a.swarm.accept_answer(answer).await?;

    if !wait_connected(a, b.swarm.did(), Duration::from_secs(30)).await {
        return Err("peers did not reach the connected state in time".into());
    }
    Ok(())
}

/// Poll `a`'s connection table until `peer` is connected, or `timeout` elapses.
pub async fn wait_connected(a: &Arc<Processor>, peer: Did, timeout: Duration) -> bool {
    let peer = peer.to_string();
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let connected = a
            .swarm
            .peers()
            .iter()
            .any(|p| p.did == peer && p.state.to_lowercase().contains("connected"));
        if connected {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Reserve a free `127.0.0.1` port by binding and immediately dropping a listener.
pub async fn free_local_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    addr
}

/// Start a local TCP echo server, returning the address it listens on.
pub async fn spawn_tcp_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let addr = listener.local_addr().expect("echo addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (mut r, mut w) = stream.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    addr
}

/// Start a local UDP echo server, returning the address it is bound to.
pub async fn spawn_udp_echo() -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind udp echo");
    let addr = socket.local_addr().expect("udp echo addr");
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65_536];
        loop {
            let Ok((n, peer)) = socket.recv_from(&mut buf).await else {
                break;
            };
            let _ = socket.send_to(&buf[..n], peer).await;
        }
    });
    addr
}

/// Run the full TCP relay round-trip and return what the relay echoed back.
///
/// server exposes a local echo service; client binds a tunnel that forwards to it over
/// the overlay; we then talk plain TCP to the tunnel and read the echoed bytes.
pub async fn tcp_round_trip(request: &[u8]) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    let (server_p, _server, server_relay) = spawn_node().await;
    let (client_p, _client, client_relay) = spawn_node().await;
    connect(&client_p, &server_p).await?;

    let echo_addr = spawn_tcp_echo().await;
    server_relay
        .register_tcp_service("echo".to_string(), echo_addr)
        .await?;

    let tunnel_addr = free_local_addr().await;
    client_relay
        .open_tcp_tunnel(tunnel_addr, server_p.swarm.did(), "echo".to_string())
        .await?;
    // Give the tunnel listener a moment to bind.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut stream = TcpStream::connect(tunnel_addr).await?;
    stream.write_all(request).await?;

    let mut got = vec![0u8; request.len()];
    stream.read_exact(&mut got).await?;
    Ok(got)
}

/// Relay an HTTP request to a real external host **through a peer**: `A → B → host`.
///
/// The server node B registers a service pointing at `target` (e.g. `google.com:80`);
/// the client node A binds a tunnel to it and we speak plain HTTP to that tunnel, so the
/// request travels client → overlay → B → `target` and the response comes back the same
/// way. Returns the raw bytes B relayed back from `target`.
pub async fn relay_http_get(target: &str, request: &[u8]) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    let (server_p, _server, server_relay) = spawn_node().await;
    let (client_p, _client, client_relay) = spawn_node().await;
    connect(&client_p, &server_p).await?;

    // B's exit service → the external host (resolved to a socket address).
    let target_addr = tokio::net::lookup_host(target)
        .await?
        .next()
        .ok_or("could not resolve target host")?;
    server_relay
        .register_tcp_service("web".to_string(), target_addr)
        .await?;

    let tunnel_addr = free_local_addr().await;
    client_relay
        .open_tcp_tunnel(tunnel_addr, server_p.swarm.did(), "web".to_string())
        .await?;

    // Talk HTTP to the local tunnel; bytes flow A → overlay → B → target and back. Each
    // `connect` opens a fresh relay session; retry until one yields an HTTP response (the
    // first attempt can race tunnel/service warmup).
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Ok(mut stream) = TcpStream::connect(tunnel_addr).await else {
            continue;
        };
        if stream.write_all(request).await.is_err() {
            continue;
        }
        let mut body = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut body)).await;
        if body.starts_with(b"HTTP/") {
            return Ok(body);
        }
    }
    Err("relay did not return an HTTP response from the target".into())
}

/// Run the full UDP relay round-trip and return what the relay echoed back.
pub async fn udp_round_trip(request: &[u8]) -> Result<Vec<u8>> {
    let (server_p, _server, server_relay) = spawn_node().await;
    let (client_p, _client, client_relay) = spawn_node().await;
    connect(&client_p, &server_p).await?;

    let echo_addr = spawn_udp_echo().await;
    server_relay
        .register_udp_service("echo".to_string(), echo_addr)
        .await?;

    let tunnel_addr = free_local_addr().await;
    client_relay
        .open_udp_tunnel(tunnel_addr, server_p.swarm.did(), "echo".to_string())
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    socket.send_to(request, tunnel_addr).await?;

    let mut buf = vec![0u8; request.len()];
    let (n, _) = tokio::time::timeout(Duration::from_secs(5), socket.recv_from(&mut buf)).await??;
    Ok(buf[..n].to_vec())
}
