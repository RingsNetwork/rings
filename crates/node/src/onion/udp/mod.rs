//! `udp`: an unconnected OS UDP datagram session with stateful peer-IP pinholes (#1292).
//!
//! Each `onion::udp` session binds a dedicated OS UDP socket at the exit, announces its bound
//! external endpoint in an initial [`OnionUdpServerFrame::Bound`] frame, transmits client
//! [`OnionUdpClientFrame::SendDatagram`] payloads from that socket, and forwards inbound UDP
//! datagrams only from peer IPs holding a live pinhole in [`OnionUdpPinholeTable`].
//!
//! Laws:
//! - **Bound identity**: the initial server frame is [`OnionUdpServerFrame::Bound`] carrying a
//!   non-unspecified IP and non-zero port; exits without an advertisable address fail closed at
//!   open.
//! - **Inbound pinhole gate**: inbound datagrams from peer IPs absent from [`OnionUdpPinholeTable`]
//!   (or disallowed by egress policy) are dropped at the OS socket before consuming return SURBs
//!   or exit byte budget.
//! - **Open quantum**: exit socket bind outcomes (success or failure) are released at multiples of
//!   250 ms, matching `onion::tcp`.
//! - **Lock-free close**: [`NativeOnionUdpSession::close`] signals closure without awaiting any
//!   reader or writer lock, waking any parked [`NativeOnionUdpSession::recv_from`] immediately and
//!   dropping the underlying session stream so `abort` leaves toward the exit.

use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
#[cfg(rings_native)]
use std::sync::atomic::AtomicBool;
#[cfg(rings_native)]
use std::sync::atomic::Ordering;
#[cfg(rings_native)]
use std::sync::Arc;
#[cfg(rings_native)]
use std::sync::Mutex as SyncMutex;
#[cfg(rings_native)]
use std::time::Duration;

#[cfg(rings_native)]
use bytes::Bytes;
#[cfg(rings_native)]
use bytes::BytesMut;
#[cfg(rings_native)]
use rings_core::utils::SharedClock;
#[cfg(rings_native)]
use tokio::net::UdpSocket;
#[cfg(rings_native)]
use tokio::sync::Mutex as AsyncMutex;
#[cfg(rings_native)]
use tokio::sync::Notify;
#[cfg(rings_native)]
use tokio::time::timeout;
#[cfg(rings_native)]
use tokio::time::Instant;
use url::Url;

use crate::error::Error;
use crate::error::Result;
#[cfg(rings_native)]
use crate::onion::exit_accounting::OnionExitAccounting;
#[cfg(rings_native)]
use crate::onion::session::dial::OnionClientStream;
#[cfg(rings_native)]
use crate::onion::session::dial::OnionStreamEvent;
#[cfg(rings_native)]
use crate::onion::session::dial::OnionStreamReceiver;
#[cfg(rings_native)]
use crate::onion::session::dial::OnionStreamSender;
#[cfg(rings_native)]
use crate::onion::session::dial::ONION_SESSION_OPEN_TIMEOUT;
#[cfg(rings_native)]
use crate::onion::session::serve::OnionWorld;
#[cfg(rings_native)]
use crate::onion::session::serve::OnionWorldReader;
#[cfg(rings_native)]
use crate::onion::session::serve::OnionWorldWriter;
#[cfg(rings_native)]
use crate::onion::OnionExitPolicy;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRouteError;
#[cfg(rings_native)]
use crate::sync_lock::lock;

pub mod codec;
pub mod pinhole;

pub use codec::decode_next_client_frame;
pub use codec::decode_next_server_frame;
pub use codec::encode_client_frame;
pub use codec::encode_server_frame;
pub use codec::OnionUdpClientFrame;
pub use codec::OnionUdpServerFrame;
pub use codec::MAX_ONION_UDP_FRAME_BYTES;
pub use codec::MAX_ONION_UDP_PAYLOAD_BYTES;
pub use codec::ONION_UDP_FRAME_LEN_BYTES;
pub use pinhole::OnionUdpPinholeTable;
pub use pinhole::MAX_PINHOLES_PER_SESSION;
pub use pinhole::MAX_PINHOLE_TTL_MS;

/// Socket-bind request mode encoded as the canonical `OnionProxyTarget` of an `onion::udp` open.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OnionUdpBindMode {
    /// Bind an IPv4 UDP socket on any available port (`0.0.0.0:1`).
    #[default]
    Ipv4AnyPort,
    /// Bind an IPv4 UDP socket on an even port (`0.0.0.0:2`).
    Ipv4EvenPort,
    /// Bind an IPv6 UDP socket on any available port (`[::]:1`).
    Ipv6AnyPort,
    /// Bind an IPv6 UDP socket on an even port (`[::]:2`).
    Ipv6EvenPort,
}

impl OnionUdpBindMode {
    /// Encode this bind mode as a canonical [`OnionProxyTarget`] authority.
    pub fn to_target(self) -> OnionProxyTarget {
        let (host, port) = match self {
            Self::Ipv4AnyPort => ("0.0.0.0", 1),
            Self::Ipv4EvenPort => ("0.0.0.0", 2),
            Self::Ipv6AnyPort => ("::", 1),
            Self::Ipv6EvenPort => ("::", 2),
        };
        OnionProxyTarget::from_canonical_literal(host, port)
    }

    /// Parse an [`OnionProxyTarget`] authority into an [`OnionUdpBindMode`].
    pub fn from_target(target: &OnionProxyTarget) -> Result<Self> {
        match (target.host(), target.port()) {
            ("0.0.0.0", 1) => Ok(Self::Ipv4AnyPort),
            ("0.0.0.0", 2) => Ok(Self::Ipv4EvenPort),
            ("::", 1) => Ok(Self::Ipv6AnyPort),
            ("::", 2) => Ok(Self::Ipv6EvenPort),
            _ => Err(Error::OnionRouteError(OnionRouteError::UdpTarget(format!(
                "unsupported udp bind target {}",
                target.authority()
            )))),
        }
    }

    /// Return whether this bind mode requests an IPv6 socket.
    pub const fn is_ipv6(self) -> bool {
        matches!(self, Self::Ipv6AnyPort | Self::Ipv6EvenPort)
    }

    /// Return whether this bind mode requires an even UDP local port.
    pub const fn requires_even_port(self) -> bool {
        matches!(self, Self::Ipv4EvenPort | Self::Ipv6EvenPort)
    }
}

/// Socket binding and peer-IP admission settings for native `onion::udp` exits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnionUdpBindConfig {
    /// Local IPv4 interface bound by IPv4 `onion::udp` sessions (`0.0.0.0` by default).
    pub bind_ipv4: Ipv4Addr,
    /// Local IPv6 interface bound by IPv6 `onion::udp` sessions (`::` by default).
    pub bind_ipv6: Ipv6Addr,
    /// Externally reachable IPv4 reported in [`OnionUdpServerFrame::Bound`] when `bind_ipv4` is
    /// unspecified (`0.0.0.0`).
    pub advertised_ipv4: Option<Ipv4Addr>,
    /// Externally reachable IPv6 reported in [`OnionUdpServerFrame::Bound`] when `bind_ipv6` is
    /// unspecified (`::`).
    pub advertised_ipv6: Option<Ipv6Addr>,
    /// Optional inclusive UDP port range `(min_port, max_port)` for bound exit sockets.
    pub port_range: Option<(u16, u16)>,
    /// Admit loopback/RFC1918 peer and advertised IPs for controlled local/integration tests.
    pub allow_private_peers: bool,
}

impl Default for OnionUdpBindConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl OnionUdpBindConfig {
    /// Default production configuration: wildcard bind, public-only peer IPs, and advertised IP
    /// populated from the node's endpoint hint or explicit override.
    pub const fn new() -> Self {
        Self {
            bind_ipv4: Ipv4Addr::UNSPECIFIED,
            bind_ipv6: Ipv6Addr::UNSPECIFIED,
            advertised_ipv4: None,
            advertised_ipv6: None,
            port_range: None,
            allow_private_peers: false,
        }
    }

    /// Local test configuration binding and advertising loopback (`127.0.0.1` / `::1`) with
    /// private/loopback peers enabled.
    pub const fn loopback_for_test() -> Self {
        Self {
            bind_ipv4: Ipv4Addr::LOCALHOST,
            bind_ipv6: Ipv6Addr::LOCALHOST,
            advertised_ipv4: Some(Ipv4Addr::LOCALHOST),
            advertised_ipv6: Some(Ipv6Addr::LOCALHOST),
            port_range: None,
            allow_private_peers: true,
        }
    }

    /// Populate unset `advertised_ipv4` / `advertised_ipv6` from an endpoint-hint IP literal if
    /// present.
    pub(crate) fn with_endpoint_hint_fallback(mut self, endpoint_hint: Option<&str>) -> Self {
        let Some(hint_ip) = endpoint_hint.and_then(parse_ip_from_endpoint_hint) else {
            return self;
        };
        if hint_ip.is_unspecified() {
            return self;
        }
        match hint_ip {
            IpAddr::V4(ip) if self.advertised_ipv4.is_none() => {
                self.advertised_ipv4 = Some(ip);
            }
            IpAddr::V6(ip) if self.advertised_ipv6.is_none() => {
                self.advertised_ipv6 = Some(ip);
            }
            _ => {}
        }
        self
    }

    /// Return whether `ip` is permitted as a peer or advertised IP under this configuration.
    pub fn allows_peer_ip(&self, ip: IpAddr) -> bool {
        !ip.is_unspecified() && (self.allow_private_peers || rings_network_policy::is_public_ip(ip))
    }

    /// Resolve the externally reachable IP address to report for a bound socket of `ipv6` family.
    pub fn resolve_advertised_ip(&self, ipv6: bool) -> Result<IpAddr> {
        let candidate = if ipv6 {
            self.advertised_ipv6
                .or_else(|| (!self.bind_ipv6.is_unspecified()).then_some(self.bind_ipv6))
                .map(IpAddr::V6)
        } else {
            self.advertised_ipv4
                .or_else(|| (!self.bind_ipv4.is_unspecified()).then_some(self.bind_ipv4))
                .map(IpAddr::V4)
        };
        let Some(ip) = candidate else {
            return Err(Error::OnionRouteError(OnionRouteError::UdpTarget(
                "onion udp exit has no advertisable external IP address".to_string(),
            )));
        };
        if !self.allows_peer_ip(ip) {
            return Err(Error::NoPermission);
        }
        Ok(ip)
    }
}

/// Extract an IP address literal from a raw IP, socket address, or URL endpoint hint.
fn parse_ip_from_endpoint_hint(raw: &str) -> Option<IpAddr> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(ip) = trimmed.trim_matches(['[', ']']).parse::<IpAddr>() {
        return Some(ip);
    }
    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return Some(addr.ip());
    }
    let parsed = Url::parse(trimmed).ok()?;
    let host = parsed.host_str()?.trim_matches(['[', ']']);
    host.parse::<IpAddr>().ok()
}

/// Maximum outbound UDP datagrams one `onion::udp` exit session may transmit per 1-second clock
/// window (`2,048` pps), preventing small-frame cell packing from turning an exit into a packet-rate
/// amplifier while leaving headroom for bidirectional real-time media bursts.
pub const MAX_OUTBOUND_DATAGRAMS_PER_SEC: u32 = 2_048;

/// Maximum time allowed for binding a UDP exit socket during session open.
#[cfg(rings_native)]
const UDP_OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// Timing quantum in milliseconds to which UDP open responses are rounded.
#[cfg(rings_native)]
const UDP_OPEN_RESPONSE_QUANTUM_MS: u128 = 250;

/// Maximum ephemeral bind attempts when searching for an even UDP port.
#[cfg(rings_native)]
const MAX_EVEN_PORT_EPHEMERAL_TRIES: usize = 64;

/// Receive buffer size in bytes for inbound OS UDP datagrams.
#[cfg(rings_native)]
const MAX_UDP_RECV_DATAGRAM_BYTES: usize = MAX_ONION_UDP_PAYLOAD_BYTES;

/// Maximum buffered reassembly bytes permitted on the exit write half.
#[cfg(rings_native)]
const MAX_WRITER_REASSEMBLY_BYTES: usize =
    (MAX_ONION_UDP_FRAME_BYTES + ONION_UDP_FRAME_LEN_BYTES) * 4;

/// Maximum buffered reassembly bytes permitted on the client read half.
#[cfg(rings_native)]
const MAX_READER_REASSEMBLY_BYTES: usize =
    (MAX_ONION_UDP_FRAME_BYTES + ONION_UDP_FRAME_LEN_BYTES) * 4;

/// Wrap `reason` in [`Error::OnionRouteError`] with [`OnionRouteError::UdpTarget`].
#[cfg(rings_native)]
fn udp_target(reason: String) -> Error {
    Error::OnionRouteError(OnionRouteError::UdpTarget(reason))
}

/// Compute the next quantized release deadline after `opened_at` for an open response at `now`.
#[cfg(rings_native)]
fn open_response_deadline(opened_at: Instant, now: Instant) -> Instant {
    let elapsed_ms = now.saturating_duration_since(opened_at).as_millis();
    let quanta = elapsed_ms.div_ceil(UDP_OPEN_RESPONSE_QUANTUM_MS).max(1);
    let deadline_ms =
        u64::try_from(quanta.saturating_mul(UDP_OPEN_RESPONSE_QUANTUM_MS)).unwrap_or(u64::MAX);
    opened_at
        .checked_add(Duration::from_millis(deadline_ms))
        .unwrap_or(now)
}

/// Convert a peer `SocketAddr` into an [`OnionProxyTarget`] for exit-policy matching.
#[cfg(rings_native)]
fn socket_addr_target(addr: SocketAddr) -> Option<OnionProxyTarget> {
    let host = addr.ip().to_string();
    OnionProxyTarget::new(&host, addr.port()).ok()
}

/// Bind a local OS [`UdpSocket`] honoring `mode`'s address family, even-port constraint, and `config`.
#[cfg(rings_native)]
async fn bind_udp_socket(mode: OnionUdpBindMode, config: &OnionUdpBindConfig) -> Result<UdpSocket> {
    let bind_ip: IpAddr = if mode.is_ipv6() {
        IpAddr::V6(config.bind_ipv6)
    } else {
        IpAddr::V4(config.bind_ipv4)
    };
    let even_only = mode.requires_even_port();
    if let Some((min_port, max_port)) = config.port_range {
        if min_port == 0 || min_port > max_port {
            return Err(udp_target(format!(
                "invalid udp port range {min_port}..={max_port}"
            )));
        }
        let mut last_err = None;
        for port in min_port..=max_port {
            if even_only && port % 2 != 0 {
                continue;
            }
            match UdpSocket::bind(SocketAddr::new(bind_ip, port)).await {
                Ok(sock) => return Ok(sock),
                Err(err) => last_err = Some(err),
            }
        }
        return Err(udp_target(format!(
            "no free udp port in {min_port}..={max_port}: {last_err:?}"
        )));
    }

    if !even_only {
        return UdpSocket::bind(SocketAddr::new(bind_ip, 0))
            .await
            .map_err(|err| udp_target(format!("bind {bind_ip}:0 failed: {err}")));
    }

    let mut held_odd_sockets = Vec::with_capacity(MAX_EVEN_PORT_EPHEMERAL_TRIES);
    for _ in 0..MAX_EVEN_PORT_EPHEMERAL_TRIES {
        let sock = UdpSocket::bind(SocketAddr::new(bind_ip, 0))
            .await
            .map_err(|err| udp_target(format!("bind {bind_ip}:0 failed: {err}")))?;
        let local = sock
            .local_addr()
            .map_err(|err| udp_target(format!("local_addr failed: {err}")))?;
        if local.port() != 0 && local.port() % 2 == 0 {
            return Ok(sock);
        }
        held_odd_sockets.push(sock);
    }
    Err(udp_target(
        "failed to bind an even ephemeral UDP port".to_string(),
    ))
}

/// Native OS socket world of an `onion::udp` exit.
#[cfg(rings_native)]
pub(crate) struct OnionUdpWorld {
    /// Exit policy enforced on bind modes and per-datagram peer addresses.
    policy: OnionExitPolicy,
    /// Byte accounting shared across exit sessions.
    accounting: OnionExitAccounting,
    /// Bind address, port range, and advertised IP configuration.
    config: OnionUdpBindConfig,
    /// Injected clock for pinhole expiry and outbound rate limiting.
    clock: SharedClock,
}

#[cfg(rings_native)]
impl OnionUdpWorld {
    /// Construct an `onion::udp` exit world under `policy`, `accounting`, `config`, and `clock`.
    pub(crate) fn new(
        policy: OnionExitPolicy,
        accounting: OnionExitAccounting,
        config: OnionUdpBindConfig,
        clock: SharedClock,
    ) -> Self {
        Self {
            policy,
            accounting,
            config,
            clock,
        }
    }

    /// Bind an OS UDP socket for `target`'s bind mode and construct the paired reader and writer halves.
    async fn bind_session(
        &self,
        target: &OnionProxyTarget,
    ) -> Result<(OnionUdpReader, OnionUdpWriter)> {
        let mode = OnionUdpBindMode::from_target(target)?;
        if !self.policy.allows_udp_bind(mode) {
            return Err(Error::NoPermission);
        }
        let advertised_ip = self.config.resolve_advertised_ip(mode.is_ipv6())?;
        let socket = bind_udp_socket(mode, &self.config).await?;
        let os_local = socket
            .local_addr()
            .map_err(|err| udp_target(format!("local_addr: {err}")))?;
        if os_local.port() == 0 {
            return Err(udp_target("bound UDP socket reported port 0".to_string()));
        }
        let local_addr = SocketAddr::new(advertised_ip, os_local.port());
        let bound_bytes = Bytes::from(encode_server_frame(&OnionUdpServerFrame::Bound {
            local_addr,
        })?);
        let shared_socket = Arc::new(socket);
        let pinholes = Arc::new(SyncMutex::new(OnionUdpPinholeTable::new()));
        Ok((
            OnionUdpReader {
                socket: Arc::clone(&shared_socket),
                local_addr,
                policy: self.policy.clone(),
                accounting: self.accounting.clone(),
                config: self.config,
                clock: self.clock.clone(),
                pinholes: Arc::clone(&pinholes),
                pending: bound_bytes,
                recv_buf: vec![0_u8; MAX_UDP_RECV_DATAGRAM_BYTES],
            },
            OnionUdpWriter {
                socket: shared_socket,
                local_addr,
                policy: self.policy.clone(),
                config: self.config,
                clock: self.clock.clone(),
                pinholes,
                buffer: BytesMut::new(),
                rate_window: OutboundDatagramWindow::default(),
            },
        ))
    }
}

/// Per-second outbound datagram counter enforcing [`MAX_OUTBOUND_DATAGRAMS_PER_SEC`].
#[cfg(rings_native)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OutboundDatagramWindow {
    /// Active 1-second clock bucket (`now_ms / 1_000`).
    second: u128,
    /// Number of outbound datagrams admitted during `second`.
    count: u32,
}

#[cfg(rings_native)]
impl OutboundDatagramWindow {
    /// Admit one outbound datagram at `now_ms`, returning `false` if the per-second cap is reached.
    fn try_admit(&mut self, now_ms: u128) -> bool {
        let sec = now_ms / 1_000;
        if self.second != sec {
            self.second = sec;
            self.count = 0;
        }
        if self.count >= MAX_OUTBOUND_DATAGRAMS_PER_SEC {
            return false;
        }
        self.count = self.count.saturating_add(1);
        true
    }
}

/// Read half of an `onion::udp` exit session, emitting [`OnionUdpServerFrame::Bound`] followed by
/// pinhole-admitted [`OnionUdpServerFrame::RecvDatagram`] frames.
#[cfg(rings_native)]
pub(crate) struct OnionUdpReader {
    /// Shared bound OS UDP socket.
    socket: Arc<UdpSocket>,
    /// Externally advertised local socket address.
    local_addr: SocketAddr,
    /// Exit policy enforced on inbound peer addresses.
    policy: OnionExitPolicy,
    /// Shared exit byte accounting.
    accounting: OnionExitAccounting,
    /// Bind and peer-IP admission configuration.
    config: OnionUdpBindConfig,
    /// Injected clock for pinhole expiry checks.
    clock: SharedClock,
    /// Shared inbound peer-IP pinhole table.
    pinholes: Arc<SyncMutex<OnionUdpPinholeTable>>,
    /// Encoded frame bytes waiting to be drained by `read`.
    pending: Bytes,
    /// Scratch buffer for receiving OS UDP datagrams.
    recv_buf: Vec<u8>,
}

/// Write half of an `onion::udp` exit session, applying [`OnionUdpClientFrame::AllowPeerIp`]
/// pinholes and sending [`OnionUdpClientFrame::SendDatagram`] payloads.
#[cfg(rings_native)]
pub(crate) struct OnionUdpWriter {
    /// Shared bound OS UDP socket.
    socket: Arc<UdpSocket>,
    /// Externally advertised local socket address.
    local_addr: SocketAddr,
    /// Exit policy enforced on outbound peer addresses.
    policy: OnionExitPolicy,
    /// Bind and peer-IP admission configuration.
    config: OnionUdpBindConfig,
    /// Injected clock for pinhole expiry and rate limiting.
    clock: SharedClock,
    /// Shared inbound peer-IP pinhole table.
    pinholes: Arc<SyncMutex<OnionUdpPinholeTable>>,
    /// Reassembly buffer for length-prefixed client frames.
    buffer: BytesMut,
    /// Per-second outbound datagram rate limiter.
    rate_window: OutboundDatagramWindow,
}

#[cfg(rings_native)]
#[async_trait::async_trait]
impl OnionWorld for OnionUdpWorld {
    type Reader = OnionUdpReader;
    type Writer = OnionUdpWriter;

    fn admits_open(&self, policy: &OnionExitPolicy, target: &OnionProxyTarget) -> bool {
        OnionUdpBindMode::from_target(target).is_ok_and(|mode| policy.allows_udp_bind(mode))
    }

    async fn open(&self, target: &OnionProxyTarget) -> Result<(Self::Reader, Self::Writer)> {
        let opened_at = Instant::now();
        let outcome = match timeout(UDP_OPEN_TIMEOUT, self.bind_session(target)).await {
            Ok(res) => res,
            Err(_) => Err(Error::OnionRouteError(OnionRouteError::UdpOpenTimedOut)),
        };
        tokio::time::sleep_until(open_response_deadline(opened_at, Instant::now())).await;
        outcome
    }
}

#[cfg(rings_native)]
#[async_trait::async_trait]
impl OnionWorldReader for OnionUdpReader {
    async fn read(&mut self, max: usize) -> Result<Option<Bytes>> {
        let max = max.max(1);
        loop {
            if !self.pending.is_empty() {
                let take = max.min(self.pending.len());
                return Ok(Some(self.pending.split_to(take)));
            }
            let (len, peer) = self
                .socket
                .recv_from(&mut self.recv_buf)
                .await
                .map_err(|err| udp_target(format!("recv_from: {err}")))?;
            if peer.port() == 0
                || peer.is_ipv6() != self.local_addr.is_ipv6()
                || !self.config.allows_peer_ip(peer.ip())
            {
                continue;
            }
            let now_ms = self.clock.now_ms();
            if !lock(&self.pinholes)?.allows(peer.ip(), now_ms) {
                continue;
            }
            let Some(peer_target) = socket_addr_target(peer) else {
                continue;
            };
            if !self.policy.allows_target(&peer_target) {
                continue;
            }
            let Some(payload) = self.recv_buf.get(..len).map(<[u8]>::to_vec) else {
                continue;
            };
            let frame = OnionUdpServerFrame::RecvDatagram { peer, payload };
            let encoded = encode_server_frame(&frame)?;
            let byte_count = u64::try_from(encoded.len()).map_err(|_| Error::InvalidData)?;
            self.accounting
                .record_bytes(&self.policy, byte_count, self.accounting.now_ms())?;
            self.pending = Bytes::from(encoded);
        }
    }
}

#[cfg(rings_native)]
#[async_trait::async_trait]
impl OnionWorldWriter for OnionUdpWriter {
    async fn write(&mut self, bytes: Bytes) -> Result<()> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_WRITER_REASSEMBLY_BYTES {
            return Err(Error::InvalidData);
        }
        self.buffer.extend_from_slice(&bytes);
        while let Some(frame) = decode_next_client_frame(&mut self.buffer)? {
            match frame {
                OnionUdpClientFrame::AllowPeerIp { peer_ip, ttl_ms } => {
                    if peer_ip.is_ipv6() != self.local_addr.is_ipv6()
                        || !self.config.allows_peer_ip(peer_ip)
                    {
                        tracing::debug!(
                            %peer_ip,
                            "onion udp exit ignored pinhole for disallowed peer IP"
                        );
                        continue;
                    }
                    let now_ms = self.clock.now_ms();
                    let _ = lock(&self.pinholes)?.allow_ip(peer_ip, ttl_ms, now_ms);
                }
                OnionUdpClientFrame::SendDatagram { peer, payload } => {
                    if peer.port() == 0
                        || peer.is_ipv6() != self.local_addr.is_ipv6()
                        || !self.config.allows_peer_ip(peer.ip())
                    {
                        tracing::debug!(
                            %peer,
                            "onion udp exit dropped outbound datagram to disallowed peer"
                        );
                        continue;
                    }
                    let Some(peer_target) = socket_addr_target(peer) else {
                        continue;
                    };
                    if !self.policy.allows_target(&peer_target) {
                        tracing::debug!(
                            %peer,
                            "onion udp exit policy denied outbound datagram target"
                        );
                        continue;
                    }
                    if !self.rate_window.try_admit(self.clock.now_ms()) {
                        tracing::debug!(
                            %peer,
                            max_pps = MAX_OUTBOUND_DATAGRAMS_PER_SEC,
                            "onion udp exit dropped outbound datagram exceeding per-second rate cap"
                        );
                        continue;
                    }
                    self.socket
                        .send_to(&payload, peer)
                        .await
                        .map_err(|err| udp_target(format!("send_to {peer}: {err}")))?;
                }
            }
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Owned receive half and frame reassembly buffer for a [`NativeOnionUdpSession`].
#[cfg(rings_native)]
struct UdpSessionReaderState {
    /// Underlying onion stream receiver.
    receiver: OnionStreamReceiver,
    /// Reassembly buffer for length-prefixed server frames.
    buffer: BytesMut,
}

/// Cancel-safe RAII lease over a synchronous `Option<T>` slot.
///
/// If the owning async operation (`recv_from` or `send_framed`) returns a live outcome or is
/// cancelled while awaiting an event or queue slot, `Drop` restores `T` into `slot` unless
/// [`NativeOnionUdpSession::close`] has set `closed`. Terminal stream states (`Fin`, `Failed`, or
/// framing error) call [`Self::retire`] so dead stream halves are dropped immediately.
#[cfg(rings_native)]
struct SlotLease<'a, T> {
    /// Backing mutex-guarded slot to restore `value` into on drop.
    slot: &'a SyncMutex<Option<T>>,
    /// Session closure flag checked before restoring `value`.
    closed: &'a AtomicBool,
    /// Leased value, or `None` once retired or restored.
    value: Option<T>,
}

#[cfg(rings_native)]
impl<'a, T> SlotLease<'a, T> {
    /// Take the value out of `slot` unless `closed` is set or the slot is already empty.
    fn acquire(slot: &'a SyncMutex<Option<T>>, closed: &'a AtomicBool) -> Result<Option<Self>> {
        if closed.load(Ordering::SeqCst) {
            if let Ok(mut guard) = lock(slot) {
                let _ = guard.take();
            }
            return Ok(None);
        }
        let Some(val) = lock(slot)?.take() else {
            return Ok(None);
        };
        Ok(Some(Self {
            slot,
            closed,
            value: Some(val),
        }))
    }

    /// Drop the leased value permanently without restoring it to `slot`.
    fn retire(&mut self) {
        let _ = self.value.take();
    }
}

#[cfg(rings_native)]
impl<T> Drop for SlotLease<'_, T> {
    fn drop(&mut self) {
        let Some(val) = self.value.take() else {
            return;
        };
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(mut guard) = lock(self.slot) {
            *guard = Some(val);
        }
        if self.closed.load(Ordering::SeqCst) {
            if let Ok(mut guard) = lock(self.slot) {
                let _ = guard.take();
            }
        }
    }
}

/// Client handle to an opened `onion::udp` datagram session whose exit has bound an OS UDP socket.
#[cfg(rings_native)]
pub struct NativeOnionUdpSession {
    /// Externally reachable socket address reported by the exit's `Bound` frame.
    local_addr: SocketAddr,
    /// Whether this session has been closed locally.
    closed: AtomicBool,
    /// Notifier woken on [`Self::close`] to interrupt parked readers and writers.
    closed_notify: Notify,
    /// Async mutex serializing outbound frame sends.
    send_gate: AsyncMutex<()>,
    /// Slot holding the stream sender when idle.
    sender_slot: SyncMutex<Option<OnionStreamSender>>,
    /// Async mutex serializing inbound frame receives.
    recv_gate: AsyncMutex<()>,
    /// Slot holding the stream receiver and reassembly buffer when idle.
    receiver_slot: SyncMutex<Option<UdpSessionReaderState>>,
}

#[cfg(rings_native)]
impl NativeOnionUdpSession {
    /// Complete the `onion::udp` open handshake by awaiting the exit's initial
    /// [`OnionUdpServerFrame::Bound`] frame.
    pub(crate) async fn from_stream(stream: OnionClientStream) -> Result<Self> {
        let (sender, mut receiver) = stream.split();
        let mut buffer = BytesMut::new();
        let bound_addr = timeout(ONION_SESSION_OPEN_TIMEOUT, async {
            loop {
                if let Some(frame) = decode_next_server_frame(&mut buffer)? {
                    return match frame {
                        OnionUdpServerFrame::Bound { local_addr }
                            if !local_addr.ip().is_unspecified() && local_addr.port() != 0 =>
                        {
                            Ok(local_addr)
                        }
                        OnionUdpServerFrame::Bound { local_addr } => Err(udp_target(format!(
                            "exit reported invalid bound address {local_addr}"
                        ))),
                        OnionUdpServerFrame::RecvDatagram { .. } => Err(udp_target(
                            "exit sent datagram before initial Bound frame".to_string(),
                        )),
                    };
                }
                match receiver.next().await {
                    Some(OnionStreamEvent::Data(chunk)) => {
                        if buffer.len().saturating_add(chunk.len()) > MAX_READER_REASSEMBLY_BYTES {
                            return Err(Error::InvalidData);
                        }
                        buffer.extend_from_slice(&chunk);
                    }
                    Some(OnionStreamEvent::Fin | OnionStreamEvent::Failed) | None => {
                        return Err(Error::OnionRouteError(OnionRouteError::UdpSessionClosed));
                    }
                }
            }
        })
        .await
        .map_err(|_| Error::OnionRouteError(OnionRouteError::UdpOpenTimedOut))??;

        Ok(Self {
            local_addr: bound_addr,
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
            send_gate: AsyncMutex::new(()),
            sender_slot: SyncMutex::new(Some(sender)),
            recv_gate: AsyncMutex::new(()),
            receiver_slot: SyncMutex::new(Some(UdpSessionReaderState { receiver, buffer })),
        })
    }

    /// Externally reachable UDP socket address bound by the exit for this session.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Open or refresh an inbound pinhole for `peer_ip` for `ttl` at the exit socket.
    pub async fn allow_peer_ip(&self, peer_ip: IpAddr, ttl: Duration) -> Result<()> {
        let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        let framed = encode_client_frame(&OnionUdpClientFrame::AllowPeerIp { peer_ip, ttl_ms })?;
        self.send_framed(Bytes::from(framed)).await
    }

    /// Transmit one UDP datagram from the exit's bound socket to `peer`.
    pub async fn send_to(&self, payload: &[u8], peer: SocketAddr) -> Result<usize> {
        let framed = encode_client_frame(&OnionUdpClientFrame::SendDatagram {
            peer,
            payload: payload.to_vec(),
        })?;
        self.send_framed(Bytes::from(framed)).await?;
        Ok(payload.len())
    }

    /// Receive the next inbound UDP datagram `(payload, peer)` from the exit's bound socket, or
    /// `Ok(None)` when the session closes cleanly or via [`Self::close`].
    ///
    /// Cancel-safe: dropping a pending `recv_from` future (for example inside a losing
    /// `tokio::select!` branch) preserves both the underlying stream receiver and any partially
    /// buffered frame bytes for the next call.
    pub async fn recv_from(&self) -> Result<Option<(Bytes, SocketAddr)>> {
        let _guard = self.recv_gate.lock().await;
        let Some(mut lease) = SlotLease::acquire(&self.receiver_slot, &self.closed)? else {
            return Ok(None);
        };
        loop {
            let Some(state) = lease.value.as_mut() else {
                return Ok(None);
            };
            match decode_next_server_frame(&mut state.buffer) {
                Ok(Some(OnionUdpServerFrame::RecvDatagram { peer, payload })) => {
                    return Ok(Some((Bytes::from(payload), peer)));
                }
                Ok(Some(OnionUdpServerFrame::Bound { .. })) => {
                    lease.retire();
                    return Err(Error::InvalidData);
                }
                Ok(None) => {}
                Err(err) => {
                    lease.retire();
                    return Err(err);
                }
            }
            let notified = self.closed_notify.notified();
            tokio::pin!(notified);
            if self.closed.load(Ordering::SeqCst) {
                lease.retire();
                return Ok(None);
            }
            let event = tokio::select! {
                () = &mut notified => None,
                ev = state.receiver.next() => ev,
            };
            match event {
                Some(OnionStreamEvent::Data(chunk)) => {
                    if state.buffer.len().saturating_add(chunk.len()) > MAX_READER_REASSEMBLY_BYTES
                    {
                        lease.retire();
                        return Err(Error::InvalidData);
                    }
                    state.buffer.extend_from_slice(&chunk);
                }
                Some(OnionStreamEvent::Fin) | None => {
                    lease.retire();
                    return Ok(None);
                }
                Some(OnionStreamEvent::Failed) => {
                    lease.retire();
                    return Err(Error::OnionRouteError(OnionRouteError::UdpSessionClosed));
                }
            }
        }
    }

    /// Synchronously signal closure, drop idle stream halves immediately, and wake any parked
    /// [`Self::recv_from`] or [`Self::send_to`] without blocking on reader/writer locks.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.drop_sender_slot();
        self.drop_receiver_slot();
        self.closed_notify.notify_waiters();
    }

    /// Send one length-prefixed client frame over the onion stream, aborting if the session closes.
    async fn send_framed(&self, framed: Bytes) -> Result<()> {
        let _guard = self.send_gate.lock().await;
        let Some(mut lease) = SlotLease::acquire(&self.sender_slot, &self.closed)? else {
            return Err(Error::OnionRouteError(OnionRouteError::UdpSessionClosed));
        };
        let Some(sender) = lease.value.as_mut() else {
            return Err(Error::OnionRouteError(OnionRouteError::UdpSessionClosed));
        };
        let notified = self.closed_notify.notified();
        tokio::pin!(notified);
        if self.closed.load(Ordering::SeqCst) {
            lease.retire();
            return Err(Error::OnionRouteError(OnionRouteError::UdpSessionClosed));
        }
        let ready = tokio::select! {
            () = &mut notified => Err(Error::OnionRouteError(OnionRouteError::UdpSessionClosed)),
            res = sender.ready() => res.map_err(|_| Error::OnionRouteError(OnionRouteError::UdpSessionClosed)),
        };
        if let Err(err) = ready {
            lease.retire();
            return Err(err);
        }
        if sender.start_send(framed).is_err() {
            lease.retire();
            return Err(Error::OnionRouteError(OnionRouteError::UdpSessionClosed));
        }
        Ok(())
    }

    /// Take and drop the idle stream sender if currently present in `sender_slot`.
    fn drop_sender_slot(&self) {
        if let Ok(mut slot) = lock(&self.sender_slot) {
            let _ = slot.take();
        }
    }

    /// Take and drop the idle stream receiver state if currently present in `receiver_slot`.
    fn drop_receiver_slot(&self) {
        if let Ok(mut slot) = lock(&self.receiver_slot) {
            let _ = slot.take();
        }
    }
}

#[cfg(test)]
mod tests;
