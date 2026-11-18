use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::PoisonError;
use std::time::Duration;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
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
const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const CHANNEL_DATA_HEADER_BYTES: usize = 4;
const CHANNEL_NUMBER_MIN: u16 = 0x4000;
const CHANNEL_NUMBER_MAX: u16 = 0x7FFF;
const DEFAULT_TURN_PORT: u16 = 3478;
const DEFAULT_TURNS_PORT: u16 = 5349;
const TURN_TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const TURN_TCP_FRAME_BODY_TIMEOUT: Duration = Duration::from_secs(10);
const TURN_TCP_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const TURN_TCP_LOCAL_DELIVERY_TIMEOUT: Duration = Duration::from_millis(200);
const OS_UDP_PROBE_TIMEOUT: Duration = Duration::from_millis(50);
const BRIDGE_UDP_MTU: usize = STUN_HEADER_BYTES + (u16::MAX as usize);
const MAX_LOCAL_UDP_PAYLOAD_LEN: usize = 65_507;
const SESSION_QUEUE_CAPACITY: usize = 256;
const MAX_BRIDGE_SESSIONS: usize = 8;
const RECENT_STUN_TX_IDS_CAPACITY: usize = 32;

#[cfg(test)]
static FORCE_OS_UDP_BLOCKED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Guard that keeps any spawned TURN-over-TCP bridge tasks alive for the lifetime of a
/// native `RTCPeerConnection` and cancels them when the connection is reconfigured or closed.
pub(crate) struct TurnTcpBridgeGuard {
    inbound_cancel: CancellationToken,
    cancel: CancellationToken,
}

impl TurnTcpBridgeGuard {
    fn new(inbound_cancel: CancellationToken, cancel: CancellationToken) -> Self {
        Self {
            inbound_cancel,
            cancel,
        }
    }

    /// Stop inbound TCP-to-local packet injection before awaiting `RTCPeerConnection::close()`
    /// so a full virtual UDP queue cannot block while holding the `VNet` mutex needed by
    /// outbound `Refresh(LIFETIME=0)`.
    pub(crate) fn quiesce_inbound(&self) {
        self.inbound_cancel.cancel();
    }

    pub(crate) fn cancel(&self) {
        self.inbound_cancel.cancel();
        self.cancel.cancel();
    }
}

impl Drop for TurnTcpBridgeGuard {
    fn drop(&mut self) {
        self.inbound_cancel.cancel();
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
        if (length_u16 & 0b11) != 0 {
            return None;
        }
        let cookie = datagram.get(4..8)?;
        if cookie != STUN_MAGIC_COOKIE {
            return None;
        }
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

/// Extract the 12-byte STUN transaction ID if `pkt` is a STUN Request (RFC 5389 §6 class `0b00`).
fn stun_request_transaction_id(pkt: &[u8]) -> Option<[u8; 12]> {
    let &[b0, b1, ..] = pkt else {
        return None;
    };
    // Top two bits 00 (STUN) and class bits (bit 0 of b0, bit 4 of b1) both 0 (Request).
    if (b0 & 0b1100_0001) != 0 || (b1 & 0b0001_0000) != 0 {
        return None;
    }
    if pkt.get(4..8) != Some(&STUN_MAGIC_COOKIE) {
        return None;
    }
    let tx_slice = pkt.get(8..STUN_HEADER_BYTES)?;
    <[u8; 12]>::try_from(tx_slice).ok()
}

/// Extract the 12-byte STUN transaction ID if `pkt` is a STUN Success or Error Response
/// (RFC 5389 §6 class `0b10` or `0b11`, where bit 0 of byte 0 `C1 == 1`).
fn stun_response_transaction_id(pkt: &[u8]) -> Option<[u8; 12]> {
    let &b0 = pkt.first()?;
    if (b0 & 0b1100_0001) != 0b0000_0001 {
        return None;
    }
    if pkt.get(4..8) != Some(&STUN_MAGIC_COOKIE) {
        return None;
    }
    let tx_slice = pkt.get(8..STUN_HEADER_BYTES)?;
    <[u8; 12]>::try_from(tx_slice).ok()
}

/// Read a single RFC 5766 STUN message or `ChannelData` frame from a TCP/TLS stream.
pub(crate) async fn read_turn_tcp_frame<R>(reader: &mut R) -> std::io::Result<Vec<u8>>
where R: AsyncRead + Unpin {
    let mut prefix = [0u8; 4];
    reader.read_exact(&mut prefix).await?;
    let [b0, b1, b2, b3] = prefix;
    let first_u16 = u16::from_be_bytes([b0, b1]);
    let length_u16 = usize::from(u16::from_be_bytes([b2, b3]));

    if (b0 & 0b1100_0000) == 0 {
        if (length_u16 & 0b11) != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unaligned STUN message length over TCP: {length_u16}"),
            ));
        }
        let mut hdr_rest = [0u8; 16];
        tokio::time::timeout(
            TURN_TCP_FRAME_BODY_TIMEOUT,
            reader.read_exact(&mut hdr_rest),
        )
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out reading STUN header over TCP",
            )
        })??;
        if hdr_rest.get(..4) != Some(&STUN_MAGIC_COOKIE) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid STUN magic cookie on TCP stream",
            ));
        }
        let mut frame = vec![0u8; STUN_HEADER_BYTES.saturating_add(length_u16)];
        if let Some(head) = frame.get_mut(..4) {
            head.copy_from_slice(&prefix);
        }
        if let Some(rest) = frame.get_mut(4..STUN_HEADER_BYTES) {
            rest.copy_from_slice(&hdr_rest);
        }
        if let Some(body) = frame.get_mut(STUN_HEADER_BYTES..) {
            if !body.is_empty() {
                tokio::time::timeout(TURN_TCP_FRAME_BODY_TIMEOUT, reader.read_exact(body))
                    .await
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "timed out reading STUN body over TCP",
                        )
                    })??;
            }
        }
        Ok(frame)
    } else if (CHANNEL_NUMBER_MIN..=CHANNEL_NUMBER_MAX).contains(&first_u16) {
        let padded_body_bytes = (length_u16 + 3) & !3;
        let mut frame = vec![0u8; CHANNEL_DATA_HEADER_BYTES.saturating_add(padded_body_bytes)];
        if let Some(head) = frame.get_mut(..4) {
            head.copy_from_slice(&prefix);
        }
        if let Some(body) = frame.get_mut(4..) {
            if !body.is_empty() {
                tokio::time::timeout(TURN_TCP_FRAME_BODY_TIMEOUT, reader.read_exact(body))
                    .await
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "timed out reading ChannelData body over TCP",
                        )
                    })??;
            }
        }
        Ok(frame)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid TURN TCP frame header: 0x{first_u16:04x}"),
        ))
    }
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
    let probe = *b"ping";
    if sock.send_to(&probe, addr).await.is_err() {
        return false;
    }
    let mut recv_buf = [0u8; 8];
    matches!(
        tokio::time::timeout(OS_UDP_PROBE_TIMEOUT, sock.recv_from(&mut recv_buf)).await,
        Ok(Ok((4, _))) if recv_buf.get(..4) == Some(&probe)
    )
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

type StunTxCache = Arc<SyncMutex<VecDeque<([u8; 12], Option<Vec<u8>>)>>>;

async fn run_stream_io<S>(
    stream: S,
    mut outbound_rx: mpsc::Receiver<Vec<u8>>,
    bridge_conn: Arc<dyn Conn + Send + Sync>,
    client_addr: SocketAddr,
    inbound_cancel: CancellationToken,
    cancel: CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let stun_tx_cache: StunTxCache = Arc::new(SyncMutex::new(VecDeque::with_capacity(
        RECENT_STUN_TX_IDS_CAPACITY,
    )));
    let cache_for_write = Arc::clone(&stun_tx_cache);
    let bridge_for_write = Arc::clone(&bridge_conn);
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
            // Suppress duplicate STUN request retransmissions over reliable TCP/TLS (RFC 5389 §7.2.2).
            // If the server already responded to this transaction ID and the response was lost on the
            // local UDP hop, replay the cached response directly without re-sending over TCP.
            if let Some(tx_id) = stun_request_transaction_id(&pkt) {
                let duplicate_action = {
                    let mut guard = cache_for_write
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if let Some((_, cached_resp)) = guard.iter().find(|(id, _)| *id == tx_id) {
                        Some(cached_resp.clone())
                    } else {
                        if guard.len() >= RECENT_STUN_TX_IDS_CAPACITY {
                            guard.pop_front();
                        }
                        guard.push_back((tx_id, None));
                        None
                    }
                };
                if let Some(maybe_cached_resp) = duplicate_action {
                    if let Some(resp) = maybe_cached_resp {
                        if resp.len() <= MAX_LOCAL_UDP_PAYLOAD_LEN {
                            let _ = tokio::time::timeout(
                                TURN_TCP_LOCAL_DELIVERY_TIMEOUT,
                                bridge_for_write.send_to(&resp, client_addr),
                            )
                            .await;
                        }
                    }
                    continue;
                }
            }
            let write_res = tokio::time::timeout(TURN_TCP_WRITE_TIMEOUT, async {
                writer.write_all(&pkt).await?;
                writer.flush().await
            })
            .await;
            if !matches!(write_res, Ok(Ok(()))) {
                break;
            }
        }
        let _ = tokio::time::timeout(TURN_TCP_WRITE_TIMEOUT, writer.shutdown()).await;
    };

    let cache_for_read = Arc::clone(&stun_tx_cache);
    let cancel_read = cancel.clone();
    let read_fut = async move {
        let mut quiesced = false;
        loop {
            let frame_res = tokio::select! {
                _ = inbound_cancel.cancelled() => {
                    quiesced = true;
                    break;
                }
                res = read_turn_tcp_frame(&mut reader) => res,
            };
            let Ok(frame) = frame_res else {
                break;
            };
            if frame.len() > MAX_LOCAL_UDP_PAYLOAD_LEN {
                tracing::debug!(
                    len = frame.len(),
                    "dropping inbound TURN TCP frame exceeding local UDP datagram limit"
                );
                continue;
            }
            if let Some(tx_id) = stun_response_transaction_id(&frame) {
                let mut guard = cache_for_read
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some((_, slot)) = guard.iter_mut().find(|(id, _)| *id == tx_id) {
                    *slot = Some(frame.clone());
                } else {
                    if guard.len() >= RECENT_STUN_TX_IDS_CAPACITY {
                        guard.pop_front();
                    }
                    guard.push_back((tx_id, Some(frame.clone())));
                }
            }
            let send_res = tokio::select! {
                _ = inbound_cancel.cancelled() => {
                    quiesced = true;
                    break;
                }
                res = tokio::time::timeout(
                    TURN_TCP_LOCAL_DELIVERY_TIMEOUT,
                    bridge_conn.send_to(&frame, client_addr),
                ) => match res {
                    Ok(inner) => inner,
                    Err(_) => break,
                },
            };
            if send_res.is_err() {
                break;
            }
        }
        if quiesced {
            // Keep `read_fut` pending after inbound quiescence so `tokio::select!` does not drop
            // `write_fut` while `RTCPeerConnection::close()` is transmitting `Refresh(LIFETIME=0)`.
            cancel_read.cancelled().await;
        }
    };

    tokio::select! {
        _ = cancel.cancelled() => {},
        _ = write_fut => {},
        _ = read_fut => {},
    }
}

async fn run_client_tcp_session(
    endpoint: TurnTcpEndpoint,
    outbound_rx: mpsc::Receiver<Vec<u8>>,
    bridge_conn: Arc<dyn Conn + Send + Sync>,
    client_addr: SocketAddr,
    inbound_cancel: CancellationToken,
    cancel: CancellationToken,
) {
    let connect_res = tokio::select! {
        _ = cancel.cancelled() => return,
        res = tokio::time::timeout(
            TURN_TCP_CONNECT_TIMEOUT,
            TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
        ) => res,
    };
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
        let tls_res = tokio::select! {
            _ = cancel.cancelled() => return,
            res = tokio::time::timeout(
                TURN_TCP_CONNECT_TIMEOUT,
                connector.connect(server_name, tcp_stream),
            ) => res,
        };
        match tls_res {
            Ok(Ok(tls_stream)) => {
                run_stream_io(
                    tls_stream,
                    outbound_rx,
                    bridge_conn,
                    client_addr,
                    inbound_cancel,
                    cancel,
                )
                .await;
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
        run_stream_io(
            tcp_stream,
            outbound_rx,
            bridge_conn,
            client_addr,
            inbound_cancel,
            cancel,
        )
        .await;
    }
}

struct BridgeSession {
    tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
    last_active: Instant,
}

fn spawn_turn_tcp_bridge(
    bridge_conn: Arc<dyn Conn + Send + Sync>,
    endpoint: TurnTcpEndpoint,
    inbound_cancel: CancellationToken,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut sessions: HashMap<SocketAddr, BridgeSession> = HashMap::new();
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
            let Some(mut framed) = encode_turn_tcp_frame(datagram) else {
                continue;
            };

            sessions.retain(|_, session| !session.tx.is_closed());
            let now = Instant::now();

            if let Some(existing) = sessions.get_mut(&client_addr) {
                existing.last_active = now;
                match existing.tx.try_send(framed) {
                    Ok(()) => continue,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::debug!(
                            %client_addr,
                            "TURN TCP bridge outbound queue full; dropping packet without resetting TCP session"
                        );
                        continue;
                    }
                    Err(mpsc::error::TrySendError::Closed(returned)) => {
                        sessions.remove(&client_addr);
                        framed = returned;
                    }
                }
            }

            if sessions.len() >= MAX_BRIDGE_SESSIONS {
                if let Some(oldest_addr) = sessions
                    .iter()
                    .min_by_key(|(_, s)| s.last_active)
                    .map(|(addr, _)| *addr)
                {
                    if let Some(evicted) = sessions.remove(&oldest_addr) {
                        evicted.cancel.cancel();
                    }
                }
            }

            let (tx, rx) = mpsc::channel(SESSION_QUEUE_CAPACITY);
            let _ = tx.try_send(framed);
            let session_cancel = cancel.child_token();
            let session_inbound_cancel = inbound_cancel.child_token();
            sessions.insert(client_addr, BridgeSession {
                tx,
                cancel: session_cancel.clone(),
                last_active: now,
            });
            let session_endpoint = endpoint.clone();
            let session_conn = Arc::clone(&bridge_conn);
            tokio::spawn(async move {
                run_client_tcp_session(
                    session_endpoint,
                    rx,
                    session_conn,
                    client_addr,
                    session_inbound_cancel,
                    session_cancel,
                )
                .await;
            });
        }

        let _ = bridge_conn.close().await;
    });
}

/// Prepare the `RTCIceServer` list, `RTCIceTransportPolicy`, and optional `VNet` + TCP TURN
/// bridges for `create_peer_connection_with_config`.
pub(crate) async fn prepare_native_ice_config(
    ice_servers: &[IceServer],
) -> Result<PreparedNativeIceConfig> {
    let mut tcp_turn_urls = 0usize;
    for server in ice_servers {
        for url in &server.urls {
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

    let use_vnet_relay_only = !os_udp_ok;
    let net = if use_vnet_relay_only {
        Arc::new(Net::new(Some(NetConfig::default())))
    } else {
        Arc::new(Net::new(None))
    };

    let cancel = CancellationToken::new();
    let inbound_cancel = cancel.child_token();
    let bridge_guard = TurnTcpBridgeGuard::new(inbound_cancel.clone(), cancel.clone());
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
                spawn_turn_tcp_bridge(
                    bridge_conn,
                    endpoint,
                    inbound_cancel.child_token(),
                    cancel.child_token(),
                );
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
        bridge_guard: Some(bridge_guard),
        disable_mdns: use_vnet_relay_only || !os_udp_ok,
    })
}

#[cfg(test)]
pub(crate) fn set_force_os_udp_blocked_for_test(blocked: bool) {
    FORCE_OS_UDP_BLOCKED.store(blocked, std::sync::atomic::Ordering::SeqCst);
}
