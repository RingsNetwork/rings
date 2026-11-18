use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::rustls;
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;
use url::Url;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::util::vnet::net::Net;
use webrtc::util::vnet::net::NetConfig;
use webrtc::util::Conn;

use crate::error::Error;
use crate::error::Result;
use crate::ice_server::IceServer;

const STUN_HEADER_BYTES: usize = 20;
const CHANNEL_DATA_HEADER_BYTES: usize = 4;
const CHANNEL_NUMBER_MIN: u16 = 0x4000;
const CHANNEL_NUMBER_MAX: u16 = 0xFFFE;
const DEFAULT_TURN_PORT: u16 = 3478;
const DEFAULT_TURNS_PORT: u16 = 5349;
const TURN_TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const BRIDGE_UDP_MTU: usize = 65_535;
const SESSION_QUEUE_CAPACITY: usize = 256;

#[cfg(test)]
static FORCE_OS_UDP_BLOCKED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Guard that keeps any spawned TURN-over-TCP bridge tasks alive for the lifetime of a
/// native `RTCPeerConnection` and cancels them when the connection is reconfigured or closed.
pub(crate) struct TurnTcpBridgeGuard {
    cancel: CancellationToken,
}

impl TurnTcpBridgeGuard {
    fn new(cancel: CancellationToken) -> Self {
        Self { cancel }
    }

    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for TurnTcpBridgeGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Target endpoint for a TCP-based TURN or TURNS server.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TurnTcpEndpoint {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) use_tls: bool,
}

/// Prepared ICE server configuration and optional virtual network for a native `RTCPeerConnection`.
pub(crate) struct PreparedNativeIceConfig {
    pub(crate) ice_servers: Vec<RTCIceServer>,
    pub(crate) ice_transport_policy: RTCIceTransportPolicy,
    pub(crate) vnet: Option<Arc<Net>>,
    pub(crate) bridge_guard: Option<TurnTcpBridgeGuard>,
    pub(crate) disable_mdns: bool,
}

/// Parse a `turn:` or `turns:` URL and return `Some(TurnTcpEndpoint)` when it uses TCP transport
/// (`turn:host[:port]?transport=tcp` or `turns:host[:port][?transport=tcp]`).
pub(crate) fn parse_turn_tcp_url(raw_url: &str) -> Option<TurnTcpEndpoint> {
    let trimmed = raw_url.trim();
    let normalized;
    let to_parse = if !trimmed.contains("://") {
        let (scheme_part, rest) = trimmed.split_once(':')?;
        if rest.is_empty() || rest.starts_with('/') {
            return None;
        }
        normalized = format!("{scheme_part}://{rest}");
        normalized.as_str()
    } else {
        trimmed
    };

    let parsed = Url::parse(to_parse).ok()?;
    let scheme = parsed.scheme();
    let is_turn = scheme.eq_ignore_ascii_case("turn");
    let is_turns = scheme.eq_ignore_ascii_case("turns");
    if !is_turn && !is_turns {
        return None;
    }

    let host = parsed
        .host_str()?
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if host.is_empty() {
        return None;
    }

    let mut transport_param: Option<String> = None;
    for (k, v) in parsed.query_pairs() {
        if k.eq_ignore_ascii_case("transport") {
            transport_param = Some(v.into_owned());
        }
    }

    let is_tcp = match transport_param.as_deref() {
        Some(proto) if proto.eq_ignore_ascii_case("tcp") => true,
        Some(_) => false,
        None => is_turns,
    };
    if !is_tcp {
        return None;
    }

    let port = parsed.port().unwrap_or(if is_turns {
        DEFAULT_TURNS_PORT
    } else {
        DEFAULT_TURN_PORT
    });

    Some(TurnTcpEndpoint {
        host,
        port,
        use_tls: is_turns,
    })
}

/// Encode an outbound STUN message or TURN `ChannelData` datagram for transmission over a
/// stream-oriented TCP/TLS TURN connection (RFC 5766 §7.3 & §11.5).
pub(crate) fn encode_turn_tcp_frame(datagram: &[u8]) -> Option<Vec<u8>> {
    let &[b0, b1, b2, b3, ..] = datagram else {
        return None;
    };
    let first_u16 = u16::from_be_bytes([b0, b1]);
    let length_u16 = usize::from(u16::from_be_bytes([b2, b3]));

    if (b0 & 0b1100_0000) == 0 {
        let total = STUN_HEADER_BYTES.checked_add(length_u16)?;
        let slice = datagram.get(..total)?;
        Some(slice.to_vec())
    } else if (CHANNEL_NUMBER_MIN..=CHANNEL_NUMBER_MAX).contains(&first_u16) {
        let unpadded_total = CHANNEL_DATA_HEADER_BYTES.checked_add(length_u16)?;
        let slice = datagram.get(..unpadded_total)?;
        let padded_data_len = (length_u16 + 3) & !3;
        let padded_total = CHANNEL_DATA_HEADER_BYTES.checked_add(padded_data_len)?;
        let mut out = Vec::with_capacity(padded_total);
        out.extend_from_slice(slice);
        out.resize(padded_total, 0u8);
        Some(out)
    } else {
        None
    }
}

/// Read a single RFC 5766 STUN message or `ChannelData` frame from a TCP/TLS stream.
pub(crate) async fn read_turn_tcp_frame<R>(reader: &mut R) -> std::io::Result<Vec<u8>>
where R: AsyncRead + Unpin {
    let mut prefix = [0u8; 4];
    reader.read_exact(&mut prefix).await?;
    let [b0, b1, b2, b3] = prefix;
    let first_u16 = u16::from_be_bytes([b0, b1]);
    let length_u16 = usize::from(u16::from_be_bytes([b2, b3]));

    let body_bytes = if (b0 & 0b1100_0000) == 0 {
        16usize.saturating_add(length_u16)
    } else if (CHANNEL_NUMBER_MIN..=CHANNEL_NUMBER_MAX).contains(&first_u16) {
        (length_u16 + 3) & !3
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid TURN TCP frame header: 0x{first_u16:04x}"),
        ));
    };

    let mut frame = vec![0u8; 4usize.saturating_add(body_bytes)];
    if let Some(head) = frame.get_mut(..4) {
        head.copy_from_slice(&prefix);
    }
    if let Some(tail) = frame.get_mut(4..) {
        reader.read_exact(tail).await?;
    }
    Ok(frame)
}

async fn can_use_os_udp() -> bool {
    #[cfg(test)]
    if FORCE_OS_UDP_BLOCKED.load(std::sync::atomic::Ordering::SeqCst) {
        return false;
    }

    let Ok(sock) = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await else {
        return false;
    };
    let Ok(addr) = sock.local_addr() else {
        return false;
    };
    sock.send_to(&[0u8], addr).await.is_ok()
}

fn build_tls_connector() -> std::io::Result<TlsConnector> {
    let root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| std::io::Error::other(e.to_string()))?
    .with_root_certificates(root_store)
    .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

async fn run_stream_io<S>(
    stream: S,
    mut outbound_rx: mpsc::Receiver<Vec<u8>>,
    bridge_conn: Arc<dyn Conn + Send + Sync>,
    client_addr: SocketAddr,
    cancel: CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let cancel_write = cancel.clone();
    let write_fut = async move {
        loop {
            let maybe_pkt = tokio::select! {
                _ = cancel_write.cancelled() => break,
                pkt = outbound_rx.recv() => pkt,
            };
            let Some(pkt) = maybe_pkt else {
                break;
            };
            if writer.write_all(&pkt).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    };

    let cancel_read = cancel.clone();
    let read_fut = async move {
        loop {
            let frame_res = tokio::select! {
                _ = cancel_read.cancelled() => break,
                res = read_turn_tcp_frame(&mut reader) => res,
            };
            let Ok(frame) = frame_res else {
                break;
            };
            if bridge_conn.send_to(&frame, client_addr).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = write_fut => {},
        _ = read_fut => {},
    }
}

async fn run_client_tcp_session(
    endpoint: TurnTcpEndpoint,
    outbound_rx: mpsc::Receiver<Vec<u8>>,
    bridge_conn: Arc<dyn Conn + Send + Sync>,
    client_addr: SocketAddr,
    cancel: CancellationToken,
) {
    let connect_res = tokio::time::timeout(
        TURN_TCP_CONNECT_TIMEOUT,
        TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
    )
    .await;
    let tcp_stream = match connect_res {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            tracing::warn!(
                host = %endpoint.host,
                port = endpoint.port,
                %error,
                "Failed to connect TCP stream to TURN server"
            );
            return;
        }
        Err(_) => {
            tracing::warn!(
                host = %endpoint.host,
                port = endpoint.port,
                "Timed out connecting TCP stream to TURN server"
            );
            return;
        }
    };
    let _ = tcp_stream.set_nodelay(true);

    if endpoint.use_tls {
        let Ok(connector) = build_tls_connector() else {
            return;
        };
        let Ok(server_name) = rustls::pki_types::ServerName::try_from(endpoint.host.clone()) else {
            tracing::warn!(host = %endpoint.host, "Invalid ServerName for TURNS TLS connection");
            return;
        };
        let tls_res = tokio::time::timeout(
            TURN_TCP_CONNECT_TIMEOUT,
            connector.connect(server_name, tcp_stream),
        )
        .await;
        match tls_res {
            Ok(Ok(tls_stream)) => {
                run_stream_io(tls_stream, outbound_rx, bridge_conn, client_addr, cancel).await;
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    host = %endpoint.host,
                    port = endpoint.port,
                    %error,
                    "TLS handshake with TURNS server failed"
                );
            }
            Err(_) => {
                tracing::warn!(
                    host = %endpoint.host,
                    port = endpoint.port,
                    "Timed out during TLS handshake with TURNS server"
                );
            }
        }
    } else {
        run_stream_io(tcp_stream, outbound_rx, bridge_conn, client_addr, cancel).await;
    }
}

fn spawn_turn_tcp_bridge(
    bridge_conn: Arc<dyn Conn + Send + Sync>,
    endpoint: TurnTcpEndpoint,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut sessions: HashMap<SocketAddr, mpsc::Sender<Vec<u8>>> = HashMap::new();
        let mut buf = vec![0u8; BRIDGE_UDP_MTU];

        loop {
            let recv_res = tokio::select! {
                _ = cancel.cancelled() => break,
                res = bridge_conn.recv_from(&mut buf) => res,
            };
            let Ok((n, client_addr)) = recv_res else {
                break;
            };
            let Some(datagram) = buf.get(..n) else {
                continue;
            };
            let Some(framed) = encode_turn_tcp_frame(datagram) else {
                continue;
            };

            let mut delivered = false;
            if let Some(existing_tx) = sessions.get(&client_addr) {
                if existing_tx.try_send(framed.clone()).is_ok() {
                    delivered = true;
                }
            }
            if !delivered {
                let (tx, rx) = mpsc::channel(SESSION_QUEUE_CAPACITY);
                let _ = tx.try_send(framed);
                sessions.insert(client_addr, tx);
                let session_endpoint = endpoint.clone();
                let session_conn = Arc::clone(&bridge_conn);
                let session_cancel = cancel.child_token();
                tokio::spawn(async move {
                    run_client_tcp_session(
                        session_endpoint,
                        rx,
                        session_conn,
                        client_addr,
                        session_cancel,
                    )
                    .await;
                });
            }
        }

        let _ = bridge_conn.close().await;
    });
}

/// Prepare the `RTCIceServer` list, `RTCIceTransportPolicy`, and optional `VNet` + TCP TURN
/// bridges for `create_peer_connection_with_config`.
pub(crate) async fn prepare_native_ice_config(
    ice_servers: &[IceServer],
    external_address: Option<&str>,
) -> Result<PreparedNativeIceConfig> {
    let mut total_urls = 0usize;
    let mut tcp_turn_urls = 0usize;
    for server in ice_servers {
        for url in &server.urls {
            total_urls += 1;
            if parse_turn_tcp_url(url).is_some() {
                tcp_turn_urls += 1;
            }
        }
    }

    let os_udp_ok = can_use_os_udp().await;
    if tcp_turn_urls == 0 {
        return Ok(PreparedNativeIceConfig {
            ice_servers: ice_servers
                .iter()
                .cloned()
                .map(RTCIceServer::from)
                .collect(),
            ice_transport_policy: RTCIceTransportPolicy::All,
            vnet: None,
            bridge_guard: None,
            disable_mdns: !os_udp_ok,
        });
    }

    let all_urls_are_tcp_turn = total_urls > 0 && tcp_turn_urls == total_urls;
    let use_vnet_relay_only = !os_udp_ok || (all_urls_are_tcp_turn && external_address.is_none());

    let net = if use_vnet_relay_only {
        Arc::new(Net::new(Some(NetConfig::default())))
    } else {
        Arc::new(Net::new(None))
    };

    let cancel = CancellationToken::new();
    let mut bridged_servers = Vec::with_capacity(ice_servers.len());

    for server in ice_servers {
        let mut rewritten_urls = Vec::with_capacity(server.urls.len());
        for url in &server.urls {
            if let Some(endpoint) = parse_turn_tcp_url(url) {
                let bridge_conn = net
                    .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                    .await
                    .map_err(|e| Error::Webrtc(webrtc::error::Error::Util(e)))?;
                let bridge_addr = bridge_conn
                    .local_addr()
                    .map_err(|e| Error::Webrtc(webrtc::error::Error::Util(e)))?;
                spawn_turn_tcp_bridge(bridge_conn, endpoint, cancel.child_token());
                rewritten_urls.push(format!("turn:{bridge_addr}"));
            } else if !use_vnet_relay_only {
                rewritten_urls.push(url.clone());
            }
        }

        if !rewritten_urls.is_empty() {
            let mut adapted = server.clone();
            adapted.urls = rewritten_urls;
            bridged_servers.push(RTCIceServer::from(adapted));
        }
    }

    Ok(PreparedNativeIceConfig {
        ice_servers: bridged_servers,
        ice_transport_policy: if use_vnet_relay_only {
            RTCIceTransportPolicy::Relay
        } else {
            RTCIceTransportPolicy::All
        },
        vnet: use_vnet_relay_only.then_some(net),
        bridge_guard: Some(TurnTcpBridgeGuard::new(cancel)),
        disable_mdns: use_vnet_relay_only || !os_udp_ok,
    })
}

#[cfg(test)]
pub(crate) fn set_force_os_udp_blocked_for_test(blocked: bool) {
    FORCE_OS_UDP_BLOCKED.store(blocked, std::sync::atomic::Ordering::SeqCst);
}
