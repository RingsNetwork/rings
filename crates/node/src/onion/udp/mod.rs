//! `udp`: an unconnected multi-peer OS UDP datagram socket over an onion session, native only
//! (#1292).
//!
//! ```text
//! client ──AllowPeer / Datagram──▶ session ═loops═▶ h ──send_to──▶ E_exit (OS UdpSocket)
//! client ◀─Bound / PeerAllowed / Datagram─ session ◀replies═ h ◀─recv_from─ E_exit (pinhole + public-IP gated)
//! ```
//!
//! Laws:
//!
//! - **World purity.** `onion::udp` is a generic OS UDP socket world: it knows nothing about
//!   WebRTC, ICE, STUN, or RFC 5766 TURN.
//! - **Bound address acknowledgement.** Immediately upon opening `E_exit`, `OnionUdpReader` queues
//!   `Bound { local_addr: E_exit }` as the first bytes of the reply stream so [`OnionUdpSession`]
//!   knows `E_exit` before returning to its caller.
//! - **Public-IP egress & stateful IP pinhole.** Production exits (`OnionUdpPeerPolicy::PublicOnly`)
//!   admit only globally routable unicast peers (`rings_network_policy::is_public_ip`) that also
//!   satisfy the exit's [`OnionExitPolicy`]. Inbound packets arriving on `E_exit` from peers
//!   without an active stateful IP pinhole (installed by `AllowPeer` or an outbound `Datagram`)
//!   are dropped at the OS socket reader before returning from `OnionWorldReader::read`, so
//!   unsolicited internet scan traffic consumes neither SURB reply blocks nor exit byte quota.
//! - **Non-blocking teardown.** [`OnionUdpSession::begin_close`] synchronously wakes any blocked
//!   [`OnionUdpSession::recv_from`], [`OnionUdpSession::allow_peer_ip`], and internal demux task
//!   without waiting on a dead upstream stream reader.

pub mod codec;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::PoisonError;
use std::time::Duration;

use bytes::Bytes;
use bytes::BytesMut;
use futures::channel::oneshot;
use rand::Rng;
use rings_core::utils::SharedClock;
use rings_core::utils::SystemClock;
use serde::Deserialize;
use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;
use tokio::time::Instant;

pub use self::codec::OnionUdpClientFrame;
pub use self::codec::OnionUdpExitFrame;
use self::codec::OnionUdpFrameDecoder;
pub use self::codec::MAX_UDP_DATAGRAM_BYTES;
use crate::error::Error;
use crate::error::Result;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::session::dial::OnionClientStream;
use crate::onion::session::dial::OnionStreamEvent;
use crate::onion::session::dial::OnionStreamReceiver;
use crate::onion::session::dial::OnionStreamSender;
use crate::onion::session::dial::ONION_SESSION_OPEN_TIMEOUT;
use crate::onion::session::serve::OnionWorld;
use crate::onion::session::serve::OnionWorldReader;
use crate::onion::session::serve::OnionWorldWriter;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRouteError;

/// Default lifetime (`10 minutes`) of a stateful IP pinhole at an `onion::udp` exit socket.
pub const DEFAULT_UDP_PINHOLE_TTL_MS: u128 = 600_000;

/// Default maximum concurrent peer IP pinholes tracked per `onion::udp` session.
pub const DEFAULT_MAX_UDP_PINHOLES: usize = 256;

/// Maximum attempts to bind a free or even-numbered UDP port in a configured port range.
const UDP_BIND_MAX_RETRIES: u16 = 32;

/// Open response timing quantum (`250 ms`), matching `OnionTcpWorld`.
const UDP_OPEN_RESPONSE_QUANTUM_MS: u128 = 250;

/// Bounded inbound datagram queue between `OnionUdpSession`'s demux reader and `recv_from`,
/// preserving SURB credit backpressure when the caller stops reading.
const SESSION_DATAGRAM_QUEUE_CAPACITY: usize = 64;

/// Maximum unconsumed `PeerAllowed` control acknowledgements buffered at an `onion::udp` exit
/// (`DEFAULT_MAX_UDP_PINHOLES`). Exceeding this cap terminates the session so an abusive client
/// packing repeated `AllowPeer` records without draining return SURBs cannot grow exit memory.
const MAX_PENDING_CONTROL_ACKS: usize = DEFAULT_MAX_UDP_PINHOLES;

fn open_response_deadline(opened_at: Instant, now: Instant) -> Instant {
    let elapsed_ms = now.saturating_duration_since(opened_at).as_millis();
    let quanta = elapsed_ms.div_ceil(UDP_OPEN_RESPONSE_QUANTUM_MS).max(1);
    let deadline_ms =
        u64::try_from(quanta.saturating_mul(UDP_OPEN_RESPONSE_QUANTUM_MS)).unwrap_or(u64::MAX);
    opened_at
        .checked_add(Duration::from_millis(deadline_ms))
        .unwrap_or(now)
}

fn udp_target(reason: impl Into<String>) -> Error {
    Error::OnionRouteError(OnionRouteError::UdpTarget(reason.into()))
}

fn udp_closed() -> Error {
    Error::OnionRouteError(OnionRouteError::UdpSessionClosed)
}

/// IP address family requested when opening an `onion::udp` exit socket.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OnionUdpFamily {
    /// Bind an IPv4 UDP socket.
    Ipv4,
    /// Bind an IPv6 UDP socket.
    Ipv6,
}

/// Parameters encoded into the canonical session target authority when opening an `onion::udp`
/// session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OnionUdpBindRequest {
    /// Requested socket address family.
    pub family: OnionUdpFamily,
    /// Whether the bound local UDP port must be even (`port % 2 == 0`).
    pub even_port: bool,
}

impl OnionUdpBindRequest {
    /// Canonical target authority (`0.0.0.0:1`, `0.0.0.0:2`, `[::]:1`, or `[::]:2`) passed through
    /// `OnionSessionRequest::target`.
    ///
    /// Using an unspecified host literal ensures an exit operator whose `OnionExitPolicy` is
    /// restricted to concrete TCP/HTTPS hosts or ports (such as `*:443` or `example.com:443`)
    /// automatically refuses multi-peer `udp` socket opens, whereas `*:*` admits them.
    pub fn to_proxy_target(self) -> Result<OnionProxyTarget> {
        let port = if self.even_port { 2 } else { 1 };
        let host = match self.family {
            OnionUdpFamily::Ipv4 => "0.0.0.0",
            OnionUdpFamily::Ipv6 => "::",
        };
        OnionProxyTarget::new(host, port)
    }

    /// Decode a canonical `onion::udp` bind target authority, rejecting any non-canonical host or
    /// port.
    pub fn from_proxy_target(target: &OnionProxyTarget) -> Result<Self> {
        let even_port = match target.port() {
            1 => false,
            2 => true,
            _ => {
                return Err(udp_target(format!(
                    "unsupported udp bind target port in {}",
                    target.authority()
                )));
            }
        };
        let family = match target.host() {
            "0.0.0.0" => OnionUdpFamily::Ipv4,
            "::" => OnionUdpFamily::Ipv6,
            _ => {
                return Err(udp_target(format!(
                    "unsupported udp bind target host in {}",
                    target.authority()
                )));
            }
        };
        Ok(Self { family, even_port })
    }
}

/// Destination IP egress/ingress policy enforced by an `onion::udp` exit socket.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OnionUdpPeerPolicy {
    /// Admit only globally routable public unicast IP addresses (`rings_network_policy::is_public_ip`)
    /// and non-zero ports. This is the sole mode reachable from YAML configuration.
    #[default]
    PublicOnly,
    /// Admit loopback, private, and link-local unicast addresses in addition to public unicast
    /// addresses (for in-process local integration tests). Still denies unspecified, multicast,
    /// broadcast/reserved Class E, and port `0`.
    AllowLoopbackAndPrivate,
}

impl OnionUdpPeerPolicy {
    /// Return whether `ip` is permitted as a UDP peer or advertised socket IP under this policy.
    pub fn allows_ip(self, ip: IpAddr) -> bool {
        match self {
            Self::PublicOnly => rings_network_policy::is_public_ip(ip),
            Self::AllowLoopbackAndPrivate => match ip {
                IpAddr::V4(v4) => {
                    let first = v4.octets()[0];
                    first != 0 && first < 224
                }
                IpAddr::V6(v6) => {
                    if v6.is_unspecified() || v6.is_multicast() {
                        return false;
                    }
                    if let Some(mapped) = v6.to_ipv4_mapped() {
                        let first = mapped.octets()[0];
                        return first != 0 && first < 224;
                    }
                    true
                }
            },
        }
    }

    /// Return whether `addr` is permitted as a UDP peer or bound socket endpoint under this policy.
    pub fn allows_addr(self, addr: SocketAddr) -> bool {
        addr.port() != 0 && self.allows_ip(addr.ip())
    }
}

/// Bounded stateful IP pinhole table for one `onion::udp` exit socket.
///
/// Exposed as a pure deterministic state machine so model tests can exercise the exact production
/// pinhole transitions alongside entry-side allocation state.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OnionUdpPinholes {
    entries: BTreeMap<IpAddr, u128>,
    ttl_ms: u128,
    max_entries: usize,
}

impl Default for OnionUdpPinholes {
    fn default() -> Self {
        Self::new(DEFAULT_UDP_PINHOLE_TTL_MS, DEFAULT_MAX_UDP_PINHOLES)
    }
}

impl OnionUdpPinholes {
    /// Create a bounded pinhole table with explicit `ttl_ms` and `max_entries`.
    pub fn new(ttl_ms: u128, max_entries: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            ttl_ms: ttl_ms.max(1),
            max_entries: max_entries.max(1),
        }
    }

    /// Prune pinholes that expired strictly before `now_ms`.
    pub fn prune_expired(&mut self, now_ms: u128) {
        self.entries.retain(|_, &mut exp| now_ms <= exp);
    }

    /// Install or refresh a stateful pinhole for `ip` valid through `now_ms + ttl_ms`.
    pub fn allow_ip(&mut self, ip: IpAddr, now_ms: u128) {
        self.prune_expired(now_ms);
        if !self.entries.contains_key(&ip) && self.entries.len() >= self.max_entries {
            if let Some(oldest_ip) = self
                .entries
                .iter()
                .min_by_key(|(_, &exp)| exp)
                .map(|(&k, _)| k)
            {
                self.entries.remove(&oldest_ip);
            }
        }
        self.entries.insert(ip, now_ms.saturating_add(self.ttl_ms));
    }

    /// Return whether `ip` currently holds an unexpired pinhole at `now_ms`.
    pub fn admits(&mut self, ip: IpAddr, now_ms: u128) -> bool {
        self.prune_expired(now_ms);
        self.entries.contains_key(&ip)
    }

    /// Read-only check (`&self`) returning whether `ip` holds an unexpired pinhole at `now_ms`.
    pub fn is_admitted_at(&self, ip: IpAddr, now_ms: u128) -> bool {
        self.entries.get(&ip).is_some_and(|&exp| now_ms <= exp)
    }

    /// Number of currently stored entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether no pinhole entries are stored.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Optional native YAML configuration (`onion_udp_exit:`) for the `udp` onion exit world.
///
/// Intentionally omits any `allow_private_peers` toggle so operator YAML files always enforce
/// [`OnionUdpPeerPolicy::PublicOnly`].
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnionUdpExitConfig {
    /// Local IPv4 address bound for IPv4 `udp` exit sockets (`0.0.0.0` when omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_ipv4: Option<Ipv4Addr>,
    /// Public IPv4 address reported in `Bound` when `bind_ipv4` is `0.0.0.0` (e.g. behind 1:1 NAT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_ipv4: Option<Ipv4Addr>,
    /// Local IPv6 address bound for IPv6 `udp` exit sockets (`::` when omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_ipv6: Option<Ipv6Addr>,
    /// Public IPv6 address reported in `Bound` when `bind_ipv6` is `::`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_ipv6: Option<Ipv6Addr>,
    /// Optional inclusive lower UDP port bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_port: Option<u16>,
    /// Optional inclusive upper UDP port bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_port: Option<u16>,
}

impl OnionUdpExitConfig {
    /// Validate and convert into a runtime [`OnionUdpConfig`] enforcing
    /// [`OnionUdpPeerPolicy::PublicOnly`].
    pub fn to_runtime_config(&self) -> Result<OnionUdpConfig> {
        if let Some(adv_v4) = self.advertised_ipv4 {
            if !rings_network_policy::is_public_ip(IpAddr::V4(adv_v4)) {
                return Err(Error::InvalidConfig(format!(
                    "onion_udp_exit.advertised_ipv4 ({adv_v4}) must be a publicly routable IPv4 address"
                )));
            }
        }
        if let Some(adv_v6) = self.advertised_ipv6 {
            if !rings_network_policy::is_public_ip(IpAddr::V6(adv_v6)) {
                return Err(Error::InvalidConfig(format!(
                    "onion_udp_exit.advertised_ipv6 ({adv_v6}) must be a publicly routable IPv6 address"
                )));
            }
        }
        if let Some(bind_v4) = self.bind_ipv4 {
            if !bind_v4.is_unspecified()
                && self.advertised_ipv4.is_none()
                && !rings_network_policy::is_public_ip(IpAddr::V4(bind_v4))
            {
                return Err(Error::InvalidConfig(format!(
                    "onion_udp_exit.bind_ipv4 ({bind_v4}) is non-public; set onion_udp_exit.advertised_ipv4 to your public IPv4 address"
                )));
            }
        }
        if let Some(bind_v6) = self.bind_ipv6 {
            if !bind_v6.is_unspecified()
                && self.advertised_ipv6.is_none()
                && !rings_network_policy::is_public_ip(IpAddr::V6(bind_v6))
            {
                return Err(Error::InvalidConfig(format!(
                    "onion_udp_exit.bind_ipv6 ({bind_v6}) is non-public; set onion_udp_exit.advertised_ipv6 to your public IPv6 address"
                )));
            }
        }
        let port_range = match (self.min_port, self.max_port) {
            (None, None) => None,
            (Some(min_port), Some(max_port)) => {
                if min_port == 0 || max_port == 0 || min_port >= max_port {
                    return Err(Error::InvalidConfig(format!(
                        "onion_udp_exit port range [{min_port}, {max_port}] must satisfy 0 < min_port < max_port"
                    )));
                }
                Some((min_port, max_port))
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(Error::InvalidConfig(
                    "onion_udp_exit.min_port and onion_udp_exit.max_port must be set together"
                        .to_string(),
                ));
            }
        };
        Ok(OnionUdpConfig {
            bind_ipv4: self.bind_ipv4.unwrap_or(Ipv4Addr::UNSPECIFIED),
            advertised_ipv4: self.advertised_ipv4,
            bind_ipv6: self.bind_ipv6.unwrap_or(Ipv6Addr::UNSPECIFIED),
            advertised_ipv6: self.advertised_ipv6,
            port_range,
            peer_policy: OnionUdpPeerPolicy::PublicOnly,
        })
    }
}

/// Runtime socket binding and peer-filtering configuration for `OnionUdpWorld`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionUdpConfig {
    /// Local IPv4 address bound when opening IPv4 `udp` exit sockets.
    pub bind_ipv4: Ipv4Addr,
    /// Optional explicit IPv4 address reported in `Bound` (`E_exit`).
    pub advertised_ipv4: Option<Ipv4Addr>,
    /// Local IPv6 address bound when opening IPv6 `udp` exit sockets.
    pub bind_ipv6: Ipv6Addr,
    /// Optional explicit IPv6 address reported in `Bound` (`E_exit`).
    pub advertised_ipv6: Option<Ipv6Addr>,
    /// Optional inclusive `(min_port, max_port)` range (`0 < min_port < max_port`).
    pub port_range: Option<(u16, u16)>,
    /// Peer egress/ingress IP policy (`PublicOnly` by default).
    pub peer_policy: OnionUdpPeerPolicy,
}

impl Default for OnionUdpConfig {
    fn default() -> Self {
        Self {
            bind_ipv4: Ipv4Addr::UNSPECIFIED,
            advertised_ipv4: None,
            bind_ipv6: Ipv6Addr::UNSPECIFIED,
            advertised_ipv6: None,
            port_range: None,
            peer_policy: OnionUdpPeerPolicy::PublicOnly,
        }
    }
}

impl OnionUdpConfig {
    /// Construct a loopback configuration (`127.0.0.1` / `::1`) admitting local peers, suitable for
    /// in-process multi-hop integration tests.
    pub const fn loopback() -> Self {
        Self {
            bind_ipv4: Ipv4Addr::LOCALHOST,
            advertised_ipv4: Some(Ipv4Addr::LOCALHOST),
            bind_ipv6: Ipv6Addr::LOCALHOST,
            advertised_ipv6: Some(Ipv6Addr::LOCALHOST),
            port_range: None,
            peer_policy: OnionUdpPeerPolicy::AllowLoopbackAndPrivate,
        }
    }
}

fn probe_default_source_ip(family: OnionUdpFamily) -> Result<IpAddr> {
    let (bind_addr, remote_probe) = match family {
        OnionUdpFamily::Ipv4 => (
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53),
        ),
        OnionUdpFamily::Ipv6 => (
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
            SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
                53,
            ),
        ),
    };
    let sock = std::net::UdpSocket::bind(bind_addr)
        .map_err(|e| udp_target(format!("probe bind failed: {e}")))?;
    sock.connect(remote_probe)
        .map_err(|e| udp_target(format!("probe route lookup failed: {e}")))?;
    let local = sock
        .local_addr()
        .map_err(|e| udp_target(format!("probe local_addr failed: {e}")))?;
    Ok(local.ip())
}

async fn bind_udp_exit_socket(
    config: &OnionUdpConfig,
    request: OnionUdpBindRequest,
) -> Result<(UdpSocket, SocketAddr)> {
    let (bind_ip, explicit_adv): (IpAddr, Option<IpAddr>) = match request.family {
        OnionUdpFamily::Ipv4 => (
            IpAddr::V4(config.bind_ipv4),
            config.advertised_ipv4.map(IpAddr::V4),
        ),
        OnionUdpFamily::Ipv6 => (
            IpAddr::V6(config.bind_ipv6),
            config.advertised_ipv6.map(IpAddr::V6),
        ),
    };

    let socket = match config.port_range {
        None => {
            let attempts = if request.even_port {
                UDP_BIND_MAX_RETRIES
            } else {
                1
            };
            let mut bound_ok = None;
            for _ in 0..attempts {
                let candidate = UdpSocket::bind(SocketAddr::new(bind_ip, 0))
                    .await
                    .map_err(|e| udp_target(format!("bind: {e}")))?;
                let port = candidate
                    .local_addr()
                    .map_err(|e| udp_target(format!("local_addr: {e}")))?
                    .port();
                if port != 0 && (!request.even_port || port % 2 == 0) {
                    bound_ok = Some(candidate);
                    break;
                }
            }
            bound_ok.ok_or_else(|| udp_target("failed to bind requested UDP port parity"))?
        }
        Some((min_port, max_port)) => {
            if min_port == 0 || max_port == 0 || min_port >= max_port {
                return Err(udp_target("invalid UDP port range"));
            }
            let mut bound_ok = None;
            if request.even_port {
                let first_even = if min_port % 2 == 0 {
                    min_port
                } else {
                    min_port.saturating_add(1)
                };
                if first_even > max_port {
                    return Err(udp_target("no even port in configured UDP port range"));
                }
                let even_count = (max_port - first_even) / 2 + 1;
                for _ in 0..UDP_BIND_MAX_RETRIES {
                    let offset = rand::thread_rng().gen_range(0..even_count);
                    let port = first_even + offset * 2;
                    if let Ok(sock) = UdpSocket::bind(SocketAddr::new(bind_ip, port)).await {
                        bound_ok = Some(sock);
                        break;
                    }
                }
            } else {
                let span = u32::from(max_port) - u32::from(min_port) + 1;
                for _ in 0..UDP_BIND_MAX_RETRIES {
                    let offset = u16::try_from(rand::thread_rng().gen_range(0..span)).unwrap_or(0);
                    let port = min_port + offset;
                    if let Ok(sock) = UdpSocket::bind(SocketAddr::new(bind_ip, port)).await {
                        bound_ok = Some(sock);
                        break;
                    }
                }
            }
            bound_ok.ok_or_else(|| udp_target("exhausted retries in configured UDP port range"))?
        }
    };

    let os_local = socket
        .local_addr()
        .map_err(|e| udp_target(format!("local_addr: {e}")))?;
    let advertised_ip = match explicit_adv {
        Some(ip) => ip,
        None if !os_local.ip().is_unspecified() => os_local.ip(),
        None => probe_default_source_ip(request.family)?,
    };
    let advertised_addr = SocketAddr::new(advertised_ip, os_local.port());
    if !config.peer_policy.allows_addr(advertised_addr) {
        return Err(udp_target(format!(
            "bound UDP exit address {advertised_addr} is not permitted by {:?}",
            config.peer_policy
        )));
    }
    Ok((socket, advertised_addr))
}

struct OnionUdpWorldShared {
    socket: SyncMutex<Option<Arc<UdpSocket>>>,
    local_addr: SocketAddr,
    policy: OnionExitPolicy,
    peer_policy: OnionUdpPeerPolicy,
    clock: SharedClock,
    pinholes: SyncMutex<OnionUdpPinholes>,
    control_out: SyncMutex<VecDeque<Bytes>>,
    closed: AtomicBool,
    wake_reader: Notify,
}

impl OnionUdpWorldShared {
    fn clone_socket(&self) -> Option<Arc<UdpSocket>> {
        self.socket
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn close_socket_now(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self
            .socket
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.wake_reader.notify_waiters();
    }

    fn allows_peer_target(&self, peer: SocketAddr) -> bool {
        if peer.is_ipv4() != self.local_addr.is_ipv4() || !self.peer_policy.allows_addr(peer) {
            return false;
        }
        let host = peer.ip().to_string();
        let Ok(target) = OnionProxyTarget::new(&host, peer.port()) else {
            return false;
        };
        self.policy.allows_target(&target)
    }

    fn allow_peer_ip(&self, ip: IpAddr, now_ms: u128) {
        self.pinholes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .allow_ip(ip, now_ms);
    }

    fn admits_inbound_ip(&self, ip: IpAddr, now_ms: u128) -> bool {
        self.pinholes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .admits(ip, now_ms)
    }

    fn queue_control(&self, bytes: Bytes) -> Result<()> {
        {
            let mut guard = self
                .control_out
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if guard.len() >= MAX_PENDING_CONTROL_ACKS {
                return Err(Error::InvalidData);
            }
            guard.push_back(bytes);
        }
        self.wake_reader.notify_waiters();
        Ok(())
    }

    fn pop_control_coalesced(&self, max_bytes: usize) -> Option<Bytes> {
        let mut guard = self
            .control_out
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let first = guard.pop_front()?;
        if guard.is_empty() || first.len() >= max_bytes {
            return Some(first);
        }
        let mut buf = BytesMut::with_capacity(max_bytes.min(1024));
        buf.extend_from_slice(&first);
        while let Some(front) = guard.front() {
            if buf.len().saturating_add(front.len()) > max_bytes {
                break;
            }
            if let Some(next) = guard.pop_front() {
                buf.extend_from_slice(&next);
            }
        }
        Some(buf.freeze())
    }
}

/// The socket world of a `udp` exit, recording its inbound reads against the exit's byte policy
/// and enforcing public-IP egress + stateful IP pinholes.
pub(crate) struct OnionUdpWorld {
    policy: OnionExitPolicy,
    accounting: OnionExitAccounting,
    config: OnionUdpConfig,
    clock: SharedClock,
}

impl OnionUdpWorld {
    /// Construct a `udp` exit world with default public-IP configuration.
    #[allow(dead_code)]
    pub(crate) fn new(policy: OnionExitPolicy, accounting: OnionExitAccounting) -> Self {
        Self::with_config_and_clock(
            policy,
            accounting,
            OnionUdpConfig::default(),
            Arc::new(SystemClock),
        )
    }

    /// Construct a `udp` exit world with explicit [`OnionUdpConfig`] and [`SharedClock`].
    pub(crate) fn with_config_and_clock(
        policy: OnionExitPolicy,
        accounting: OnionExitAccounting,
        config: OnionUdpConfig,
        clock: SharedClock,
    ) -> Self {
        Self {
            policy,
            accounting,
            config,
            clock,
        }
    }
}

/// Read half of an open `onion::udp` exit socket.
pub(crate) struct OnionUdpReader {
    shared: Arc<OnionUdpWorldShared>,
    policy: OnionExitPolicy,
    accounting: OnionExitAccounting,
    pending: Bytes,
    recv_buf: Vec<u8>,
}

/// Write half of an open `onion::udp` exit socket.
pub(crate) struct OnionUdpWriter {
    shared: Arc<OnionUdpWorldShared>,
    decoder: OnionUdpFrameDecoder,
}

#[async_trait::async_trait]
impl OnionWorld for OnionUdpWorld {
    type Reader = OnionUdpReader;
    type Writer = OnionUdpWriter;

    async fn open(&self, target: &OnionProxyTarget) -> Result<(Self::Reader, Self::Writer)> {
        let opened_at = Instant::now();
        let result = async {
            let bind_req = OnionUdpBindRequest::from_proxy_target(target)?;
            let (socket, local_addr) = bind_udp_exit_socket(&self.config, bind_req).await?;
            let initial_bound = OnionUdpExitFrame::Bound { local_addr }.encode()?;
            Ok::<_, Error>((socket, local_addr, initial_bound))
        }
        .await;
        tokio::time::sleep_until(open_response_deadline(opened_at, Instant::now())).await;
        let (socket, local_addr, initial_bound) = result?;
        let shared = Arc::new(OnionUdpWorldShared {
            socket: SyncMutex::new(Some(Arc::new(socket))),
            local_addr,
            policy: self.policy.clone(),
            peer_policy: self.config.peer_policy,
            clock: self.clock.clone(),
            pinholes: SyncMutex::new(OnionUdpPinholes::default()),
            control_out: SyncMutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
            wake_reader: Notify::new(),
        });
        Ok((
            OnionUdpReader {
                shared: Arc::clone(&shared),
                policy: self.policy.clone(),
                accounting: self.accounting.clone(),
                pending: initial_bound,
                recv_buf: vec![0_u8; MAX_UDP_DATAGRAM_BYTES],
            },
            OnionUdpWriter {
                shared,
                decoder: OnionUdpFrameDecoder::default(),
            },
        ))
    }
}

#[async_trait::async_trait]
impl OnionWorldReader for OnionUdpReader {
    async fn read(&mut self, max: usize) -> Result<Option<Bytes>> {
        let max = max.max(1);
        while self.pending.is_empty() {
            let woken = self.shared.wake_reader.notified();
            if let Some(control_bytes) = self.shared.pop_control_coalesced(max) {
                self.pending = control_bytes;
                break;
            }
            if self.shared.closed.load(Ordering::Acquire) {
                self.shared.close_socket_now();
                return Ok(None);
            }
            let Some(socket) = self.shared.clone_socket() else {
                return Ok(None);
            };
            tokio::select! {
                _ = woken => continue,
                recv_res = socket.recv_from(&mut self.recv_buf) => {
                    let (n, src_addr) = match recv_res {
                        Ok(v) => v,
                        Err(error) => {
                            if self.shared.closed.load(Ordering::Acquire) {
                                self.shared.close_socket_now();
                                return Ok(None);
                            }
                            tracing::debug!(%error, "ignoring transient onion UDP exit recv_from error");
                            continue;
                        }
                    };
                    if self.shared.closed.load(Ordering::Acquire) {
                        self.shared.close_socket_now();
                        return Ok(None);
                    }
                    let now_ms = self.shared.clock.now_ms();
                    if !self.shared.allows_peer_target(src_addr)
                        || !self.shared.admits_inbound_ip(src_addr.ip(), now_ms)
                    {
                        // Drop unsolicited or policy-denied datagrams at the OS socket reader
                        // without returning a frame, spending zero SURBs and zero exit byte quota.
                        continue;
                    }
                    let Some(slice) = self.recv_buf.get(..n) else {
                        continue;
                    };
                    let frame = OnionUdpExitFrame::Datagram {
                        peer: src_addr,
                        payload: Bytes::copy_from_slice(slice),
                    };
                    self.pending = frame.encode()?;
                }
            }
        }

        let take_len = max.min(self.pending.len());
        let chunk = self.pending.split_to(take_len);
        let byte_count = u64::try_from(chunk.len()).map_err(|_| Error::InvalidData)?;
        self.accounting
            .record_bytes(&self.policy, byte_count, self.accounting.now_ms())?;
        Ok(Some(chunk))
    }
}

#[async_trait::async_trait]
impl OnionWorldWriter for OnionUdpWriter {
    async fn write(&mut self, bytes: Bytes) -> Result<()> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(udp_closed());
        }
        self.decoder.push(&bytes)?;
        while let Some(frame) = self.decoder.next_client_frame()? {
            let now_ms = self.shared.clock.now_ms();
            match frame {
                OnionUdpClientFrame::AllowPeer { ip } => {
                    if ip.is_ipv4() == self.shared.local_addr.is_ipv4()
                        && self.shared.peer_policy.allows_ip(ip)
                    {
                        self.shared.allow_peer_ip(ip, now_ms);
                        let ack = OnionUdpExitFrame::PeerAllowed { ip }.encode()?;
                        self.shared.queue_control(ack)?;
                    }
                }
                OnionUdpClientFrame::Datagram { peer, payload } => {
                    if self.shared.allows_peer_target(peer) {
                        if let Some(socket) = self.shared.clone_socket() {
                            self.shared.allow_peer_ip(peer.ip(), now_ms);
                            let _ = socket.send_to(&payload, peer).await;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.shared.close_socket_now();
        Ok(())
    }
}

impl Drop for OnionUdpReader {
    fn drop(&mut self) {
        self.shared.close_socket_now();
    }
}

impl Drop for OnionUdpWriter {
    fn drop(&mut self) {
        self.shared.close_socket_now();
    }
}

struct SessionReceiverState {
    receiver: Option<OnionStreamReceiver>,
    decoder: OnionUdpFrameDecoder,
}

struct PendingPinholeGuard<'a> {
    session: &'a OnionUdpSession,
    ip: IpAddr,
    waiter_id: u64,
    disarmed: bool,
}

impl Drop for PendingPinholeGuard<'_> {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        let mut guard = self
            .session
            .pending_pinholes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(waiters) = guard.get_mut(&self.ip) {
            waiters.retain(|(id, _)| *id != self.waiter_id);
            if waiters.is_empty() {
                guard.remove(&self.ip);
            }
        }
    }
}

type PendingPinholeMap = HashMap<IpAddr, Vec<(u64, oneshot::Sender<()>)>>;

/// Client handle to an open multi-peer `onion::udp` datagram session (#1292).
///
/// Multiplexes `AllowPeer`/`PeerAllowed` acknowledgements and inbound `Datagram` frames directly
/// across `recv_from` and `allow_peer_ip` without spawning detached background tasks.
pub struct OnionUdpSession {
    local_addr: SocketAddr,
    sender: AsyncMutex<Option<OnionStreamSender>>,
    rx_state: AsyncMutex<SessionReceiverState>,
    datagram_backlog: SyncMutex<VecDeque<(Bytes, SocketAddr)>>,
    pending_pinholes: SyncMutex<PendingPinholeMap>,
    next_waiter_id: AtomicU64,
    closed_tx: watch::Sender<bool>,
    datagram_notify: Notify,
}

impl OnionUdpSession {
    /// Complete the initial `Bound { local_addr }` handshake over an opened `OnionClientStream`.
    pub(crate) async fn from_stream(
        stream: OnionClientStream,
        request: OnionUdpBindRequest,
    ) -> Result<Self> {
        let (sender, mut receiver) = stream.split();
        let mut decoder = OnionUdpFrameDecoder::default();
        let bound_fut = async {
            loop {
                match receiver.next().await {
                    Some(OnionStreamEvent::Data(chunk)) => {
                        decoder.push(&chunk)?;
                        if let Some(frame) = decoder.next_exit_frame()? {
                            return match frame {
                                OnionUdpExitFrame::Bound { local_addr } => Ok(local_addr),
                                _ => Err(Error::InvalidData),
                            };
                        }
                    }
                    Some(OnionStreamEvent::Fin | OnionStreamEvent::Failed) | None => {
                        return Err(Error::OnionRouteError(OnionRouteError::SessionFailed));
                    }
                }
            }
        };
        let local_addr = tokio::time::timeout(ONION_SESSION_OPEN_TIMEOUT, bound_fut)
            .await
            .map_err(|_| Error::OnionProxyRequestTimedOut)??;

        let expected_v4 = matches!(request.family, OnionUdpFamily::Ipv4);
        if local_addr.is_ipv4() != expected_v4
            || local_addr.port() == 0
            || (request.even_port && local_addr.port() % 2 != 0)
        {
            return Err(Error::InvalidData);
        }

        let (closed_tx, _) = watch::channel(false);
        Ok(Self {
            local_addr,
            sender: AsyncMutex::new(Some(sender)),
            rx_state: AsyncMutex::new(SessionReceiverState {
                receiver: Some(receiver),
                decoder,
            }),
            datagram_backlog: SyncMutex::new(VecDeque::new()),
            pending_pinholes: SyncMutex::new(HashMap::new()),
            next_waiter_id: AtomicU64::new(1),
            closed_tx,
            datagram_notify: Notify::new(),
        })
    }

    async fn wait_closed(&self) {
        let mut rx = self.closed_tx.subscribe();
        let _ = rx.wait_for(|&closed| closed).await;
    }

    fn fulfill_pinhole(&self, ip: IpAddr) {
        let waiters = self
            .pending_pinholes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&ip);
        if let Some(waiters) = waiters {
            for (_, tx) in waiters {
                let _ = tx.send(());
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_pinhole_count(&self) -> usize {
        self.pending_pinholes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(Vec::len)
            .sum()
    }

    fn pop_backlog(&self) -> Option<(Bytes, SocketAddr)> {
        self.datagram_backlog
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    fn push_backlog(&self, item: (Bytes, SocketAddr)) {
        let mut guard = self
            .datagram_backlog
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if guard.len() < SESSION_DATAGRAM_QUEUE_CAPACITY {
            guard.push_back(item);
        }
        drop(guard);
        self.datagram_notify.notify_one();
    }

    /// Return the externally reachable socket address `E_exit` bound by the exit for this session.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Return whether this session has closed or failed.
    pub fn is_closed(&self) -> bool {
        *self.closed_tx.borrow()
    }

    /// Open or refresh a stateful IP pinhole for `ip` at the exit socket and await confirmation
    /// (`PeerAllowed`) so subsequent inbound datagrams from `ip` are guaranteed to be admitted.
    pub async fn allow_peer_ip(&self, ip: IpAddr) -> Result<()> {
        if self.is_closed() {
            return Err(udp_closed());
        }
        if ip.is_ipv4() != self.local_addr.is_ipv4() {
            return Err(Error::InvalidData);
        }
        let encoded = OnionUdpClientFrame::AllowPeer { ip }.encode()?;
        let waiter_id = self.next_waiter_id.fetch_add(1, Ordering::Relaxed);
        let (ack_tx, mut ack_rx) = oneshot::channel();
        {
            let mut guard = self
                .pending_pinholes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if self.is_closed() {
                return Err(udp_closed());
            }
            guard.entry(ip).or_default().push((waiter_id, ack_tx));
        }
        let mut waiter_guard = PendingPinholeGuard {
            session: self,
            ip,
            waiter_id,
            disarmed: false,
        };

        let res = tokio::time::timeout(ONION_SESSION_OPEN_TIMEOUT, async {
            let send_res = {
                let mut sender_guard = tokio::select! {
                    biased;
                    _ = self.wait_closed() => return Err(udp_closed()),
                    guard = self.sender.lock() => guard,
                };
                let Some(sender) = sender_guard.as_mut() else {
                    return Err(udp_closed());
                };
                tokio::select! {
                    biased;
                    _ = self.wait_closed() => Err(udp_closed()),
                    res = sender.send(encoded) => res,
                }
            };
            send_res?;

            loop {
                tokio::select! {
                    biased;
                    res = &mut ack_rx => return res.map_err(|_| udp_closed()),
                    _ = self.wait_closed() => return Err(udp_closed()),
                    mut rx = self.rx_state.lock() => {
                        if let Ok(Some(())) = ack_rx.try_recv() {
                            return Ok(());
                        }
                        loop {
                            match rx.decoder.next_exit_frame() {
                                Ok(Some(OnionUdpExitFrame::PeerAllowed { ip: got_ip })) => {
                                    self.fulfill_pinhole(got_ip);
                                    if let Ok(Some(())) = ack_rx.try_recv() {
                                        return Ok(());
                                    }
                                }
                                Ok(Some(OnionUdpExitFrame::Datagram { peer, payload })) => {
                                    self.push_backlog((payload, peer));
                                }
                                Ok(Some(OnionUdpExitFrame::Bound { .. })) | Err(_) => {
                                    drop(rx);
                                    self.begin_close();
                                    return Err(udp_closed());
                                }
                                Ok(None) => break,
                            }
                        }
                        let Some(receiver) = rx.receiver.as_mut() else {
                            return Err(udp_closed());
                        };
                        tokio::select! {
                            biased;
                            res = &mut ack_rx => return res.map_err(|_| udp_closed()),
                            _ = self.wait_closed() => return Err(udp_closed()),
                            ev = receiver.next() => match ev {
                                Some(OnionStreamEvent::Data(chunk)) => {
                                    if rx.decoder.push(&chunk).is_err() {
                                        drop(rx);
                                        self.begin_close();
                                        return Err(udp_closed());
                                    }
                                }
                                Some(OnionStreamEvent::Fin | OnionStreamEvent::Failed) | None => {
                                    drop(rx);
                                    self.begin_close();
                                    return Err(udp_closed());
                                }
                            }
                        }
                    }
                }
            }
        })
        .await;

        match res {
            Ok(Ok(())) => {
                waiter_guard.disarmed = true;
                Ok(())
            }
            Ok(Err(err)) => Err(err),
            Err(_) => Err(Error::OnionProxyRequestTimedOut),
        }
    }

    /// Send a single UDP datagram `payload` to `peer` through the exit socket.
    pub async fn send_to(&self, payload: Bytes, peer: SocketAddr) -> Result<()> {
        if self.is_closed() {
            return Err(udp_closed());
        }
        if peer.is_ipv4() != self.local_addr.is_ipv4() || peer.port() == 0 {
            return Err(Error::InvalidData);
        }
        let encoded = OnionUdpClientFrame::Datagram { peer, payload }.encode()?;
        let mut sender_guard = tokio::select! {
            biased;
            _ = self.wait_closed() => return Err(udp_closed()),
            guard = self.sender.lock() => guard,
        };
        let Some(sender) = sender_guard.as_mut() else {
            return Err(udp_closed());
        };
        tokio::select! {
            biased;
            _ = self.wait_closed() => Err(udp_closed()),
            res = sender.send(encoded) => res,
        }
    }

    /// Receive the next inbound `(payload, source_peer_addr)` datagram from the exit socket.
    pub async fn recv_from(&self) -> Result<(Bytes, SocketAddr)> {
        loop {
            let dg_notified = self.datagram_notify.notified();
            if self.is_closed() {
                return Err(udp_closed());
            }
            if let Some(item) = self.pop_backlog() {
                return Ok(item);
            }
            let mut rx = tokio::select! {
                biased;
                _ = self.wait_closed() => return Err(udp_closed()),
                _ = dg_notified => continue,
                guard = self.rx_state.lock() => guard,
            };
            if self.is_closed() {
                return Err(udp_closed());
            }
            if let Some(item) = self.pop_backlog() {
                return Ok(item);
            }
            loop {
                match rx.decoder.next_exit_frame() {
                    Ok(Some(OnionUdpExitFrame::Datagram { peer, payload })) => {
                        return Ok((payload, peer));
                    }
                    Ok(Some(OnionUdpExitFrame::PeerAllowed { ip })) => {
                        self.fulfill_pinhole(ip);
                    }
                    Ok(Some(OnionUdpExitFrame::Bound { .. })) | Err(_) => {
                        drop(rx);
                        self.begin_close();
                        return Err(udp_closed());
                    }
                    Ok(None) => break,
                }
            }
            let Some(receiver) = rx.receiver.as_mut() else {
                return Err(udp_closed());
            };
            match tokio::select! {
                biased;
                _ = self.wait_closed() => return Err(udp_closed()),
                ev = receiver.next() => ev,
            } {
                Some(OnionStreamEvent::Data(chunk)) => {
                    if rx.decoder.push(&chunk).is_err() {
                        drop(rx);
                        self.begin_close();
                        return Err(udp_closed());
                    }
                }
                Some(OnionStreamEvent::Fin | OnionStreamEvent::Failed) | None => {
                    drop(rx);
                    self.begin_close();
                    return Err(udp_closed());
                }
            }
        }
    }

    /// Synchronously mark this session closed and wake any blocked `recv_from`, `allow_peer_ip`,
    /// or `send_to` call without waiting on upstream I/O.
    pub fn begin_close(&self) {
        let _ = self.closed_tx.send_replace(true);
        self.pending_pinholes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        if let Ok(mut sender_guard) = self.sender.try_lock() {
            let _ = sender_guard.take();
        }
        if let Ok(mut rx_guard) = self.rx_state.try_lock() {
            let _ = rx_guard.receiver.take();
        }
        self.datagram_notify.notify_waiters();
    }

    /// Tear down this `onion::udp` session cleanly, dropping both stream halves so the session
    /// driver immediately dispatches `abort` toward the exit and releases `E_exit`.
    pub async fn close(&self) {
        self.begin_close();
        let _ = self.sender.lock().await.take();
        let _ = self.rx_state.lock().await.receiver.take();
    }
}

impl Drop for OnionUdpSession {
    fn drop(&mut self) {
        self.begin_close();
    }
}

#[cfg(test)]
mod tests;
