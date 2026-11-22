use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::PoisonError;

use async_trait::async_trait;
use rand::Rng;
use rings_core::delegation::DelegateeKey;
use rings_core::ecc::PublicKey;
use rings_core::utils::SharedClock;
use stun::agent::TransactionId;
use stun::attributes::AttrType;
use stun::attributes::ATTR_DONT_FRAGMENT;
use stun::attributes::ATTR_EVEN_PORT;
use stun::attributes::ATTR_FINGERPRINT;
use stun::attributes::ATTR_LIFETIME;
use stun::attributes::ATTR_MESSAGE_INTEGRITY;
use stun::attributes::ATTR_NONCE;
use stun::attributes::ATTR_REALM;
use stun::attributes::ATTR_REQUESTED_ADDRESS_FAMILY;
use stun::attributes::ATTR_RESERVATION_TOKEN;
use stun::attributes::ATTR_USERNAME;
use stun::attributes::ATTR_XOR_PEER_ADDRESS;
use stun::error_code::ErrorCode;
use stun::error_code::ErrorCodeAttribute;
use stun::error_code::CODE_ADDR_FAMILY_NOT_SUPPORTED;
use stun::error_code::CODE_ALLOC_MISMATCH;
use stun::error_code::CODE_BAD_REQUEST;
use stun::error_code::CODE_FORBIDDEN;
use stun::error_code::CODE_INSUFFICIENT_CAPACITY;
use stun::error_code::CODE_PEER_ADDR_FAMILY_MISMATCH;
use stun::error_code::CODE_STALE_NONCE;
use stun::error_code::CODE_UNAUTHORIZED;
use stun::error_code::CODE_UNKNOWN_ATTRIBUTE;
use stun::error_code::CODE_UNSUPPORTED_TRANS_PROTO;
use stun::fingerprint::FINGERPRINT;
use stun::integrity::MessageIntegrity;
use stun::message::Getter;
use stun::message::Message;
use stun::message::MessageType;
use stun::message::Setter;
use stun::message::CLASS_ERROR_RESPONSE;
use stun::message::CLASS_INDICATION;
use stun::message::CLASS_REQUEST;
use stun::message::CLASS_SUCCESS_RESPONSE;
use stun::message::METHOD_ALLOCATE;
use stun::message::METHOD_BINDING;
use stun::message::METHOD_CHANNEL_BIND;
use stun::message::METHOD_CREATE_PERMISSION;
use stun::message::METHOD_DATA;
use stun::message::METHOD_REFRESH;
use stun::message::METHOD_SEND;
use stun::textattrs::Nonce;
use stun::textattrs::Realm;
use stun::textattrs::Username;
use stun::uattrs::UnknownAttributes;
use stun::xoraddr::XorMappedAddress;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time::Duration;
use tokio::time::Instant;
use turn::allocation::five_tuple::FiveTuple;
use turn::allocation::AllocationInfo;
use turn::auth::AuthHandler;
use turn::proto::chandata::ChannelData;
use turn::proto::channum::ChannelNumber;
use turn::proto::data::Data;
use turn::proto::evenport::EvenPort;
use turn::proto::lifetime::Lifetime;
use turn::proto::lifetime::DEFAULT_LIFETIME;
use turn::proto::peeraddr::PeerAddress;
use turn::proto::relayaddr::RelayedAddress;
use turn::proto::reqfamily::RequestedAddressFamily;
use turn::proto::reqfamily::REQUESTED_FAMILY_IPV4;
use turn::proto::reqtrans::RequestedTransport;
use turn::proto::rsrvtoken::ReservationToken;
use turn::proto::Protocol;
use turn::proto::PROTO_UDP;
use webrtc_util::vnet::net::Net;
use webrtc_util::Conn;

use crate::auth::DelegationAuthHandler;
use crate::bridge::BridgeAllocateStart;
use crate::bridge::BridgeChannelBindError;
use crate::bridge::BridgeCompleteAllocate;
use crate::bridge::BridgeExistingAllocate;
use crate::bridge::BridgeExpireOutcome;
use crate::bridge::BridgePermissionCheck;
use crate::bridge::BridgeRefreshOutcome;
use crate::bridge::BridgeReserveError;
use crate::bridge::TurnEntryBridge;
use crate::bridge::BRIDGE_FAILED_ALLOCATE_CACHE_TTL_MS;
use crate::bridge::BRIDGE_PERMISSION_TTL_MS;
use crate::config::RelayAddressConfig;
use crate::config::RelayPeerPolicy;
use crate::config::TurnServerConfig;
use crate::config::DEFAULT_PORT_RANGE_MAX_RETRIES;
use crate::credentials::format_turn_server_url;
use crate::credentials::SignedTurnSessionTicket;
use crate::credentials::TurnAuthIdentity;
use crate::credentials::TurnCredentialSigner;
use crate::credentials::TurnCredentials;
use crate::error::Result;

const STUN_HEADER_BYTES: usize = 20;
const INBOUND_MTU: usize = STUN_HEADER_BYTES + (u16::MAX as usize);
const STUN_ATTR_HEADER_BYTES: usize = 4;
const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const CHANNEL_DATA_HEADER_BYTES: usize = 4;
const CHANNEL_NUMBER_MIN: u16 = 0x4000;
const CHANNEL_NUMBER_MAX: u16 = 0x7FFF;
const PROTO_TCP: Protocol = Protocol(6);
const TCP_INBOUND_QUEUE_CAPACITY: usize = 256;
const TCP_PEER_CONTROL_QUEUE_CAPACITY: usize = 64;
const TCP_PEER_WRITE_QUEUE_CAPACITY: usize = 64;
const MAX_TCP_PEER_QUEUED_BYTES: usize = 256 * 1024;
const MAX_TCP_TOTAL_QUEUED_BYTES: usize = 16 * 1024 * 1024;
const MAX_TCP_CONNECTIONS_CAP: usize = 512;
const TCP_ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(20);
const TCP_INITIAL_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_UNAUTHENTICATED_TCP_FRAMES: usize = 8;
const TCP_FRAME_BODY_TIMEOUT: Duration = Duration::from_secs(10);
const TCP_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const MAXIMUM_ALLOCATION_LIFETIME: Duration = Duration::from_secs(3600);
const TCP_IDLE_READ_TIMEOUT: Duration = MAXIMUM_ALLOCATION_LIFETIME;
const NONCE_LIFETIME: Duration = Duration::from_secs(3600);
const MAX_DELETION_TXS_PER_TUPLE: usize = 16;

/// Transport-agnostic UDP datagram relay backing a single TURN allocation (#1292).
///
/// The default implementation wraps a local OS UDP socket governed by [`RelayPeerPolicy`].
/// External crates (such as `rings-turn-service`) can implement this trait over a multi-hop
/// `rings-node` `onion::udp` session without `rings-turn` depending on `rings-node`.
#[async_trait]
pub trait TurnDatagramRelay: Send + Sync + 'static {
    /// Open or refresh a stateful egress/ingress IP pinhole for `peer_ip` before the allocation
    /// commits an RFC 5766 permission or channel binding.
    async fn allow_peer_ip(&self, peer_ip: IpAddr) -> webrtc_util::Result<()> {
        let _ = peer_ip;
        Ok(())
    }

    /// Transmit a single UDP datagram `payload` from the relayed transport address to `peer`.
    async fn send_to(&self, payload: &[u8], peer: SocketAddr) -> webrtc_util::Result<usize>;

    /// Receive the next inbound UDP datagram addressed to the relayed transport address, returning
    /// `(bytes_written, source_peer_addr)`.
    async fn recv_datagram(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)>;

    /// Initiate non-blocking teardown so any concurrent `recv_datagram` wakes up immediately
    /// without waiting on a dead upstream reader.
    fn begin_close(&self);

    /// Complete asynchronous teardown and release all underlying sockets or session resources.
    async fn close(&self) -> webrtc_util::Result<()>;
}

/// Factory allocating a fresh [`TurnDatagramRelay`] and its public relayed [`SocketAddr`]
/// (`XOR-RELAYED-ADDRESS`) for each admitted TURN `Allocate` request (#1292).
#[async_trait]
pub trait TurnDatagramRelayAllocator: Send + Sync + 'static {
    /// Return whether this allocator serves IPv4 (`true`) or IPv6 (`false`) relayed addresses.
    fn is_ipv4(&self) -> bool;

    /// Allocate a fresh datagram relay endpoint, returning `(relay, relayed_socket_addr)`.
    async fn allocate_relay(
        &self,
        use_ipv4: bool,
        need_even_port: bool,
    ) -> std::result::Result<(Arc<dyn TurnDatagramRelay>, SocketAddr), turn::Error>;
}

struct PolicyRelayConn {
    inner: SyncMutex<Option<Arc<dyn Conn + Send + Sync>>>,
    closing_inner: SyncMutex<Option<Arc<dyn Conn + Send + Sync>>>,
    peer_policy: RelayPeerPolicy,
    closed_notify: Notify,
    recv_lock: Mutex<()>,
}

impl PolicyRelayConn {
    fn new(inner: Arc<dyn Conn + Send + Sync>, peer_policy: RelayPeerPolicy) -> Self {
        Self {
            inner: SyncMutex::new(Some(inner)),
            closing_inner: SyncMutex::new(None),
            peer_policy,
            closed_notify: Notify::new(),
            recv_lock: Mutex::new(()),
        }
    }

    fn clone_inner(&self) -> webrtc_util::Result<Arc<dyn Conn + Send + Sync>> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| webrtc_util::Error::Other("relay socket closed".to_string()))
    }
}

#[async_trait]
impl TurnDatagramRelay for PolicyRelayConn {
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        if !self.peer_policy.is_allowed_addr(target) {
            return Err(webrtc_util::Error::Other(
                "relay destination address denied by policy".to_string(),
            ));
        }
        let inner = self.clone_inner()?;
        inner.send_to(buf, target).await
    }

    async fn recv_datagram(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        let _guard = self.recv_lock.lock().await;
        loop {
            let notified = self.closed_notify.notified();
            let inner = self.clone_inner()?;
            let res = tokio::select! {
                _ = notified => return Err(webrtc_util::Error::Other("relay socket closed".to_string())),
                res = inner.recv_from(buf) => res,
            };
            let (n, src_addr) = match res {
                Ok(v) => v,
                Err(err) => {
                    if self.clone_inner().is_err() {
                        return Err(err);
                    }
                    tracing::debug!("ignoring transient relay recv_from error: {err}");
                    continue;
                }
            };
            if self.peer_policy.is_allowed_addr(src_addr) {
                return Ok((n, src_addr));
            }
        }
    }

    fn begin_close(&self) {
        let taken = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(conn) = taken {
            *self
                .closing_inner
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(conn);
        }
        self.closed_notify.notify_waiters();
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        self.begin_close();
        let _guard = self.recv_lock.lock().await;
        let taken = self
            .closing_inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(inner) = taken {
            let _ = inner.close().await;
        }
        Ok(())
    }
}

struct PolicyRelayAddressGenerator {
    relay: RelayAddressConfig,
    peer_policy: RelayPeerPolicy,
    net: Arc<Net>,
}

impl PolicyRelayAddressGenerator {
    fn new(relay: RelayAddressConfig, peer_policy: RelayPeerPolicy) -> Self {
        Self {
            relay,
            peer_policy,
            net: Arc::new(Net::new(None)),
        }
    }

    async fn bind_port(
        &self,
        use_ipv4: bool,
        bind_ip: IpAddr,
        relay_ip_override: Option<IpAddr>,
        port: u16,
    ) -> std::result::Result<(Arc<PolicyRelayConn>, SocketAddr), turn::Error> {
        if use_ipv4 != bind_ip.is_ipv4() {
            return Err(turn::Error::ErrInvalidRequestedFamilyValue);
        }
        let addr = SocketAddr::new(bind_ip, port);
        let conn = self.net.bind(addr).await?;
        let mut relay_addr = conn.local_addr()?;
        if let Some(relay_ip) = relay_ip_override {
            relay_addr.set_ip(relay_ip);
        }
        let policy_conn = Arc::new(PolicyRelayConn::new(conn, self.peer_policy));
        Ok((policy_conn, relay_addr))
    }

    async fn allocate_relay_conn(
        &self,
        use_ipv4: bool,
        need_even: bool,
    ) -> std::result::Result<(Arc<PolicyRelayConn>, SocketAddr), turn::Error> {
        match self.relay {
            RelayAddressConfig::None { bind_ip } => {
                let max_attempts = if need_even {
                    DEFAULT_PORT_RANGE_MAX_RETRIES
                } else {
                    1
                };
                for _ in 0..max_attempts {
                    let (conn, addr) = self.bind_port(use_ipv4, bind_ip, None, 0).await?;
                    if !need_even || addr.port() % 2 == 0 {
                        return Ok((conn, addr));
                    }
                    let _ = conn.close().await;
                }
                Err(turn::Error::ErrMaxRetriesExceeded)
            }
            RelayAddressConfig::Static { relay_ip, bind_ip } => {
                let max_attempts = if need_even {
                    DEFAULT_PORT_RANGE_MAX_RETRIES
                } else {
                    1
                };
                for _ in 0..max_attempts {
                    let (conn, addr) = self.bind_port(use_ipv4, bind_ip, Some(relay_ip), 0).await?;
                    if !need_even || addr.port() % 2 == 0 {
                        return Ok((conn, addr));
                    }
                    let _ = conn.close().await;
                }
                Err(turn::Error::ErrMaxRetriesExceeded)
            }
            RelayAddressConfig::Range {
                relay_ip,
                bind_ip,
                min_port,
                max_port,
                max_retries,
            } => {
                let retries = if max_retries == 0 {
                    DEFAULT_PORT_RANGE_MAX_RETRIES
                } else {
                    max_retries
                };
                if need_even {
                    let first_even = if min_port % 2 == 0 {
                        min_port
                    } else {
                        min_port.saturating_add(1)
                    };
                    if first_even > max_port {
                        return Err(turn::Error::ErrMaxRetriesExceeded);
                    }
                    let even_count = (max_port - first_even) / 2 + 1;
                    for _ in 0..retries {
                        let offset = rand::thread_rng().gen_range(0..even_count);
                        let port = first_even + offset * 2;
                        if let Ok(bound) = self
                            .bind_port(use_ipv4, bind_ip, Some(relay_ip), port)
                            .await
                        {
                            return Ok(bound);
                        }
                    }
                    return Err(turn::Error::ErrMaxRetriesExceeded);
                }
                let span = u32::from(max_port) - u32::from(min_port) + 1;
                for _ in 0..retries {
                    let offset = u16::try_from(rand::thread_rng().gen_range(0..span)).unwrap_or(0);
                    let port = min_port + offset;
                    if let Ok(bound) = self
                        .bind_port(use_ipv4, bind_ip, Some(relay_ip), port)
                        .await
                    {
                        return Ok(bound);
                    }
                }
                Err(turn::Error::ErrMaxRetriesExceeded)
            }
        }
    }
}

#[async_trait]
impl TurnDatagramRelayAllocator for PolicyRelayAddressGenerator {
    fn is_ipv4(&self) -> bool {
        relay_bind_ip_is_ipv4(&self.relay)
    }

    async fn allocate_relay(
        &self,
        use_ipv4: bool,
        need_even_port: bool,
    ) -> std::result::Result<(Arc<dyn TurnDatagramRelay>, SocketAddr), turn::Error> {
        let (conn, addr) = self.allocate_relay_conn(use_ipv4, need_even_port).await?;
        Ok((conn as Arc<dyn TurnDatagramRelay>, addr))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct OrderedFiveTuple {
    protocol: u8,
    src_addr: SocketAddr,
    dst_addr: SocketAddr,
}

impl From<FiveTuple> for OrderedFiveTuple {
    fn from(ft: FiveTuple) -> Self {
        Self {
            protocol: ft.protocol.0,
            src_addr: ft.src_addr,
            dst_addr: ft.dst_addr,
        }
    }
}

impl From<OrderedFiveTuple> for FiveTuple {
    fn from(oft: OrderedFiveTuple) -> Self {
        Self {
            protocol: Protocol(oft.protocol),
            src_addr: oft.src_addr,
            dst_addr: oft.dst_addr,
        }
    }
}

type ProductionBridge = TurnEntryBridge<OrderedFiveTuple, TransactionId, Vec<u8>>;
type CompletedDeletionCache = Arc<Mutex<HashMap<FiveTuple, VecDeque<CompletedDeletionEntry>>>>;

struct PendingOpenWaiter {
    conn: Arc<dyn Conn + Send + Sync>,
    src_addr: SocketAddr,
    cancel_tx: watch::Sender<bool>,
}

/// Result of reserving a TURN relay session on a [`TurnServer`] (#1292).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnSessionReservation {
    /// Server-signed unlinkable session ticket used to derive [`TurnCredentials`].
    pub ticket: SignedTurnSessionTicket,
    /// This TURN server's connectable advertised address.
    pub server_addr: SocketAddr,
    /// This TURN server's compressed secp256k1 [`PublicKey<33>`].
    pub server_pubkey: PublicKey<33>,
}

/// Bounded per-allocation outbound datagram queue isolating the shared TURN receive loop from
/// backpressured asynchronous relay backends (e.g. `onion::udp`).
const ALLOCATION_OUTBOUND_QUEUE_CAPACITY: usize = 64;

/// Maximum concurrent in-flight `CreatePermission` / `ChannelBind` backend requests per allocation.
const MAX_IN_FLIGHT_CONTROL_OPS_PER_ALLOCATION: usize = 8;

struct AllocationControlPermit(Arc<AtomicUsize>);

impl Drop for AllocationControlPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ManagedAllocation {
    alloc_id: u64,
    five_tuple: FiveTuple,
    ord_tuple: OrderedFiveTuple,
    username: String,
    relay_addr: SocketAddr,
    relay_conn: Arc<dyn TurnDatagramRelay>,
    expiry_tx: watch::Sender<Instant>,
    tcp_authenticated_tx: Option<watch::Sender<Option<u64>>>,
    closed_tx: watch::Sender<bool>,
    in_flight_control: Arc<AtomicUsize>,
    control_op_lock: Mutex<()>,
    outbound_tx: mpsc::Sender<(Vec<u8>, SocketAddr)>,
    outbound_worker: SyncMutex<Option<JoinHandle<()>>>,
}

impl ManagedAllocation {
    fn new(
        alloc_id: u64,
        five_tuple: FiveTuple,
        username: String,
        relay_addr: SocketAddr,
        relay_conn: Arc<dyn TurnDatagramRelay>,
        lifetime: Duration,
        tcp_authenticated_tx: Option<watch::Sender<Option<u64>>>,
    ) -> Arc<Self> {
        if let Some(ref tx) = tcp_authenticated_tx {
            let _ = tx.send_replace(Some(alloc_id));
        }
        let (expiry_tx, _) = watch::channel(Instant::now() + lifetime);
        let (closed_tx, closed_rx) = watch::channel(false);
        let (outbound_tx, mut outbound_rx) =
            mpsc::channel::<(Vec<u8>, SocketAddr)>(ALLOCATION_OUTBOUND_QUEUE_CAPACITY);
        let worker_relay = Arc::clone(&relay_conn);
        let mut worker_closed_rx = closed_rx;
        let worker_handle = tokio::spawn(async move {
            loop {
                if *worker_closed_rx.borrow() {
                    break;
                }
                let item = tokio::select! {
                    biased;
                    _ = worker_closed_rx.wait_for(|&closed| closed) => break,
                    next = outbound_rx.recv() => next,
                };
                let Some((payload, dst)) = item else {
                    break;
                };
                let _ = tokio::select! {
                    biased;
                    _ = worker_closed_rx.wait_for(|&closed| closed) => break,
                    res = worker_relay.send_to(&payload, dst) => res,
                };
            }
        });

        Arc::new(Self {
            alloc_id,
            five_tuple,
            ord_tuple: OrderedFiveTuple::from(five_tuple),
            username,
            relay_addr,
            relay_conn,
            expiry_tx,
            tcp_authenticated_tx,
            closed_tx,
            in_flight_control: Arc::new(AtomicUsize::new(0)),
            control_op_lock: Mutex::new(()),
            outbound_tx,
            outbound_worker: SyncMutex::new(Some(worker_handle)),
        })
    }

    async fn wait_closed(&self) {
        let mut rx = self.closed_tx.subscribe();
        let _ = rx.wait_for(|&closed| closed).await;
    }

    fn try_acquire_control_permit(&self) -> Option<AllocationControlPermit> {
        if *self.closed_tx.borrow() {
            return None;
        }
        self.in_flight_control
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                (cur < MAX_IN_FLIGHT_CONTROL_OPS_PER_ALLOCATION).then_some(cur + 1)
            })
            .ok()
            .map(|_| AllocationControlPermit(Arc::clone(&self.in_flight_control)))
    }

    fn refresh_wall_deadline(&self, lifetime: Duration) {
        let _ = self.expiry_tx.send_replace(Instant::now() + lifetime);
    }

    fn begin_close(&self) {
        if let Some(ref tx) = self.tcp_authenticated_tx {
            let alloc_id = self.alloc_id;
            tx.send_if_modified(|current| {
                if *current == Some(alloc_id) {
                    *current = None;
                    true
                } else {
                    false
                }
            });
        }
        let _ = self.expiry_tx.send_replace(Instant::now());
        let _ = self.closed_tx.send_replace(true);
        self.relay_conn.begin_close();
    }

    async fn finish_close(&self) {
        self.begin_close();
        let handle = self
            .outbound_worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
        let _ = self.relay_conn.close().await;
    }

    async fn recv_payload_from_peer(
        &self,
        buf: &mut [u8],
    ) -> webrtc_util::Result<(usize, SocketAddr)> {
        self.relay_conn.recv_datagram(buf).await
    }

    fn enqueue_payload_to_peer(&self, payload: &[u8], target: SocketAddr) {
        if !*self.closed_tx.borrow() {
            let _ = self.outbound_tx.try_send((payload.to_vec(), target));
        }
    }
}

fn encode_turn_tcp_frame(datagram: &[u8]) -> Option<Vec<u8>> {
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

fn is_stun_control_response(datagram: &[u8]) -> bool {
    let Some(&b0) = datagram.first() else {
        return false;
    };
    if datagram.get(4..8) != Some(&STUN_MAGIC_COOKIE) {
        return false;
    }
    // RFC 5389 Figure 3: STUN messages have top 2 bits 00, and STUN class bit C1 (bit 0 of byte 0)
    // is 1 for both Success Response (class 0b10) and Error Response (class 0b11), and 0 for
    // Request (0b00) and Indication (0b01, such as Data Indication 0x0017).
    (b0 & 0b1100_0001) == 0b0000_0001
}

async fn read_turn_tcp_frame_body<R>(reader: &mut R, prefix: [u8; 4]) -> std::io::Result<Vec<u8>>
where R: AsyncRead + Unpin {
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
        tokio::time::timeout(TCP_FRAME_BODY_TIMEOUT, reader.read_exact(&mut hdr_rest))
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
                tokio::time::timeout(TCP_FRAME_BODY_TIMEOUT, reader.read_exact(body))
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
                tokio::time::timeout(TCP_FRAME_BODY_TIMEOUT, reader.read_exact(body))
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

struct QueuedTcpFrame {
    bytes: Vec<u8>,
    peer_queued_bytes: Arc<AtomicUsize>,
    total_queued_bytes: Arc<AtomicUsize>,
}

impl QueuedTcpFrame {
    fn try_reserve(
        bytes: Vec<u8>,
        peer_queued_bytes: &Arc<AtomicUsize>,
        total_queued_bytes: &Arc<AtomicUsize>,
        enforce_peer_cap: bool,
    ) -> Option<Self> {
        let len = bytes.len();
        if enforce_peer_cap {
            let prev_peer = peer_queued_bytes.fetch_add(len, Ordering::AcqRel);
            if prev_peer.saturating_add(len) > MAX_TCP_PEER_QUEUED_BYTES {
                peer_queued_bytes.fetch_sub(len, Ordering::Release);
                return None;
            }
        } else {
            peer_queued_bytes.fetch_add(len, Ordering::AcqRel);
        }
        let prev_total = total_queued_bytes.fetch_add(len, Ordering::AcqRel);
        if prev_total.saturating_add(len) > MAX_TCP_TOTAL_QUEUED_BYTES {
            total_queued_bytes.fetch_sub(len, Ordering::Release);
            peer_queued_bytes.fetch_sub(len, Ordering::Release);
            return None;
        }
        Some(Self {
            bytes,
            peer_queued_bytes: Arc::clone(peer_queued_bytes),
            total_queued_bytes: Arc::clone(total_queued_bytes),
        })
    }
}

impl Drop for QueuedTcpFrame {
    fn drop(&mut self) {
        let len = self.bytes.len();
        self.peer_queued_bytes.fetch_sub(len, Ordering::Release);
        self.total_queued_bytes.fetch_sub(len, Ordering::Release);
    }
}

#[derive(Clone)]
struct TcpPeerEntry {
    conn_id: u64,
    control_tx: mpsc::Sender<QueuedTcpFrame>,
    data_tx: mpsc::Sender<QueuedTcpFrame>,
    peer_queued_bytes: Arc<AtomicUsize>,
    authenticated_tx: watch::Sender<Option<u64>>,
    evict_tx: watch::Sender<bool>,
}

#[derive(Clone)]
struct TcpPeerHandoff {
    conn_id: u64,
    evict_tx: watch::Sender<bool>,
    done_rx: watch::Receiver<bool>,
}

type TcpPeerMap = HashMap<SocketAddr, TcpPeerEntry>;
type TcpPeerHandoffMap = HashMap<SocketAddr, TcpPeerHandoff>;
type TcpDisconnectAckMap = HashMap<SocketAddr, VecDeque<tokio::sync::oneshot::Sender<()>>>;

struct TcpTurnListenerConn {
    local_addr: SocketAddr,
    inbound_rx: Mutex<mpsc::Receiver<(Vec<u8>, SocketAddr)>>,
    peers: Arc<SyncMutex<TcpPeerMap>>,
    total_queued_bytes: Arc<AtomicUsize>,
    disconnect_acks: Arc<SyncMutex<TcpDisconnectAckMap>>,
    shutdown_tx: watch::Sender<bool>,
    accept_handle: Mutex<Option<JoinHandle<()>>>,
}

impl TcpTurnListenerConn {
    fn spawn(
        listener: TcpListener,
        local_addr: SocketAddr,
        connection_semaphore: Arc<Semaphore>,
    ) -> Arc<Self> {
        let (inbound_tx, inbound_rx) = mpsc::channel(TCP_INBOUND_QUEUE_CAPACITY);
        let peers: Arc<SyncMutex<TcpPeerMap>> = Arc::new(SyncMutex::new(HashMap::new()));
        let total_queued_bytes = Arc::new(AtomicUsize::new(0));
        let handoffs: Arc<SyncMutex<TcpPeerHandoffMap>> = Arc::new(SyncMutex::new(HashMap::new()));
        let disconnect_acks: Arc<SyncMutex<TcpDisconnectAckMap>> =
            Arc::new(SyncMutex::new(HashMap::new()));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let next_conn_id = Arc::new(AtomicU64::new(1));

        let peers_for_accept = Arc::clone(&peers);
        let handoffs_for_accept = Arc::clone(&handoffs);
        let disconnect_acks_for_accept = Arc::clone(&disconnect_acks);
        let accept_handle = tokio::spawn(async move {
            let mut peer_tasks = JoinSet::new();
            loop {
                if *shutdown_rx.borrow_and_update() {
                    break;
                }
                let accepted = tokio::select! {
                    res = listener.accept() => res,
                    Some(_) = peer_tasks.join_next(), if !peer_tasks.is_empty() => continue,
                    res = shutdown_rx.changed() => {
                        if res.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                        continue;
                    }
                };
                let (tcp_stream, peer_addr) = match accepted {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::debug!("ignoring transient TCP TURN accept error: {err}");
                        tokio::select! {
                            () = tokio::time::sleep(TCP_ACCEPT_ERROR_BACKOFF) => {},
                            res = shutdown_rx.changed() => {
                                if res.is_err() || *shutdown_rx.borrow() {
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                };
                let Ok(permit) = Arc::clone(&connection_semaphore).try_acquire_owned() else {
                    tracing::debug!(%peer_addr, "rejecting TCP TURN connection: connection cap reached");
                    drop(tcp_stream);
                    continue;
                };
                let _ = tcp_stream.set_nodelay(true);
                let conn_id = next_conn_id.fetch_add(1, Ordering::Relaxed);
                let (control_tx, mut control_rx) =
                    mpsc::channel::<QueuedTcpFrame>(TCP_PEER_CONTROL_QUEUE_CAPACITY);
                let (data_tx, mut data_rx) =
                    mpsc::channel::<QueuedTcpFrame>(TCP_PEER_WRITE_QUEUE_CAPACITY);
                let peer_queued_bytes = Arc::new(AtomicUsize::new(0));
                let (evict_tx, mut evict_rx) = watch::channel(false);
                let (done_tx, done_rx) = watch::channel(false);
                let (authenticated_tx, mut authenticated_rx) = watch::channel(None);

                // Synchronously chain same-address reconnects (`A -> B -> C`) in `accept()` so
                // each successor observes and waits on its immediate predecessor in strict order.
                let previous_handoff = handoffs_for_accept
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(peer_addr, TcpPeerHandoff {
                        conn_id,
                        evict_tx: evict_tx.clone(),
                        done_rx: done_rx.clone(),
                    });

                let (mut reader, mut writer) = tokio::io::split(tcp_stream);
                let inbound_tx_peer = inbound_tx.clone();
                let peers_cleanup = Arc::clone(&peers_for_accept);
                let handoffs_cleanup = Arc::clone(&handoffs_for_accept);
                let disconnect_acks_peer = Arc::clone(&disconnect_acks_for_accept);
                let mut peer_shutdown_read = shutdown_rx.clone();
                let mut peer_shutdown_write = shutdown_rx.clone();

                peer_tasks.spawn(async move {
                    let _permit = permit;
                    // If a prior TCP connection from the exact same `(ip, port)` is still winding
                    // down, evict it and wait for `TurnServer::read_loop` to finish processing its
                    // disconnect sentinel before registering or reading from this connection.
                    if let Some(prev) = previous_handoff {
                        let _ = prev.evict_tx.send_replace(true);
                        let mut prev_done = prev.done_rx;
                        tokio::select! {
                            _ = prev_done.wait_for(|&done| done) => {},
                            _ = peer_shutdown_read.changed() => {
                                if *peer_shutdown_read.borrow() {
                                    let _ = done_tx.send_replace(true);
                                    return;
                                }
                            }
                        }
                    }

                    // If a newer connection (`C`) arrived while we (`B`) were waiting for `A`,
                    // exit immediately without touching `peers` or reading from the socket.
                    if *evict_rx.borrow() || *peer_shutdown_read.borrow() {
                        {
                            let mut guard = handoffs_cleanup
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            if guard.get(&peer_addr).is_some_and(|h| h.conn_id == conn_id) {
                                guard.remove(&peer_addr);
                            }
                        }
                        let _ = done_tx.send_replace(true);
                        return;
                    }

                    peers_cleanup
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(peer_addr, TcpPeerEntry {
                            conn_id,
                            control_tx,
                            data_tx,
                            peer_queued_bytes,
                            authenticated_tx,
                            evict_tx: evict_tx.clone(),
                        });

                    let mut evict_read = evict_rx.clone();
                    let read_loop = async {
                        let mut unauth_deadline = Instant::now() + TCP_INITIAL_FRAME_TIMEOUT;
                        let mut last_activity = Instant::now();
                        let mut unauth_frames = 0usize;
                        let mut was_authenticated = false;
                        loop {
                            let mut prefix = [0u8; 4];
                            let mut prefix_filled = 0usize;
                            let prefix_ok = loop {
                                if *peer_shutdown_read.borrow_and_update()
                                    || *evict_read.borrow_and_update()
                                {
                                    break false;
                                }
                                let is_auth = authenticated_rx.borrow_and_update().is_some();
                                if is_auth {
                                    was_authenticated = true;
                                    unauth_frames = 0;
                                } else if was_authenticated {
                                    was_authenticated = false;
                                    unauth_deadline = Instant::now() + TCP_INITIAL_FRAME_TIMEOUT;
                                    unauth_frames = 0;
                                } else if unauth_frames >= MAX_UNAUTHENTICATED_TCP_FRAMES {
                                    break false;
                                }
                                let deadline = if is_auth {
                                    if prefix_filled > 0 {
                                        (last_activity + TCP_FRAME_BODY_TIMEOUT)
                                            .min(last_activity + TCP_IDLE_READ_TIMEOUT)
                                    } else {
                                        last_activity + TCP_IDLE_READ_TIMEOUT
                                    }
                                } else if prefix_filled > 0 {
                                    (last_activity + TCP_FRAME_BODY_TIMEOUT).min(unauth_deadline)
                                } else {
                                    unauth_deadline
                                };
                                let Some(rem_slice) = prefix.get_mut(prefix_filled..) else {
                                    break true;
                                };
                                if rem_slice.is_empty() {
                                    break true;
                                }
                                tokio::select! {
                                    res = reader.read(rem_slice) => {
                                        match res {
                                            Ok(0) | Err(_) => break false,
                                            Ok(n) => {
                                                last_activity = Instant::now();
                                                prefix_filled = prefix_filled.saturating_add(n);
                                                if prefix_filled >= 4 {
                                                    break true;
                                                }
                                            }
                                        }
                                    }
                                    () = tokio::time::sleep_until(deadline) => break false,
                                    res = authenticated_rx.changed() => {
                                        if res.is_err() {
                                            break false;
                                        }
                                    }
                                    _ = evict_read.changed() => break false,
                                    res = peer_shutdown_read.changed() => {
                                        if res.is_err() || *peer_shutdown_read.borrow() {
                                            break false;
                                        }
                                    }
                                }
                            };
                            if !prefix_ok {
                                break;
                            }
                            let frame_res = tokio::select! {
                                res = read_turn_tcp_frame_body(&mut reader, prefix) => res,
                                _ = evict_read.changed() => break,
                                res = peer_shutdown_read.changed() => {
                                    if res.is_err() || *peer_shutdown_read.borrow() {
                                        break;
                                    }
                                    break;
                                }
                            };
                            let Ok(frame) = frame_res else {
                                break;
                            };
                            last_activity = Instant::now();
                            if authenticated_rx.borrow().is_none() {
                                unauth_frames = unauth_frames.saturating_add(1);
                            }
                            tokio::select! {
                                res = inbound_tx_peer.send((frame, peer_addr)) => {
                                    if res.is_err() {
                                        break;
                                    }
                                }
                                _ = evict_read.changed() => break,
                                res = peer_shutdown_read.changed() => {
                                    if res.is_err() || *peer_shutdown_read.borrow() {
                                        break;
                                    }
                                }
                            }
                        }
                    };

                    let write_loop = async {
                        loop {
                            if *peer_shutdown_write.borrow_and_update()
                                || *evict_rx.borrow_and_update()
                            {
                                break;
                            }
                            let maybe_pkt = tokio::select! {
                                biased;
                                pkt = control_rx.recv() => pkt,
                                pkt = data_rx.recv() => pkt,
                                _ = evict_rx.changed() => break,
                                res = peer_shutdown_write.changed() => {
                                    if res.is_err() || *peer_shutdown_write.borrow() {
                                        break;
                                    }
                                    continue;
                                }
                            };
                            let Some(pkt) = maybe_pkt else {
                                break;
                            };
                            let write_res = tokio::time::timeout(TCP_WRITE_TIMEOUT, async {
                                writer.write_all(&pkt.bytes).await?;
                                writer.flush().await
                            })
                            .await;
                            drop(pkt);
                            if !matches!(write_res, Ok(Ok(()))) {
                                break;
                            }
                        }
                        let _ = tokio::time::timeout(TCP_WRITE_TIMEOUT, writer.shutdown()).await;
                    };

                    tokio::select! {
                        _ = read_loop => {},
                        _ = write_loop => {},
                    }

                    let removed_current = {
                        let mut guard =
                            peers_cleanup.lock().unwrap_or_else(PoisonError::into_inner);
                        if guard
                            .get(&peer_addr)
                            .is_some_and(|entry| entry.conn_id == conn_id)
                        {
                            guard.remove(&peer_addr);
                            true
                        } else {
                            false
                        }
                    };

                    if removed_current && !*peer_shutdown_read.borrow() {
                        // Enqueue a 0-byte disconnect sentinel in FIFO order and wait for
                        // `TurnServer::read_loop` to retire any allocation/cached state for this
                        // 5-tuple before releasing `done_tx` to any waiting successor connection.
                        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
                        disconnect_acks_peer
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .entry(peer_addr)
                            .or_default()
                            .push_back(ack_tx);
                        let sent = tokio::select! {
                            res = inbound_tx_peer.send((Vec::new(), peer_addr)) => res.is_ok(),
                            _ = peer_shutdown_read.changed() => false,
                        };
                        if sent && !*peer_shutdown_read.borrow() {
                            tokio::select! {
                                _ = ack_rx => {},
                                _ = peer_shutdown_read.changed() => {},
                            }
                        }
                    }

                    {
                        let mut guard = handoffs_cleanup
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        if guard.get(&peer_addr).is_some_and(|h| h.conn_id == conn_id) {
                            guard.remove(&peer_addr);
                        }
                    }
                    let _ = done_tx.send_replace(true);
                });
            }

            drop(listener);
            while peer_tasks.join_next().await.is_some() {}
        });

        Arc::new(Self {
            local_addr,
            inbound_rx: Mutex::new(inbound_rx),
            peers,
            total_queued_bytes,
            disconnect_acks,
            shutdown_tx,
            accept_handle: Mutex::new(Some(accept_handle)),
        })
    }

    fn peer_context(&self, peer_addr: SocketAddr) -> Option<(u64, watch::Sender<Option<u64>>)> {
        self.peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&peer_addr)
            .map(|entry| (entry.conn_id, entry.authenticated_tx.clone()))
    }

    fn send_to_peer_conn(
        &self,
        buf: &[u8],
        target: SocketAddr,
        expected_conn_id: Option<u64>,
    ) -> webrtc_util::Result<usize> {
        let Some(framed) = encode_turn_tcp_frame(buf) else {
            return Err(webrtc_util::Error::Other(
                "invalid outbound TURN TCP frame".to_string(),
            ));
        };
        let is_control = is_stun_control_response(buf);
        let maybe_peer = self
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&target)
            .and_then(|entry| {
                if let Some(cid) = expected_conn_id {
                    if entry.conn_id != cid {
                        return None;
                    }
                }
                let tx = if is_control {
                    entry.control_tx.clone()
                } else {
                    entry.data_tx.clone()
                };
                Some((
                    tx,
                    Arc::clone(&entry.peer_queued_bytes),
                    entry.evict_tx.clone(),
                ))
            });
        let Some((tx, peer_queued_bytes, evict_tx)) = maybe_peer else {
            return Err(webrtc_util::Error::Other(
                "TCP TURN peer disconnected".to_string(),
            ));
        };
        let Some(queued_frame) = QueuedTcpFrame::try_reserve(
            framed,
            &peer_queued_bytes,
            &self.total_queued_bytes,
            !is_control,
        ) else {
            if is_control {
                let _ = evict_tx.send_replace(true);
            }
            return Err(webrtc_util::Error::Other(
                "TCP TURN outbound byte budget exceeded".to_string(),
            ));
        };
        if let Err(err) = tx.try_send(queued_frame) {
            if is_control {
                let _ = evict_tx.send_replace(true);
            }
            return Err(webrtc_util::Error::Other(err.to_string()));
        }
        Ok(buf.len())
    }

    fn notify_peer_disconnect_processed(&self, peer_addr: SocketAddr) {
        let maybe_tx = {
            let mut guard = self
                .disconnect_acks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let tx = guard.get_mut(&peer_addr).and_then(VecDeque::pop_front);
            if guard.get(&peer_addr).is_some_and(VecDeque::is_empty) {
                guard.remove(&peer_addr);
            }
            tx
        };
        if let Some(tx) = maybe_tx {
            let _ = tx.send(());
        }
    }
}

#[async_trait]
impl Conn for TcpTurnListenerConn {
    async fn connect(&self, _addr: SocketAddr) -> webrtc_util::Result<()> {
        Err(webrtc_util::Error::Other("not applicable".to_string()))
    }

    async fn recv(&self, _buf: &mut [u8]) -> webrtc_util::Result<usize> {
        Err(webrtc_util::Error::Other("not applicable".to_string()))
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        let mut rx = self.inbound_rx.lock().await;
        let Some((frame, peer_addr)) = rx.recv().await else {
            return Err(webrtc_util::Error::Other(
                "TCP TURN listener closed".to_string(),
            ));
        };
        let n = buf.len().min(frame.len());
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), frame.get(..n)) {
            dst.copy_from_slice(src);
        }
        Ok((n, peer_addr))
    }

    async fn send(&self, _buf: &[u8]) -> webrtc_util::Result<usize> {
        Err(webrtc_util::Error::Other("not applicable".to_string()))
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        self.send_to_peer_conn(buf, target, None)
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        let _ = self.shutdown_tx.send_replace(true);
        let handle = self.accept_handle.lock().await.take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
        self.peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.disconnect_acks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

struct RecordingConn {
    inner: SyncMutex<Option<Arc<dyn Conn + Send + Sync>>>,
    local_addr: SocketAddr,
    protocol: Protocol,
}

impl RecordingConn {
    fn new(inner: Arc<dyn Conn + Send + Sync>, local_addr: SocketAddr) -> Self {
        Self {
            inner: SyncMutex::new(Some(inner)),
            local_addr,
            protocol: PROTO_UDP,
        }
    }

    fn new_tcp(inner: Arc<dyn Conn + Send + Sync>, local_addr: SocketAddr) -> Self {
        Self {
            inner: SyncMutex::new(Some(inner)),
            local_addr,
            protocol: PROTO_TCP,
        }
    }

    fn tcp_peer_context(&self, peer_addr: SocketAddr) -> Option<(u64, watch::Sender<Option<u64>>)> {
        let inner = self.clone_inner().ok()?;
        let tcp_conn = inner.as_any().downcast_ref::<TcpTurnListenerConn>()?;
        tcp_conn.peer_context(peer_addr)
    }

    fn send_to_tcp_peer_conn(
        &self,
        buf: &[u8],
        target: SocketAddr,
        expected_conn_id: u64,
    ) -> webrtc_util::Result<usize> {
        let inner = self.clone_inner()?;
        let Some(tcp_conn) = inner.as_any().downcast_ref::<TcpTurnListenerConn>() else {
            return Err(webrtc_util::Error::Other("not a TCP listener".to_string()));
        };
        tcp_conn.send_to_peer_conn(buf, target, Some(expected_conn_id))
    }

    fn notify_tcp_peer_disconnect_processed(&self, peer_addr: SocketAddr) {
        if let Ok(inner) = self.clone_inner() {
            if let Some(tcp_conn) = inner.as_any().downcast_ref::<TcpTurnListenerConn>() {
                tcp_conn.notify_peer_disconnect_processed(peer_addr);
            }
        }
    }

    fn clone_inner(&self) -> webrtc_util::Result<Arc<dyn Conn + Send + Sync>> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| webrtc_util::Error::Other("TURN listener closed".to_string()))
    }

    fn is_closed(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
    }

    async fn shutdown_listener(&self) {
        let taken = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(inner) = taken {
            let _ = inner.close().await;
        }
    }
}

#[async_trait]
impl Conn for RecordingConn {
    async fn connect(&self, addr: SocketAddr) -> webrtc_util::Result<()> {
        let inner = self.clone_inner()?;
        inner.connect(addr).await
    }

    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        let inner = self.clone_inner()?;
        inner.recv(buf).await
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        let inner = self.clone_inner()?;
        inner.recv_from(buf).await
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        let inner = self.clone_inner()?;
        inner.send(buf).await
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        let inner = self.clone_inner()?;
        inner.send_to(buf, target).await
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|c| c.remote_addr())
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

struct ScopedTcpPeerConn {
    recording: Arc<RecordingConn>,
    conn_id: u64,
}

#[async_trait]
impl Conn for ScopedTcpPeerConn {
    async fn connect(&self, addr: SocketAddr) -> webrtc_util::Result<()> {
        self.recording.connect(addr).await
    }

    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        self.recording.recv(buf).await
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        self.recording.recv_from(buf).await
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        self.recording.send(buf).await
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        self.recording
            .send_to_tcp_peer_conn(buf, target, self.conn_id)
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        self.recording.local_addr()
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        self.recording.remote_addr()
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

fn generate_nonce() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill(&mut bytes);
    let mut out = String::with_capacity(32);
    for b in bytes {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

async fn prune_and_bound_nonces(
    nonces: &Mutex<HashMap<String, (FiveTuple, Instant)>>,
    max_nonces: usize,
    reserve_one: bool,
) {
    let mut guard = nonces.lock().await;
    if max_nonces == 0 {
        guard.clear();
        return;
    }
    let now = Instant::now();
    guard.retain(|_, (_, created_at)| {
        now.checked_duration_since(*created_at).unwrap_or_default() < NONCE_LIFETIME
    });
    let target_cap = if reserve_one {
        max_nonces.saturating_sub(1)
    } else {
        max_nonces
    };
    while guard.len() > target_cap {
        let Some(oldest_key) = guard
            .iter()
            .min_by_key(|(_, (_, created_at))| *created_at)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        guard.remove(&oldest_key);
    }
}

const fn padded_attr_value_len(len: usize) -> usize {
    (len + 3) & !3
}

fn truncate_message_attributes(m: &mut Message, keep_count: usize) {
    if m.attributes.0.len() <= keep_count {
        return;
    }
    let mut removed_bytes = 0usize;
    for attr in m.attributes.0.iter().skip(keep_count) {
        removed_bytes += STUN_ATTR_HEADER_BYTES + padded_attr_value_len(usize::from(attr.length));
    }
    m.attributes.0.truncate(keep_count);
    m.length = m
        .length
        .saturating_sub(u32::try_from(removed_bytes).unwrap_or(u32::MAX));
    m.write_length();
    let new_raw_len = STUN_HEADER_BYTES + usize::try_from(m.length).unwrap_or(0);
    if m.raw.len() > new_raw_len {
        m.raw.truncate(new_raw_len);
    }
}

/// Enforce RFC 5389 Section 15.4 & 15.5 on messages carrying `MESSAGE-INTEGRITY`:
/// `FINGERPRINT` must not precede `MESSAGE-INTEGRITY`, and any attribute following
/// `MESSAGE-INTEGRITY` other than an immediately-following valid `FINGERPRINT` is
/// ignored (stripped from both `m.attributes` and `m.raw`) before dispatch.
fn sanitize_authenticated_message(m: &mut Message) -> bool {
    let Some(mi_idx) = m
        .attributes
        .0
        .iter()
        .position(|attr| attr.typ == ATTR_MESSAGE_INTEGRITY)
    else {
        return true;
    };
    if m.attributes
        .0
        .iter()
        .take(mi_idx)
        .any(|attr| attr.typ == ATTR_FINGERPRINT)
    {
        return false;
    }
    if m.attributes.0.get(mi_idx + 1).map(|a| a.typ) == Some(ATTR_FINGERPRINT) {
        truncate_message_attributes(m, mi_idx + 2);
        if FINGERPRINT.check(m).is_err() {
            return false;
        }
    } else {
        truncate_message_attributes(m, mi_idx + 1);
    }
    true
}

fn check_message_peer_addresses(m: &Message, peer_policy: RelayPeerPolicy) -> bool {
    let ignore_port = m.typ.method == METHOD_CREATE_PERMISSION;
    for attr in &m.attributes.0 {
        if attr.typ == ATTR_MESSAGE_INTEGRITY {
            break;
        }
        if attr.typ != ATTR_XOR_PEER_ADDRESS {
            continue;
        }
        let mut single = Message {
            transaction_id: m.transaction_id,
            ..Default::default()
        };
        single.attributes.0.push(attr.clone());
        let mut peer_addr = PeerAddress::default();
        if peer_addr.get_from(&single).is_err() {
            return false;
        }
        let allowed = if ignore_port {
            peer_policy.is_allowed_ip(peer_addr.ip)
        } else {
            peer_policy.is_allowed_addr(SocketAddr::new(peer_addr.ip, peer_addr.port))
        };
        if !allowed {
            return false;
        }
    }
    true
}

async fn send_unauthenticated_error(
    conn: &Arc<dyn Conn + Send + Sync>,
    dst: SocketAddr,
    m: &Message,
    code: ErrorCode,
    reason: &[u8],
) {
    let built = {
        let mut err_msg = Message::new();
        let setters: Vec<Box<dyn Setter>> = vec![
            Box::new(Message {
                transaction_id: m.transaction_id,
                ..Default::default()
            }),
            Box::new(MessageType::new(m.typ.method, CLASS_ERROR_RESPONSE)),
            Box::new(ErrorCodeAttribute {
                code,
                reason: reason.to_vec(),
            }),
            Box::new(FINGERPRINT),
        ];
        err_msg.build(&setters).is_ok().then_some(err_msg.raw)
    };
    if let Some(raw) = built {
        let _ = conn.send_to(&raw, dst).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn issue_nonce_challenge(
    conn: &Arc<dyn Conn + Send + Sync>,
    nonces: &Mutex<HashMap<String, (FiveTuple, Instant)>>,
    max_nonces: usize,
    realm: &str,
    five_tuple: FiveTuple,
    m: &Message,
    code: ErrorCode,
    reason: &[u8],
) {
    let src_addr = five_tuple.src_addr;
    prune_and_bound_nonces(nonces, max_nonces, true).await;
    let nonce = generate_nonce();
    nonces
        .lock()
        .await
        .insert(nonce.clone(), (five_tuple, Instant::now()));
    let built = {
        let mut resp = Message::new();
        let setters: Vec<Box<dyn Setter>> = vec![
            Box::new(Message {
                transaction_id: m.transaction_id,
                ..Default::default()
            }),
            Box::new(MessageType::new(m.typ.method, CLASS_ERROR_RESPONSE)),
            Box::new(ErrorCodeAttribute {
                code,
                reason: reason.to_vec(),
            }),
            Box::new(Nonce::new(ATTR_NONCE, nonce)),
            Box::new(Realm::new(ATTR_REALM, realm.to_string())),
            Box::new(FINGERPRINT),
        ];
        resp.build(&setters).is_ok().then_some(resp.raw)
    };
    if let Some(raw) = built {
        let _ = conn.send_to(&raw, src_addr).await;
    }
}

async fn authenticate_turn_request(
    conn: &Arc<dyn Conn + Send + Sync>,
    nonces: &Mutex<HashMap<String, (FiveTuple, Instant)>>,
    max_nonces: usize,
    auth_handler: &DelegationAuthHandler,
    realm: &str,
    five_tuple: FiveTuple,
    m: &Message,
) -> Option<(Username, Vec<u8>)> {
    let src_addr = five_tuple.src_addr;
    if !m.contains(ATTR_MESSAGE_INTEGRITY) {
        issue_nonce_challenge(
            conn,
            nonces,
            max_nonces,
            realm,
            five_tuple,
            m,
            CODE_UNAUTHORIZED,
            b"Unauthorized",
        )
        .await;
        return None;
    }

    prune_and_bound_nonces(nonces, max_nonces, false).await;

    let mut nonce_attr = Nonce::new(ATTR_NONCE, String::new());
    let mut realm_attr = Realm::new(ATTR_REALM, String::new());
    let mut username_attr = Username::new(ATTR_USERNAME, String::new());
    if nonce_attr.get_from(m).is_err()
        || realm_attr.get_from(m).is_err()
        || username_attr.get_from(m).is_err()
    {
        send_unauthenticated_error(conn, src_addr, m, CODE_BAD_REQUEST, b"Bad Request").await;
        return None;
    }

    let nonce_valid = {
        let mut guard = nonces.lock().await;
        match guard.get(&nonce_attr.text).copied() {
            Some((owner_tuple, created_at)) if owner_tuple == five_tuple => {
                if Instant::now()
                    .checked_duration_since(created_at)
                    .unwrap_or_default()
                    < NONCE_LIFETIME
                {
                    true
                } else {
                    guard.remove(&nonce_attr.text);
                    false
                }
            }
            _ => false,
        }
    };
    if !nonce_valid {
        issue_nonce_challenge(
            conn,
            nonces,
            max_nonces,
            realm,
            five_tuple,
            m,
            CODE_STALE_NONCE,
            b"Stale Nonce",
        )
        .await;
        return None;
    }

    let Ok(key) = auth_handler.auth_handle(&username_attr.text, &realm_attr.text, src_addr) else {
        send_unauthenticated_error(conn, src_addr, m, CODE_UNAUTHORIZED, b"Unauthorized").await;
        return None;
    };
    if MessageIntegrity(key.clone()).check(&mut m.clone()).is_err() {
        send_unauthenticated_error(conn, src_addr, m, CODE_UNAUTHORIZED, b"Unauthorized").await;
        return None;
    }

    Some((username_attr, key))
}

async fn send_authenticated_error(
    conn: &Arc<dyn Conn + Send + Sync>,
    dst: SocketAddr,
    m: &Message,
    code: ErrorCode,
    reason: &[u8],
    unknown_attrs: Option<Vec<AttrType>>,
    mi_key: Vec<u8>,
) {
    let built = {
        let mut err_msg = Message::new();
        let msg_type = MessageType::new(m.typ.method, CLASS_ERROR_RESPONSE);
        let err_attr = ErrorCodeAttribute {
            code,
            reason: reason.to_vec(),
        };
        let mut setters: Vec<Box<dyn Setter>> = vec![
            Box::new(Message {
                transaction_id: m.transaction_id,
                ..Default::default()
            }),
            Box::new(msg_type),
            Box::new(err_attr),
        ];
        if let Some(unknown) = unknown_attrs {
            setters.push(Box::new(UnknownAttributes(unknown)));
        }
        setters.push(Box::new(MessageIntegrity(mi_key)));
        setters.push(Box::new(FINGERPRINT));
        err_msg.build(&setters).is_ok().then_some(err_msg.raw)
    };
    if let Some(raw) = built {
        let _ = conn.send_to(&raw, dst).await;
    }
}

async fn send_forbidden_error(
    conn: &Arc<dyn Conn + Send + Sync>,
    dst: SocketAddr,
    m: &Message,
    mi_key: Vec<u8>,
) {
    send_authenticated_error(
        conn,
        dst,
        m,
        CODE_FORBIDDEN,
        b"Peer Address Forbidden",
        None,
        mi_key,
    )
    .await;
}

const fn relay_bind_ip_is_ipv4(relay: &RelayAddressConfig) -> bool {
    match *relay {
        RelayAddressConfig::None { bind_ip }
        | RelayAddressConfig::Static { bind_ip, .. }
        | RelayAddressConfig::Range { bind_ip, .. } => bind_ip.is_ipv4(),
    }
}

const fn relay_bind_ip(relay: &RelayAddressConfig) -> IpAddr {
    match *relay {
        RelayAddressConfig::None { bind_ip }
        | RelayAddressConfig::Static { bind_ip, .. }
        | RelayAddressConfig::Range { bind_ip, .. } => bind_ip,
    }
}

fn clamp_allocation_lifetime(requested: Duration) -> Duration {
    requested
        .min(MAXIMUM_ALLOCATION_LIFETIME)
        .max(DEFAULT_LIFETIME)
}

async fn close_alloc_ids(
    bridge: &Arc<Mutex<ProductionBridge>>,
    handles: &Arc<Mutex<HashMap<u64, Arc<ManagedAllocation>>>>,
    ids: Vec<u64>,
) {
    if ids.is_empty() {
        return;
    }
    let mut closing = Vec::with_capacity(ids.len());
    {
        let mut guard = handles.lock().await;
        for id in ids {
            let maybe_alloc = guard.remove(&id);
            if let Some(ref alloc) = maybe_alloc {
                alloc.begin_close();
            }
            closing.push((id, maybe_alloc));
        }
    }
    let bridge = Arc::clone(bridge);
    let _ = tokio::spawn(async move {
        for (id, maybe_alloc) in closing {
            if let Some(alloc) = maybe_alloc {
                alloc.finish_close().await;
            }
            bridge.lock().await.finish_teardown(id);
        }
    })
    .await;
}

fn build_authenticated_error_raw(
    m: &Message,
    code: ErrorCode,
    reason: &[u8],
    unknown_attrs: Option<Vec<AttrType>>,
    mi_key: Vec<u8>,
) -> Option<Vec<u8>> {
    let mut err_msg = Message::new();
    let msg_type = MessageType::new(m.typ.method, CLASS_ERROR_RESPONSE);
    let err_attr = ErrorCodeAttribute {
        code,
        reason: reason.to_vec(),
    };
    let mut setters: Vec<Box<dyn Setter>> = vec![
        Box::new(Message {
            transaction_id: m.transaction_id,
            ..Default::default()
        }),
        Box::new(msg_type),
        Box::new(err_attr),
    ];
    if let Some(unknown) = unknown_attrs {
        setters.push(Box::new(UnknownAttributes(unknown)));
    }
    setters.push(Box::new(MessageIntegrity(mi_key)));
    setters.push(Box::new(FINGERPRINT));
    err_msg.build(&setters).is_ok().then_some(err_msg.raw)
}

struct CompletedDeletionEntry {
    username: String,
    transaction_id: TransactionId,
    response_raw: Vec<u8>,
    completed_at: Instant,
}

async fn evict_nonces_up_to(
    nonces: &Mutex<HashMap<String, (FiveTuple, Instant)>>,
    five_tuple: FiveTuple,
    cutoff: Instant,
) {
    let mut guard = nonces.lock().await;
    guard.retain(|_, (owner_tuple, created_at)| *owner_tuple != five_tuple || *created_at > cutoff);
}

async fn prune_completed_deletions(
    completed_deletions: &Mutex<HashMap<FiveTuple, VecDeque<CompletedDeletionEntry>>>,
    nonces: &Mutex<HashMap<String, (FiveTuple, Instant)>>,
    max_tuples: usize,
) {
    let now = Instant::now();
    let evicted_cutoffs = {
        let mut guard = completed_deletions.lock().await;
        let mut evicted_cutoffs: Vec<(FiveTuple, Instant)> = Vec::new();
        guard.retain(|tuple, entries| {
            while let Some(front) = entries.front() {
                if now
                    .checked_duration_since(front.completed_at)
                    .unwrap_or_default()
                    >= NONCE_LIFETIME
                {
                    if let Some(evicted) = entries.pop_front() {
                        evicted_cutoffs.push((*tuple, evicted.completed_at));
                    }
                } else {
                    break;
                }
            }
            !entries.is_empty()
        });
        while guard.len() > max_tuples {
            let Some(oldest_tuple) = guard
                .iter()
                .min_by_key(|(_, entries)| {
                    entries.back().map_or_else(Instant::now, |e| e.completed_at)
                })
                .map(|(t, _)| *t)
            else {
                break;
            };
            if let Some(entries) = guard.remove(&oldest_tuple) {
                if let Some(last) = entries.back() {
                    evicted_cutoffs.push((oldest_tuple, last.completed_at));
                }
            }
        }
        evicted_cutoffs
    };
    for (tuple, cutoff) in evicted_cutoffs {
        evict_nonces_up_to(nonces, tuple, cutoff).await;
    }
}

async fn find_completed_deletion(
    completed_deletions: &Mutex<HashMap<FiveTuple, VecDeque<CompletedDeletionEntry>>>,
    nonces: &Mutex<HashMap<String, (FiveTuple, Instant)>>,
    five_tuple: &FiveTuple,
    username: &str,
    transaction_id: TransactionId,
    max_tuples: usize,
) -> Option<Vec<u8>> {
    prune_completed_deletions(completed_deletions, nonces, max_tuples).await;
    let guard = completed_deletions.lock().await;
    let entries = guard.get(five_tuple)?;
    entries
        .iter()
        .find(|e| e.username == username && e.transaction_id == transaction_id)
        .map(|e| e.response_raw.clone())
}

async fn record_completed_deletion(
    completed_deletions: &Mutex<HashMap<FiveTuple, VecDeque<CompletedDeletionEntry>>>,
    nonces: &Mutex<HashMap<String, (FiveTuple, Instant)>>,
    five_tuple: FiveTuple,
    username: String,
    transaction_id: TransactionId,
    response_raw: Vec<u8>,
    max_tuples: usize,
) {
    prune_completed_deletions(completed_deletions, nonces, max_tuples.saturating_sub(1)).await;
    let evicted_cutoffs = {
        let mut guard = completed_deletions.lock().await;
        let entries = guard.entry(five_tuple).or_default();
        let mut evicted_cutoffs = Vec::new();
        while entries.len() >= MAX_DELETION_TXS_PER_TUPLE {
            if let Some(evicted) = entries.pop_front() {
                evicted_cutoffs.push(evicted.completed_at);
            }
        }
        entries.push_back(CompletedDeletionEntry {
            username,
            transaction_id,
            response_raw,
            completed_at: Instant::now(),
        });
        evicted_cutoffs
    };
    for cutoff in evicted_cutoffs {
        evict_nonces_up_to(nonces, five_tuple, cutoff).await;
    }
}

fn is_zero_lifetime_refresh(msg: &Message) -> bool {
    if msg.typ.method != METHOD_REFRESH || !msg.contains(ATTR_LIFETIME) {
        return false;
    }
    let mut lt = Lifetime::default();
    lt.get_from(msg).is_ok() && lt.0.is_zero()
}

async fn get_active_allocation(
    bridge: &Arc<Mutex<ProductionBridge>>,
    handles: &Arc<Mutex<HashMap<u64, Arc<ManagedAllocation>>>>,
    clock: &SharedClock,
    five_tuple: &FiveTuple,
) -> Option<Arc<ManagedAllocation>> {
    let now_ms = clock.now_ms();
    let ord_tuple = OrderedFiveTuple::from(*five_tuple);
    let (expired_id, live_id) = bridge.lock().await.active_allocation_id(&ord_tuple, now_ms);
    if let Some(exp_id) = expired_id {
        close_alloc_ids(bridge, handles, vec![exp_id]).await;
    }
    let alloc_id = live_id?;
    handles.lock().await.get(&alloc_id).cloned()
}

async fn try_expire_allocation_if_current(
    bridge: &Arc<Mutex<ProductionBridge>>,
    handles: &Arc<Mutex<HashMap<u64, Arc<ManagedAllocation>>>>,
    clock: &SharedClock,
    alloc: &Arc<ManagedAllocation>,
) -> bool {
    let outcome =
        bridge
            .lock()
            .await
            .expire_if_due(&alloc.ord_tuple, alloc.alloc_id, clock.now_ms());
    match outcome {
        BridgeExpireOutcome::AlreadyRemoved => true,
        BridgeExpireOutcome::StillLive { expires_at_ms } => {
            let now_ms = clock.now_ms();
            let rem_ms = u64::try_from(expires_at_ms.saturating_sub(now_ms)).unwrap_or(u64::MAX);
            alloc.refresh_wall_deadline(Duration::from_millis(rem_ms.max(2)));
            false
        }
        BridgeExpireOutcome::Expired { close_alloc_id } => {
            close_alloc_ids(bridge, handles, vec![close_alloc_id]).await;
            true
        }
    }
}

async fn remove_allocation_if_current(
    bridge: &Arc<Mutex<ProductionBridge>>,
    handles: &Arc<Mutex<HashMap<u64, Arc<ManagedAllocation>>>>,
    alloc: &Arc<ManagedAllocation>,
) {
    let removed_id = bridge
        .lock()
        .await
        .remove_if_current(&alloc.ord_tuple, alloc.alloc_id);
    if let Some(id) = removed_id {
        close_alloc_ids(bridge, handles, vec![id]).await;
    }
}

async fn relay_packet_handler(
    alloc: Arc<ManagedAllocation>,
    bridge: Arc<Mutex<ProductionBridge>>,
    handles: Arc<Mutex<HashMap<u64, Arc<ManagedAllocation>>>>,
    clock: SharedClock,
    turn_socket: Arc<dyn Conn + Send + Sync>,
    client_addr: SocketAddr,
) {
    let mut buffer = vec![0u8; INBOUND_MTU];
    let mut expiry_rx = alloc.expiry_tx.subscribe();
    loop {
        let deadline = *expiry_rx.borrow_and_update();
        if deadline <= Instant::now() {
            if try_expire_allocation_if_current(&bridge, &handles, &clock, &alloc).await {
                return;
            }
            continue;
        }
        let sleep = tokio::time::sleep_until(deadline + Duration::from_millis(2));
        tokio::pin!(sleep);

        tokio::select! {
            _ = &mut sleep => {
                if try_expire_allocation_if_current(&bridge, &handles, &clock, &alloc).await {
                    return;
                }
            }
            changed = expiry_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            recv_res = alloc.recv_payload_from_peer(&mut buffer) => {
                let Ok((n, src_addr)) = recv_res else {
                    break;
                };
                let now_ms = clock.now_ms();
                let forwarding_decision = {
                    let mut guard = bridge.lock().await;
                    match guard.expire_if_due(&alloc.ord_tuple, alloc.alloc_id, now_ms) {
                        BridgeExpireOutcome::AlreadyRemoved => Err(None),
                        BridgeExpireOutcome::Expired { close_alloc_id } => Err(Some(close_alloc_id)),
                        BridgeExpireOutcome::StillLive { .. } => {
                            // RFC 5766 Section 10.3: verify that an active permission exists for
                            // the peer's IP before forwarding via ChannelData or DataIndication.
                            Ok(guard.inbound_forwarding_channel(
                                &alloc.ord_tuple,
                                alloc.alloc_id,
                                src_addr,
                                now_ms,
                            ))
                        }
                    }
                };
                let maybe_channel = match forwarding_decision {
                    Err(Some(close_id)) => {
                        close_alloc_ids(&bridge, &handles, vec![close_id]).await;
                        return;
                    }
                    Err(None) => return,
                    Ok(None) => continue,
                    Ok(Some(ch)) => ch,
                };
                let Some(payload) = buffer.get(..n) else {
                    continue;
                };
                if let Some(number) = maybe_channel {
                    let mut channel_data = ChannelData {
                        data: payload.to_vec(),
                        number: ChannelNumber(number),
                        raw: Vec::new(),
                    };
                    channel_data.encode();
                    let _ = turn_socket.send_to(&channel_data.raw, client_addr).await;
                } else {
                    let peer_address_attr = PeerAddress {
                        ip: src_addr.ip(),
                        port: src_addr.port(),
                    };
                    let data_attr = Data(payload.to_vec());
                    let mut msg = Message::new();
                    if msg
                        .build(&[
                            Box::new(TransactionId::new()),
                            Box::new(MessageType::new(METHOD_DATA, CLASS_INDICATION)),
                            Box::new(peer_address_attr),
                            Box::new(data_attr),
                        ])
                        .is_ok()
                    {
                        let _ = turn_socket.send_to(&msg.raw, client_addr).await;
                    }
                }
            }
        }
    }
    remove_allocation_if_current(&bridge, &handles, &alloc).await;
}

#[allow(clippy::too_many_arguments)]
async fn run_pending_allocate_open(
    open_id: u64,
    five_tuple: FiveTuple,
    ord_tuple: OrderedFiveTuple,
    tx_id: TransactionId,
    username: String,
    use_ipv4: bool,
    need_even_port: bool,
    lifetime_duration: Duration,
    mi_key: Vec<u8>,
    relay_turn_socket: Arc<dyn Conn + Send + Sync>,
    tcp_authenticated_tx: Option<watch::Sender<Option<u64>>>,
    had_ticket: bool,
    m_for_err: Message,
    relay_allocator: Arc<dyn TurnDatagramRelayAllocator>,
    bridge: Arc<Mutex<ProductionBridge>>,
    handles: Arc<Mutex<HashMap<u64, Arc<ManagedAllocation>>>>,
    pending_waiters: Arc<Mutex<HashMap<u64, PendingOpenWaiter>>>,
    clock: SharedClock,
    mut shutdown_rx: watch::Receiver<bool>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let alloc_res = tokio::select! {
        res = relay_allocator.allocate_relay(use_ipv4, need_even_port) => Some(res),
        _ = shutdown_rx.changed() => None,
        _ = cancel_rx.changed() => None,
    };

    let Some(alloc_res) = alloc_res else {
        pending_waiters.lock().await.remove(&open_id);
        let _ = bridge
            .lock()
            .await
            .complete_allocate_err(ord_tuple, open_id, None, clock.now_ms());
        return;
    };

    match alloc_res {
        Err(_) => {
            let err_raw = build_authenticated_error_raw(
                &m_for_err,
                CODE_INSUFFICIENT_CAPACITY,
                b"Insufficient Capacity",
                None,
                mi_key,
            );
            let cache_entry = if had_ticket {
                err_raw
                    .as_ref()
                    .map(|raw| (raw.clone(), BRIDGE_FAILED_ALLOCATE_CACHE_TTL_MS))
            } else {
                None
            };
            let should_send = bridge.lock().await.complete_allocate_err(
                ord_tuple,
                open_id,
                cache_entry,
                clock.now_ms(),
            );
            let waiter = pending_waiters.lock().await.remove(&open_id);
            if should_send {
                if let (Some(raw), Some(w)) = (err_raw, waiter) {
                    let _ = w.conn.send_to(&raw, w.src_addr).await;
                }
            }
        }
        Ok((relay_conn, relay_addr)) => {
            let waiter = pending_waiters.lock().await.remove(&open_id);
            let Some(waiter) = waiter else {
                relay_conn.begin_close();
                let _ = relay_conn.close().await;
                bridge.lock().await.finish_teardown(open_id);
                return;
            };
            let src_addr = waiter.src_addr;
            let built = {
                let mut resp = Message::new();
                let setters: Vec<Box<dyn Setter>> = vec![
                    Box::new(Message {
                        transaction_id: tx_id,
                        ..Default::default()
                    }),
                    Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_SUCCESS_RESPONSE)),
                    Box::new(RelayedAddress {
                        ip: relay_addr.ip(),
                        port: relay_addr.port(),
                    }),
                    Box::new(Lifetime(lifetime_duration)),
                    Box::new(XorMappedAddress {
                        ip: src_addr.ip(),
                        port: src_addr.port(),
                    }),
                    Box::new(MessageIntegrity(mi_key)),
                    Box::new(FINGERPRINT),
                ];
                resp.build(&setters).is_ok().then_some(resp.raw)
            };
            let Some(resp_raw) = built else {
                relay_conn.begin_close();
                let _ = relay_conn.close().await;
                let _ = bridge.lock().await.complete_allocate_err(
                    ord_tuple,
                    open_id,
                    None,
                    clock.now_ms(),
                );
                return;
            };

            let decision = {
                let mut b_guard = bridge.lock().await;
                if *shutdown_rx.borrow() || *cancel_rx.borrow() {
                    let _ = b_guard.disconnect_tuple(&ord_tuple);
                }
                let decision = b_guard.complete_allocate_ok(
                    ord_tuple,
                    open_id,
                    relay_addr,
                    resp_raw.clone(),
                    clock.now_ms(),
                );
                if let BridgeCompleteAllocate::Installed { alloc_id } = decision {
                    let alloc = ManagedAllocation::new(
                        alloc_id,
                        five_tuple,
                        username,
                        relay_addr,
                        Arc::clone(&relay_conn),
                        lifetime_duration,
                        tcp_authenticated_tx,
                    );
                    handles.lock().await.insert(alloc_id, Arc::clone(&alloc));
                    tokio::spawn(relay_packet_handler(
                        alloc,
                        Arc::clone(&bridge),
                        Arc::clone(&handles),
                        Arc::clone(&clock),
                        relay_turn_socket,
                        src_addr,
                    ));
                }
                decision
            };

            match decision {
                BridgeCompleteAllocate::AbortAndCloseRelay { closing_id } => {
                    relay_conn.begin_close();
                    let _ = relay_conn.close().await;
                    bridge.lock().await.finish_teardown(closing_id);
                }
                BridgeCompleteAllocate::Installed { .. } => {
                    let _ = waiter.conn.send_to(&resp_raw, src_addr).await;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_authenticated_allocate(
    conn: &Arc<dyn Conn + Send + Sync>,
    bridge: &Arc<Mutex<ProductionBridge>>,
    handles: &Arc<Mutex<HashMap<u64, Arc<ManagedAllocation>>>>,
    pending_waiters: &Arc<Mutex<HashMap<u64, PendingOpenWaiter>>>,
    pending_tasks: &mut JoinSet<()>,
    clock: &SharedClock,
    shutdown_rx: &watch::Receiver<bool>,
    relay_allocator: &Arc<dyn TurnDatagramRelayAllocator>,
    five_tuple: FiveTuple,
    src_addr: SocketAddr,
    m: &Message,
    username_attr: Username,
    mi_key: Vec<u8>,
) {
    let ord_tuple = OrderedFiveTuple::from(five_tuple);

    // First check whether this request is a retransmission or mismatch on an active, in-flight,
    // or cached allocation BEFORE validating Allocate-specific attributes or consuming a ticket.
    let (expired_id, existing) = bridge.lock().await.check_existing_allocate(
        &ord_tuple,
        &username_attr.text,
        &m.transaction_id,
        clock.now_ms(),
    );
    if let Some(exp_id) = expired_id {
        close_alloc_ids(bridge, handles, vec![exp_id]).await;
    }
    match existing {
        BridgeExistingAllocate::ShuttingDown => return,
        BridgeExistingAllocate::ReplayCached(cached_raw) => {
            let _ = conn.send_to(&cached_raw, src_addr).await;
            return;
        }
        BridgeExistingAllocate::CoalescedInFlight { open_id } => {
            if let Some(waiter) = pending_waiters.lock().await.get_mut(&open_id) {
                waiter.conn = Arc::clone(conn);
                waiter.src_addr = src_addr;
            }
            return;
        }
        BridgeExistingAllocate::AllocationMismatch => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_ALLOC_MISMATCH,
                b"Allocation Mismatch",
                None,
                mi_key,
            )
            .await;
            return;
        }
        BridgeExistingAllocate::None => {}
    }
    let mut requested_transport = RequestedTransport::default();
    if requested_transport.get_from(m).is_err() {
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_BAD_REQUEST,
            b"Missing or Malformed Requested-Transport",
            None,
            mi_key,
        )
        .await;
        return;
    }
    if requested_transport.protocol != PROTO_UDP {
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_UNSUPPORTED_TRANS_PROTO,
            b"Unsupported Transport Protocol",
            None,
            mi_key,
        )
        .await;
        return;
    }

    if m.contains(ATTR_DONT_FRAGMENT) {
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_UNKNOWN_ATTRIBUTE,
            b"Unknown Attribute",
            Some(vec![ATTR_DONT_FRAGMENT]),
            mi_key,
        )
        .await;
        return;
    }

    if m.contains(ATTR_RESERVATION_TOKEN) {
        let mut reservation_token_attr = ReservationToken::default();
        if reservation_token_attr.get_from(m).is_err()
            || m.contains(ATTR_EVEN_PORT)
            || m.contains(ATTR_REQUESTED_ADDRESS_FAMILY)
        {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_BAD_REQUEST,
                b"Invalid Reservation-Token Request",
                None,
                mi_key,
            )
            .await;
            return;
        }
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_INSUFFICIENT_CAPACITY,
            b"Port Reservation Not Supported",
            None,
            mi_key,
        )
        .await;
        return;
    }

    // RFC 6156 Section 4.1: If REQUESTED-ADDRESS-FAMILY is absent, the default requested relay
    // family is IPv4; if the relay cannot provide that family, return 440.
    let mut requested_ipv4 = true;
    if m.contains(ATTR_REQUESTED_ADDRESS_FAMILY) {
        let mut req_family = RequestedAddressFamily::default();
        match req_family.get_from(m) {
            Err(stun::Error::Other(_)) => {
                send_authenticated_error(
                    conn,
                    src_addr,
                    m,
                    CODE_ADDR_FAMILY_NOT_SUPPORTED,
                    b"Address Family Not Supported",
                    None,
                    mi_key,
                )
                .await;
                return;
            }
            Err(_) => {
                send_authenticated_error(
                    conn,
                    src_addr,
                    m,
                    CODE_BAD_REQUEST,
                    b"Malformed Requested-Address-Family",
                    None,
                    mi_key,
                )
                .await;
                return;
            }
            Ok(()) => {
                requested_ipv4 = req_family == REQUESTED_FAMILY_IPV4;
            }
        }
    }
    if requested_ipv4 != relay_allocator.is_ipv4() {
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_ADDR_FAMILY_NOT_SUPPORTED,
            b"Address Family Not Supported",
            None,
            mi_key,
        )
        .await;
        return;
    }
    let use_ipv4 = requested_ipv4;

    let mut need_even_port = false;
    if m.contains(ATTR_EVEN_PORT) {
        let mut even_port = EvenPort::default();
        if even_port.get_from(m).is_err() {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_BAD_REQUEST,
                b"Malformed Even-Port Attribute",
                None,
                mi_key,
            )
            .await;
            return;
        }
        if even_port != EvenPort::default() {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_INSUFFICIENT_CAPACITY,
                b"Paired Port Reservation Not Supported",
                None,
                mi_key,
            )
            .await;
            return;
        }
        need_even_port = true;
    }

    let mut lifetime_duration = DEFAULT_LIFETIME;
    if m.contains(ATTR_LIFETIME) {
        let mut lifetime = Lifetime::default();
        if lifetime.get_from(m).is_err() {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_BAD_REQUEST,
                b"Malformed Lifetime Attribute",
                None,
                mi_key,
            )
            .await;
            return;
        }
        lifetime_duration = clamp_allocation_lifetime(lifetime.0);
    }

    let decoded_identity = TurnCredentials::decode_username_parts(&username_attr.text);

    let tcp_peer_ctx = conn
        .as_any()
        .downcast_ref::<RecordingConn>()
        .and_then(|rc| rc.tcp_peer_context(src_addr));
    let relay_turn_socket: Arc<dyn Conn + Send + Sync> = match (
        conn.as_any().downcast_ref::<RecordingConn>(),
        tcp_peer_ctx.as_ref().map(|(cid, _)| *cid),
    ) {
        (Some(rc), Some(conn_id)) => Arc::new(ScopedTcpPeerConn {
            recording: Arc::new(RecordingConn {
                inner: SyncMutex::new(rc.clone_inner().ok()),
                local_addr: rc.local_addr,
                protocol: rc.protocol,
            }),
            conn_id,
        }),
        (Some(rc), None) if rc.protocol == PROTO_TCP => {
            // The TCP peer already disconnected before this queued Allocate request was reached.
            return;
        }
        _ => Arc::clone(conn),
    };
    let tcp_authenticated_tx = tcp_peer_ctx.map(|(_, tx)| tx);

    let ticket_id = match decoded_identity {
        Ok((TurnAuthIdentity::SessionTicket(signed_ticket), ..)) => {
            Some(signed_ticket.ticket.session_id)
        }
        _ => None,
    };

    // Complete teardown of any already-expired allocations BEFORE checking capacity so an expired
    // allocation on another tuple never causes a spurious `508` or consumes a fresh ticket.
    let admit_now_ms = loop {
        let candidate_now = clock.now_ms();
        let pre_expired = bridge.lock().await.sweep_expired_allocations(candidate_now);
        if pre_expired.is_empty() {
            break candidate_now;
        }
        close_alloc_ids(bridge, handles, pre_expired).await;
    };

    let (expired_ids, start_decision) = {
        let mut guard = bridge.lock().await;
        if *shutdown_rx.borrow() {
            return;
        }
        guard.begin_allocate(
            ord_tuple,
            username_attr.text.clone(),
            m.transaction_id,
            ticket_id,
            lifetime_duration.as_millis(),
            admit_now_ms,
        )
    };
    if !expired_ids.is_empty() {
        close_alloc_ids(bridge, handles, expired_ids).await;
    }

    match start_decision {
        BridgeAllocateStart::ShuttingDown => {}
        BridgeAllocateStart::ReplayCached(cached_raw) => {
            let _ = conn.send_to(&cached_raw, src_addr).await;
        }
        BridgeAllocateStart::CoalescedInFlight { open_id } => {
            if let Some(waiter) = pending_waiters.lock().await.get_mut(&open_id) {
                waiter.conn = Arc::clone(conn);
                waiter.src_addr = src_addr;
            }
        }
        BridgeAllocateStart::AllocationMismatch => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_ALLOC_MISMATCH,
                b"Allocation Mismatch",
                None,
                mi_key,
            )
            .await;
        }
        BridgeAllocateStart::TicketUnknownOrConsumed => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_UNAUTHORIZED,
                b"Session Ticket Unknown or Already Consumed",
                None,
                mi_key,
            )
            .await;
        }
        BridgeAllocateStart::CapacityReached {
            cache_for_retransmit,
        } => {
            let err_raw = build_authenticated_error_raw(
                m,
                CODE_INSUFFICIENT_CAPACITY,
                b"Allocation Capacity Reached",
                None,
                mi_key,
            );
            if let Some(raw) = err_raw {
                if cache_for_retransmit {
                    let now_after = clock.now_ms();
                    bridge.lock().await.cache_allocate_response(
                        ord_tuple,
                        username_attr.text,
                        m.transaction_id,
                        raw.clone(),
                        now_after.saturating_add(BRIDGE_FAILED_ALLOCATE_CACHE_TTL_MS),
                        now_after,
                    );
                }
                let _ = conn.send_to(&raw, src_addr).await;
            }
        }
        BridgeAllocateStart::StartBackendOpen { open_id } => {
            let (cancel_tx, cancel_rx) = watch::channel(false);
            pending_waiters
                .lock()
                .await
                .insert(open_id, PendingOpenWaiter {
                    conn: Arc::clone(conn),
                    src_addr,
                    cancel_tx,
                });
            pending_tasks.spawn(run_pending_allocate_open(
                open_id,
                five_tuple,
                ord_tuple,
                m.transaction_id,
                username_attr.text,
                use_ipv4,
                need_even_port,
                lifetime_duration,
                mi_key,
                relay_turn_socket,
                tcp_authenticated_tx,
                ticket_id.is_some(),
                m.clone(),
                Arc::clone(relay_allocator),
                Arc::clone(bridge),
                Arc::clone(handles),
                Arc::clone(pending_waiters),
                Arc::clone(clock),
                shutdown_rx.clone(),
                cancel_rx,
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_authenticated_refresh(
    conn: &Arc<dyn Conn + Send + Sync>,
    bridge: &Arc<Mutex<ProductionBridge>>,
    handles: &Arc<Mutex<HashMap<u64, Arc<ManagedAllocation>>>>,
    clock: &SharedClock,
    nonces: &Mutex<HashMap<String, (FiveTuple, Instant)>>,
    completed_deletions: &Mutex<HashMap<FiveTuple, VecDeque<CompletedDeletionEntry>>>,
    max_allocations: usize,
    alloc: Arc<ManagedAllocation>,
    m: &Message,
    mi_key: Vec<u8>,
) {
    let src_addr = alloc.five_tuple.src_addr;
    let mut lifetime_duration = DEFAULT_LIFETIME;
    if m.contains(ATTR_LIFETIME) {
        let mut lifetime = Lifetime::default();
        if lifetime.get_from(m).is_err() {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_BAD_REQUEST,
                b"Malformed Lifetime Attribute",
                None,
                mi_key,
            )
            .await;
            return;
        }
        lifetime_duration = if lifetime.0.is_zero() {
            Duration::ZERO
        } else {
            clamp_allocation_lifetime(lifetime.0)
        };
    }

    let refresh_outcome = bridge.lock().await.refresh(
        &alloc.ord_tuple,
        alloc.alloc_id,
        lifetime_duration.as_millis(),
        clock.now_ms(),
    );
    match refresh_outcome {
        BridgeRefreshOutcome::Deleted { close_alloc_id } => {
            close_alloc_ids(bridge, handles, vec![close_alloc_id]).await;
        }
        BridgeRefreshOutcome::Expired { close_alloc_id } => {
            close_alloc_ids(bridge, handles, vec![close_alloc_id]).await;
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_ALLOC_MISMATCH,
                b"Allocation Mismatch",
                None,
                mi_key,
            )
            .await;
            return;
        }
        BridgeRefreshOutcome::NotFound => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_ALLOC_MISMATCH,
                b"Allocation Mismatch",
                None,
                mi_key,
            )
            .await;
            return;
        }
        BridgeRefreshOutcome::Refreshed { .. } => {
            alloc.refresh_wall_deadline(lifetime_duration);
        }
    }

    let built = {
        let mut resp = Message::new();
        let setters: Vec<Box<dyn Setter>> = vec![
            Box::new(Message {
                transaction_id: m.transaction_id,
                ..Default::default()
            }),
            Box::new(MessageType::new(METHOD_REFRESH, CLASS_SUCCESS_RESPONSE)),
            Box::new(Lifetime(lifetime_duration)),
            Box::new(MessageIntegrity(mi_key)),
            Box::new(FINGERPRINT),
        ];
        resp.build(&setters).is_ok().then_some(resp.raw)
    };
    if let Some(raw) = built {
        if lifetime_duration.is_zero() {
            record_completed_deletion(
                completed_deletions,
                nonces,
                alloc.five_tuple,
                alloc.username.clone(),
                m.transaction_id,
                raw.clone(),
                max_allocations,
            )
            .await;
        }
        let _ = conn.send_to(&raw, src_addr).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_authenticated_create_permission(
    conn: &Arc<dyn Conn + Send + Sync>,
    bridge: &Arc<Mutex<ProductionBridge>>,
    clock: &SharedClock,
    shutdown_rx: &watch::Receiver<bool>,
    pending_tasks: &mut JoinSet<()>,
    alloc: &Arc<ManagedAllocation>,
    m: &Message,
    peer_policy: RelayPeerPolicy,
    max_permissions_per_allocation: usize,
    mi_key: Vec<u8>,
) {
    let src_addr = alloc.five_tuple.src_addr;
    let relay_addr = alloc.relay_addr;

    let mut peer_addrs = Vec::new();
    let mut malformed = false;
    for attr in &m.attributes.0 {
        if attr.typ == ATTR_MESSAGE_INTEGRITY {
            break;
        }
        if attr.typ != ATTR_XOR_PEER_ADDRESS {
            continue;
        }
        let mut single = Message {
            transaction_id: m.transaction_id,
            ..Default::default()
        };
        single.attributes.0.push(attr.clone());
        let mut peer_addr = PeerAddress::default();
        if peer_addr.get_from(&single).is_err() {
            malformed = true;
            break;
        }
        peer_addrs.push(peer_addr);
    }

    if malformed || peer_addrs.is_empty() {
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_BAD_REQUEST,
            b"Missing or Malformed XOR-PEER-ADDRESS",
            None,
            mi_key,
        )
        .await;
        return;
    }

    let mut candidate_ips = Vec::with_capacity(peer_addrs.len());
    let mut unique_ips = HashSet::with_capacity(peer_addrs.len());
    for peer_addr in &peer_addrs {
        if peer_addr.ip.is_ipv4() != relay_addr.is_ipv4() {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_PEER_ADDR_FAMILY_MISMATCH,
                b"Peer Address Family Mismatch",
                None,
                mi_key,
            )
            .await;
            return;
        }
        if !peer_policy.is_allowed_ip(peer_addr.ip) {
            send_forbidden_error(conn, src_addr, m, mi_key).await;
            return;
        }
        if unique_ips.insert(peer_addr.ip) {
            candidate_ips.push(peer_addr.ip);
        }
    }

    let precheck = bridge.lock().await.can_install_permissions(
        &alloc.ord_tuple,
        alloc.alloc_id,
        &candidate_ips,
        max_permissions_per_allocation,
        clock.now_ms(),
    );
    match precheck {
        BridgePermissionCheck::Permitted => {}
        BridgePermissionCheck::AllocationGone => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_ALLOC_MISMATCH,
                b"Allocation Mismatch",
                None,
                mi_key,
            )
            .await;
            return;
        }
        BridgePermissionCheck::QuotaExceeded => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_INSUFFICIENT_CAPACITY,
                b"Permission Quota Reached",
                None,
                mi_key,
            )
            .await;
            return;
        }
    }

    let Some(control_permit) = alloc.try_acquire_control_permit() else {
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_INSUFFICIENT_CAPACITY,
            b"Too Many Pending Control Requests",
            None,
            mi_key,
        )
        .await;
        return;
    };

    let conn = Arc::clone(conn);
    let bridge = Arc::clone(bridge);
    let clock = Arc::clone(clock);
    let alloc = Arc::clone(alloc);
    let m = m.clone();
    let mut shutdown_rx = shutdown_rx.clone();
    pending_tasks.spawn(async move {
        let _permit = control_permit;
        let _op_guard = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => return,
            _ = alloc.wait_closed() => return,
            guard = alloc.control_op_lock.lock() => guard,
        };

        let recheck = bridge.lock().await.can_install_permissions(
            &alloc.ord_tuple,
            alloc.alloc_id,
            &candidate_ips,
            max_permissions_per_allocation,
            clock.now_ms(),
        );
        match recheck {
            BridgePermissionCheck::Permitted => {}
            BridgePermissionCheck::AllocationGone => {
                send_authenticated_error(
                    &conn,
                    src_addr,
                    &m,
                    CODE_ALLOC_MISMATCH,
                    b"Allocation Mismatch",
                    None,
                    mi_key,
                )
                .await;
                return;
            }
            BridgePermissionCheck::QuotaExceeded => {
                send_authenticated_error(
                    &conn,
                    src_addr,
                    &m,
                    CODE_INSUFFICIENT_CAPACITY,
                    b"Permission Quota Reached",
                    None,
                    mi_key,
                )
                .await;
                return;
            }
        }

        for &ip in &candidate_ips {
            let pinhole_res = tokio::select! {
                biased;
                _ = shutdown_rx.changed() => None,
                _ = alloc.wait_closed() => None,
                res = alloc.relay_conn.allow_peer_ip(ip) => Some(res),
            };
            match pinhole_res {
                Some(Ok(())) => {}
                Some(Err(_)) => {
                    send_authenticated_error(
                        &conn,
                        src_addr,
                        &m,
                        CODE_INSUFFICIENT_CAPACITY,
                        b"Relay Backend Unavailable",
                        None,
                        mi_key,
                    )
                    .await;
                    return;
                }
                None => return,
            }
        }

        let committed = bridge.lock().await.commit_permissions(
            &alloc.ord_tuple,
            alloc.alloc_id,
            &candidate_ips,
            BRIDGE_PERMISSION_TTL_MS,
            max_permissions_per_allocation,
            clock.now_ms(),
        );
        match committed {
            BridgePermissionCheck::Permitted => {
                let built = {
                    let mut resp = Message::new();
                    let setters: Vec<Box<dyn Setter>> = vec![
                        Box::new(Message {
                            transaction_id: m.transaction_id,
                            ..Default::default()
                        }),
                        Box::new(MessageType::new(
                            METHOD_CREATE_PERMISSION,
                            CLASS_SUCCESS_RESPONSE,
                        )),
                        Box::new(MessageIntegrity(mi_key)),
                        Box::new(FINGERPRINT),
                    ];
                    resp.build(&setters).is_ok().then_some(resp.raw)
                };
                if let Some(raw) = built {
                    let _ = conn.send_to(&raw, src_addr).await;
                }
            }
            BridgePermissionCheck::AllocationGone => {
                send_authenticated_error(
                    &conn,
                    src_addr,
                    &m,
                    CODE_ALLOC_MISMATCH,
                    b"Allocation Mismatch",
                    None,
                    mi_key,
                )
                .await;
            }
            BridgePermissionCheck::QuotaExceeded => {
                send_authenticated_error(
                    &conn,
                    src_addr,
                    &m,
                    CODE_INSUFFICIENT_CAPACITY,
                    b"Permission Quota Reached",
                    None,
                    mi_key,
                )
                .await;
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn handle_authenticated_channel_bind(
    conn: &Arc<dyn Conn + Send + Sync>,
    bridge: &Arc<Mutex<ProductionBridge>>,
    clock: &SharedClock,
    shutdown_rx: &watch::Receiver<bool>,
    pending_tasks: &mut JoinSet<()>,
    alloc: &Arc<ManagedAllocation>,
    m: &Message,
    peer_policy: RelayPeerPolicy,
    channel_bind_timeout: Duration,
    max_permissions_per_allocation: usize,
    mi_key: Vec<u8>,
) {
    let src_addr = alloc.five_tuple.src_addr;
    let relay_addr = alloc.relay_addr;

    let mut channel = ChannelNumber::default();
    let mut peer_addr = PeerAddress::default();
    if channel.get_from(m).is_err() || !channel.valid() || peer_addr.get_from(m).is_err() {
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_BAD_REQUEST,
            b"Missing or Malformed ChannelBind Attribute",
            None,
            mi_key,
        )
        .await;
        return;
    }

    if peer_addr.ip.is_ipv4() != relay_addr.is_ipv4() {
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_PEER_ADDR_FAMILY_MISMATCH,
            b"Peer Address Family Mismatch",
            None,
            mi_key,
        )
        .await;
        return;
    }

    let target_sock = SocketAddr::new(peer_addr.ip, peer_addr.port);
    if !peer_policy.is_allowed_addr(target_sock) {
        send_forbidden_error(conn, src_addr, m, mi_key).await;
        return;
    }

    let precheck = bridge.lock().await.can_bind_channel(
        &alloc.ord_tuple,
        alloc.alloc_id,
        channel.0,
        target_sock,
        max_permissions_per_allocation,
        clock.now_ms(),
    );
    match precheck {
        Ok(()) => {}
        Err(BridgeChannelBindError::AllocationGone) => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_ALLOC_MISMATCH,
                b"Allocation Mismatch",
                None,
                mi_key,
            )
            .await;
            return;
        }
        Err(BridgeChannelBindError::Conflict) => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_BAD_REQUEST,
                b"ChannelBind Conflict",
                None,
                mi_key,
            )
            .await;
            return;
        }
        Err(BridgeChannelBindError::PermissionQuotaExceeded) => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_INSUFFICIENT_CAPACITY,
                b"Permission Quota Reached",
                None,
                mi_key,
            )
            .await;
            return;
        }
    }

    let Some(control_permit) = alloc.try_acquire_control_permit() else {
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_INSUFFICIENT_CAPACITY,
            b"Too Many Pending Control Requests",
            None,
            mi_key,
        )
        .await;
        return;
    };

    let conn = Arc::clone(conn);
    let bridge = Arc::clone(bridge);
    let clock = Arc::clone(clock);
    let alloc = Arc::clone(alloc);
    let m = m.clone();
    let mut shutdown_rx = shutdown_rx.clone();
    pending_tasks.spawn(async move {
        let _permit = control_permit;
        let _op_guard = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => return,
            _ = alloc.wait_closed() => return,
            guard = alloc.control_op_lock.lock() => guard,
        };

        let recheck = bridge.lock().await.can_bind_channel(
            &alloc.ord_tuple,
            alloc.alloc_id,
            channel.0,
            target_sock,
            max_permissions_per_allocation,
            clock.now_ms(),
        );
        match recheck {
            Ok(()) => {}
            Err(BridgeChannelBindError::AllocationGone) => {
                send_authenticated_error(
                    &conn,
                    src_addr,
                    &m,
                    CODE_ALLOC_MISMATCH,
                    b"Allocation Mismatch",
                    None,
                    mi_key,
                )
                .await;
                return;
            }
            Err(BridgeChannelBindError::Conflict) => {
                send_authenticated_error(
                    &conn,
                    src_addr,
                    &m,
                    CODE_BAD_REQUEST,
                    b"ChannelBind Conflict",
                    None,
                    mi_key,
                )
                .await;
                return;
            }
            Err(BridgeChannelBindError::PermissionQuotaExceeded) => {
                send_authenticated_error(
                    &conn,
                    src_addr,
                    &m,
                    CODE_INSUFFICIENT_CAPACITY,
                    b"Permission Quota Reached",
                    None,
                    mi_key,
                )
                .await;
                return;
            }
        }

        let pinhole_res = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => None,
            _ = alloc.wait_closed() => None,
            res = alloc.relay_conn.allow_peer_ip(target_sock.ip()) => Some(res),
        };
        match pinhole_res {
            Some(Ok(())) => {}
            Some(Err(_)) => {
                send_authenticated_error(
                    &conn,
                    src_addr,
                    &m,
                    CODE_INSUFFICIENT_CAPACITY,
                    b"Relay Backend Unavailable",
                    None,
                    mi_key,
                )
                .await;
                return;
            }
            None => return,
        }

        let committed = bridge.lock().await.commit_channel_bind(
            &alloc.ord_tuple,
            alloc.alloc_id,
            channel.0,
            target_sock,
            channel_bind_timeout.as_millis(),
            BRIDGE_PERMISSION_TTL_MS,
            max_permissions_per_allocation,
            clock.now_ms(),
        );
        match committed {
            Ok(()) => {
                let built = {
                    let mut resp = Message::new();
                    let setters: Vec<Box<dyn Setter>> = vec![
                        Box::new(Message {
                            transaction_id: m.transaction_id,
                            ..Default::default()
                        }),
                        Box::new(MessageType::new(
                            METHOD_CHANNEL_BIND,
                            CLASS_SUCCESS_RESPONSE,
                        )),
                        Box::new(MessageIntegrity(mi_key)),
                        Box::new(FINGERPRINT),
                    ];
                    resp.build(&setters).is_ok().then_some(resp.raw)
                };
                if let Some(raw) = built {
                    let _ = conn.send_to(&raw, src_addr).await;
                }
            }
            Err(BridgeChannelBindError::AllocationGone) => {
                send_authenticated_error(
                    &conn,
                    src_addr,
                    &m,
                    CODE_ALLOC_MISMATCH,
                    b"Allocation Mismatch",
                    None,
                    mi_key,
                )
                .await;
            }
            Err(BridgeChannelBindError::Conflict) => {
                send_authenticated_error(
                    &conn,
                    src_addr,
                    &m,
                    CODE_BAD_REQUEST,
                    b"ChannelBind Conflict",
                    None,
                    mi_key,
                )
                .await;
            }
            Err(BridgeChannelBindError::PermissionQuotaExceeded) => {
                send_authenticated_error(
                    &conn,
                    src_addr,
                    &m,
                    CODE_INSUFFICIENT_CAPACITY,
                    b"Permission Quota Reached",
                    None,
                    mi_key,
                )
                .await;
            }
        }
    });
}

fn collect_concrete_listener_candidates(
    use_ipv4: bool,
    extra_local_ips: &[IpAddr],
) -> Vec<SocketAddr> {
    let mut candidates = Vec::new();
    let loopback_ip = if use_ipv4 {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    };
    candidates.push(SocketAddr::new(loopback_ip, 0));

    for &ip in extra_local_ips {
        if ip.is_ipv4() == use_ipv4 && !ip.is_unspecified() && !ip.is_multicast() {
            let sa = SocketAddr::new(ip, 0);
            if !candidates.contains(&sa) {
                candidates.push(sa);
            }
        }
    }

    if let Ok(ifaces) = webrtc_util::ifaces::ifaces() {
        for iface in ifaces {
            if let Some(mut sa) = iface.addr {
                if sa.is_ipv4() == use_ipv4 && !sa.ip().is_unspecified() && !sa.ip().is_multicast()
                {
                    sa.set_port(0);
                    if !candidates.contains(&sa) {
                        candidates.push(sa);
                    }
                }
            }
        }
    }
    candidates
}

async fn bind_listener_sockets(
    listen_addr: SocketAddr,
    extra_local_ips: &[IpAddr],
    connection_semaphore: Arc<Semaphore>,
) -> Result<(SocketAddr, Vec<Arc<RecordingConn>>)> {
    if !listen_addr.ip().is_unspecified() {
        if listen_addr.port() != 0 {
            let socket = UdpSocket::bind(listen_addr).await?;
            let local_addr = socket.local_addr()?;
            let tcp_listener = TcpListener::bind(local_addr).await?;
            let udp_conn: Arc<dyn Conn + Send + Sync> = Arc::new(socket);
            let tcp_conn: Arc<dyn Conn + Send + Sync> =
                TcpTurnListenerConn::spawn(tcp_listener, local_addr, connection_semaphore);
            return Ok((local_addr, vec![
                Arc::new(RecordingConn::new(udp_conn, local_addr)),
                Arc::new(RecordingConn::new_tcp(tcp_conn, local_addr)),
            ]));
        }

        let mut last_err: Option<std::io::Error> = None;
        for _ in 0..DEFAULT_PORT_RANGE_MAX_RETRIES {
            let socket = UdpSocket::bind(listen_addr).await?;
            let local_addr = socket.local_addr()?;
            match TcpListener::bind(local_addr).await {
                Ok(tcp_listener) => {
                    let udp_conn: Arc<dyn Conn + Send + Sync> = Arc::new(socket);
                    let tcp_conn: Arc<dyn Conn + Send + Sync> =
                        TcpTurnListenerConn::spawn(tcp_listener, local_addr, connection_semaphore);
                    return Ok((local_addr, vec![
                        Arc::new(RecordingConn::new(udp_conn, local_addr)),
                        Arc::new(RecordingConn::new_tcp(tcp_conn, local_addr)),
                    ]));
                }
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                    last_err = Some(err);
                    continue;
                }
                Err(err) => return Err(err.into()),
            }
        }
        return Err(last_err
            .unwrap_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "failed to bind matching UDP and TCP TURN listener port",
                )
            })
            .into());
    }

    let candidates = collect_concrete_listener_candidates(listen_addr.is_ipv4(), extra_local_ips);
    if listen_addr.port() != 0 {
        let port = listen_addr.port();
        let mut listeners = Vec::new();
        let mut first_err: Option<std::io::Error> = None;
        for mut cand in candidates {
            cand.set_port(port);
            match UdpSocket::bind(cand).await {
                Ok(sock) => {
                    let concrete_addr = sock.local_addr()?;
                    let tcp_listener = TcpListener::bind(concrete_addr).await?;
                    let udp_conn: Arc<dyn Conn + Send + Sync> = Arc::new(sock);
                    let tcp_conn: Arc<dyn Conn + Send + Sync> = TcpTurnListenerConn::spawn(
                        tcp_listener,
                        concrete_addr,
                        Arc::clone(&connection_semaphore),
                    );
                    listeners.push(Arc::new(RecordingConn::new(udp_conn, concrete_addr)));
                    listeners.push(Arc::new(RecordingConn::new_tcp(tcp_conn, concrete_addr)));
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::AddrInUse
                        || err.kind() == std::io::ErrorKind::PermissionDenied
                        || cand.ip().is_loopback() =>
                {
                    return Err(err.into());
                }
                Err(err) => {
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                }
            }
        }
        if listeners.is_empty() {
            return Err(first_err
                .unwrap_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::AddrNotAvailable,
                        "no bindable concrete TURN listener address",
                    )
                })
                .into());
        }
        return Ok((SocketAddr::new(listen_addr.ip(), port), listeners));
    }

    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..DEFAULT_PORT_RANGE_MAX_RETRIES {
        let probe = UdpSocket::bind(SocketAddr::new(listen_addr.ip(), 0)).await?;
        let port = probe.local_addr()?.port();
        drop(probe);

        let mut listeners = Vec::new();
        let mut conflict = false;
        for mut cand in candidates.iter().copied() {
            cand.set_port(port);
            match UdpSocket::bind(cand).await {
                Ok(sock) => {
                    let concrete_addr = sock.local_addr()?;
                    match TcpListener::bind(concrete_addr).await {
                        Ok(tcp_listener) => {
                            let udp_conn: Arc<dyn Conn + Send + Sync> = Arc::new(sock);
                            let tcp_conn: Arc<dyn Conn + Send + Sync> = TcpTurnListenerConn::spawn(
                                tcp_listener,
                                concrete_addr,
                                Arc::clone(&connection_semaphore),
                            );
                            listeners.push(Arc::new(RecordingConn::new(udp_conn, concrete_addr)));
                            listeners
                                .push(Arc::new(RecordingConn::new_tcp(tcp_conn, concrete_addr)));
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                            last_err = Some(err);
                            conflict = true;
                            break;
                        }
                        Err(err) => return Err(err.into()),
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                    last_err = Some(err);
                    conflict = true;
                    break;
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::PermissionDenied
                        || cand.ip().is_loopback() =>
                {
                    return Err(err.into());
                }
                Err(err) => {
                    if last_err.is_none() {
                        last_err = Some(err);
                    }
                }
            }
        }
        if !conflict && !listeners.is_empty() {
            return Ok((SocketAddr::new(listen_addr.ip(), port), listeners));
        }
    }

    Err(last_err
        .unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "failed to bind concrete TURN listener sockets",
            )
        })
        .into())
}

/// A running Rings TURN server backed by [`DelegationAuthHandler`].
pub struct TurnServer {
    local_addr: SocketAddr,
    advertised_addr: SocketAddr,
    realm: String,
    auth_handler: Arc<DelegationAuthHandler>,
    clock: SharedClock,
    max_reservation_ttl: Duration,
    bridge: Arc<Mutex<ProductionBridge>>,
    handles: Arc<Mutex<HashMap<u64, Arc<ManagedAllocation>>>>,
    nonces: Arc<Mutex<HashMap<String, (FiveTuple, Instant)>>>,
    shutdown_tx: watch::Sender<bool>,
    done_rx: watch::Receiver<bool>,
}

impl TurnServer {
    /// Bind the UDP listener socket and start the TURN server with [`DelegationAuthHandler`].
    pub async fn start(config: TurnServerConfig) -> Result<Self> {
        if config.relay_allocator.is_none() {
            config.relay.validate()?;
        }
        if let Some(advertised_ip) = config.advertised_ip {
            if advertised_ip.is_ipv4() != config.listen_addr.ip().is_ipv4() {
                return Err(crate::error::TurnError::ListenerAddressFamilyMismatch {
                    advertised_ip,
                    listen_ip: config.listen_addr.ip(),
                });
            }
        }

        let relay_advertised = config.relay.advertised_ip();
        let mut extra_ips = vec![relay_bind_ip(&config.relay), relay_advertised];
        if let Some(adv) = config.advertised_ip {
            extra_ips.push(adv);
        }
        let max_tcp_connections = config
            .max_allocations
            .max(1)
            .saturating_add(config.max_nonces.max(1))
            .min(MAX_TCP_CONNECTIONS_CAP);
        let connection_semaphore = Arc::new(Semaphore::new(max_tcp_connections));
        let (local_addr, listeners) =
            bind_listener_sockets(config.listen_addr, &extra_ips, connection_semaphore).await?;
        let advertised_ip = match config.advertised_ip {
            Some(ip) if !ip.is_unspecified() => ip,
            _ if !local_addr.ip().is_unspecified() => local_addr.ip(),
            _ if !relay_advertised.is_unspecified()
                && relay_advertised.is_ipv4() == local_addr.ip().is_ipv4() =>
            {
                relay_advertised
            }
            _ => match local_addr.ip() {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            },
        };
        let advertised_addr = SocketAddr::new(advertised_ip, local_addr.port());

        let clock = Arc::clone(&config.clock);
        let auth_handler = Arc::new(DelegationAuthHandler::with_server_key_and_clock(
            config.admission,
            config.server_key,
            config.clock,
        ));
        let relay_allocator: Arc<dyn TurnDatagramRelayAllocator> = match config.relay_allocator {
            Some(custom) => custom,
            None => Arc::new(PolicyRelayAddressGenerator::new(
                config.relay,
                config.peer_policy,
            )),
        };
        let max_allocations = config.max_allocations.max(1);
        let max_reservations = config.max_reservations;
        let max_reservation_ttl = config.max_reservation_ttl.min(MAXIMUM_ALLOCATION_LIFETIME);
        let bridge = Arc::new(Mutex::new(TurnEntryBridge::new(
            max_allocations,
            max_reservations,
            max_reservation_ttl.as_millis(),
        )));
        let handles = Arc::new(Mutex::new(HashMap::new()));
        let nonces = Arc::new(Mutex::new(HashMap::new()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        let channel_bind_timeout = if config.channel_bind_timeout.is_zero() {
            DEFAULT_LIFETIME
        } else {
            config.channel_bind_timeout
        };

        tokio::spawn(Self::read_loop(
            listeners,
            Arc::clone(&bridge),
            Arc::clone(&handles),
            Arc::clone(&clock),
            relay_allocator,
            Arc::clone(&nonces),
            Arc::clone(&auth_handler),
            config.realm.clone(),
            channel_bind_timeout,
            config.peer_policy,
            config.max_nonces.max(1),
            max_allocations,
            config.max_permissions_per_allocation.max(1),
            shutdown_rx,
            done_tx,
        ));

        Ok(Self {
            local_addr,
            advertised_addr,
            realm: config.realm,
            auth_handler,
            clock,
            max_reservation_ttl,
            bridge,
            handles,
            nonces,
            shutdown_tx,
            done_rx,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn read_loop(
        listeners: Vec<Arc<RecordingConn>>,
        bridge: Arc<Mutex<ProductionBridge>>,
        handles: Arc<Mutex<HashMap<u64, Arc<ManagedAllocation>>>>,
        clock: SharedClock,
        relay_allocator: Arc<dyn TurnDatagramRelayAllocator>,
        nonces: Arc<Mutex<HashMap<String, (FiveTuple, Instant)>>>,
        auth_handler: Arc<DelegationAuthHandler>,
        realm: String,
        channel_bind_timeout: Duration,
        peer_policy: RelayPeerPolicy,
        max_nonces: usize,
        max_allocations: usize,
        max_permissions_per_allocation: usize,
        mut shutdown_rx: watch::Receiver<bool>,
        done_tx: watch::Sender<bool>,
    ) {
        let (pkt_tx, mut pkt_rx) =
            tokio::sync::mpsc::channel::<(Vec<u8>, SocketAddr, Arc<RecordingConn>)>(256);
        let mut reader_handles = Vec::with_capacity(listeners.len());
        for listener in &listeners {
            let listener = Arc::clone(listener);
            let pkt_tx = pkt_tx.clone();
            let mut reader_shutdown = shutdown_rx.clone();
            reader_handles.push(tokio::spawn(async move {
                let mut buf = vec![0u8; INBOUND_MTU];
                loop {
                    if *reader_shutdown.borrow_and_update() {
                        break;
                    }
                    let (n, src_addr) = tokio::select! {
                        v = listener.recv_from(&mut buf) => {
                            match v {
                                Ok(v) => v,
                                Err(err) => {
                                    if listener.is_closed() {
                                        tracing::debug!("exit TURN listener reader on closed socket: {err}");
                                        break;
                                    }
                                    tracing::debug!("ignoring transient TURN listener recv_from error: {err}");
                                    continue;
                                }
                            }
                        }
                        res = reader_shutdown.changed() => {
                            if res.is_err() || *reader_shutdown.borrow() {
                                break;
                            }
                            continue;
                        }
                    };
                    let Some(payload) = buf.get(..n).map(<[u8]>::to_vec) else {
                        continue;
                    };
                    tokio::select! {
                        res = pkt_tx.send((payload, src_addr, Arc::clone(&listener))) => {
                            if res.is_err() {
                                break;
                            }
                        }
                        res = reader_shutdown.changed() => {
                            if res.is_err() || *reader_shutdown.borrow() {
                                break;
                            }
                        }
                    }
                }
            }));
        }
        drop(pkt_tx);

        let completed_deletions: CompletedDeletionCache = Arc::new(Mutex::new(HashMap::new()));
        let pending_waiters: Arc<Mutex<HashMap<u64, PendingOpenWaiter>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut pending_tasks = JoinSet::new();

        loop {
            if *shutdown_rx.borrow_and_update() {
                break;
            }
            let (payload, addr, recording_conn) = tokio::select! {
                maybe_pkt = pkt_rx.recv() => {
                    let Some(pkt) = maybe_pkt else {
                        break;
                    };
                    pkt
                }
                Some(_) = pending_tasks.join_next(), if !pending_tasks.is_empty() => {
                    continue;
                }
                res = shutdown_rx.changed() => {
                    if res.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                    continue;
                }
            };
            let local_dst_addr = recording_conn.local_addr;
            let recording_conn_dyn: Arc<dyn Conn + Send + Sync> =
                Arc::clone(&recording_conn) as Arc<dyn Conn + Send + Sync>;
            let five_tuple = FiveTuple {
                src_addr: addr,
                dst_addr: local_dst_addr,
                protocol: recording_conn.protocol,
            };
            let ord_tuple = OrderedFiveTuple::from(five_tuple);

            if payload.is_empty() && recording_conn.protocol == PROTO_TCP {
                let outcome = bridge.lock().await.disconnect_tuple(&ord_tuple);
                if let Some(open_id) = outcome.cancelled_open_id {
                    if let Some(waiter) = pending_waiters.lock().await.remove(&open_id) {
                        let _ = waiter.cancel_tx.send_replace(true);
                    }
                }
                if let Some(alloc_id) = outcome.closed_alloc_id {
                    close_alloc_ids(&bridge, &handles, vec![alloc_id]).await;
                }
                completed_deletions.lock().await.remove(&five_tuple);
                nonces
                    .lock()
                    .await
                    .retain(|_, (tuple, _)| *tuple != five_tuple);
                recording_conn.notify_tcp_peer_disconnect_processed(addr);
                continue;
            }

            if ChannelData::is_channel_data(&payload) {
                let mut channel_data = ChannelData {
                    raw: payload,
                    ..Default::default()
                };
                if channel_data.decode().is_ok() {
                    if let Some(alloc) =
                        get_active_allocation(&bridge, &handles, &clock, &five_tuple).await
                    {
                        let peer = bridge.lock().await.outbound_channel_peer(
                            &alloc.ord_tuple,
                            alloc.alloc_id,
                            channel_data.number.0,
                            clock.now_ms(),
                        );
                        if let Some(peer) = peer {
                            alloc.enqueue_payload_to_peer(&channel_data.data, peer);
                        }
                    }
                }
                continue;
            }

            let mut decoded_msg = Message {
                raw: payload,
                ..Default::default()
            };
            if decoded_msg.decode().is_err() || !sanitize_authenticated_message(&mut decoded_msg) {
                continue;
            }

            if decoded_msg.typ.class == CLASS_INDICATION {
                if decoded_msg.typ.method == METHOD_SEND
                    && !decoded_msg.contains(ATTR_DONT_FRAGMENT)
                    && check_message_peer_addresses(&decoded_msg, peer_policy)
                {
                    let mut data = Data::default();
                    let mut peer_address = PeerAddress::default();
                    if data.get_from(&decoded_msg).is_ok()
                        && peer_address.get_from(&decoded_msg).is_ok()
                    {
                        let dst = SocketAddr::new(peer_address.ip, peer_address.port);
                        if peer_policy.is_allowed_addr(dst) {
                            if let Some(alloc) =
                                get_active_allocation(&bridge, &handles, &clock, &five_tuple).await
                            {
                                let allowed = bridge.lock().await.has_permission(
                                    &alloc.ord_tuple,
                                    alloc.alloc_id,
                                    dst.ip(),
                                    clock.now_ms(),
                                );
                                if allowed {
                                    alloc.enqueue_payload_to_peer(&data.0, dst);
                                }
                            }
                        }
                    }
                }
                continue;
            }

            if decoded_msg.typ.class != CLASS_REQUEST {
                continue;
            }

            if decoded_msg.typ.method == METHOD_BINDING {
                let built = {
                    let mut resp = Message::new();
                    let setters: Vec<Box<dyn Setter>> = vec![
                        Box::new(Message {
                            transaction_id: decoded_msg.transaction_id,
                            ..Default::default()
                        }),
                        Box::new(MessageType::new(METHOD_BINDING, CLASS_SUCCESS_RESPONSE)),
                        Box::new(XorMappedAddress {
                            ip: addr.ip(),
                            port: addr.port(),
                        }),
                        Box::new(FINGERPRINT),
                    ];
                    resp.build(&setters).is_ok().then_some(resp.raw)
                };
                if let Some(raw) = built {
                    let _ = recording_conn.send_to(&raw, addr).await;
                }
                continue;
            }

            if decoded_msg.typ.method != METHOD_ALLOCATE
                && decoded_msg.typ.method != METHOD_REFRESH
                && decoded_msg.typ.method != METHOD_CREATE_PERMISSION
                && decoded_msg.typ.method != METHOD_CHANNEL_BIND
            {
                send_unauthenticated_error(
                    &recording_conn_dyn,
                    addr,
                    &decoded_msg,
                    CODE_BAD_REQUEST,
                    b"Unsupported Method",
                )
                .await;
                continue;
            }

            let existing_alloc =
                get_active_allocation(&bridge, &handles, &clock, &five_tuple).await;
            let now_for_owner = clock.now_ms();
            let expected_owner = if decoded_msg.typ.method == METHOD_ALLOCATE {
                bridge
                    .lock()
                    .await
                    .expected_owner_for_allocate(
                        &ord_tuple,
                        &decoded_msg.transaction_id,
                        now_for_owner,
                    )
                    .map(str::to_owned)
            } else if is_zero_lifetime_refresh(&decoded_msg) {
                let from_del = completed_deletions
                    .lock()
                    .await
                    .get(&five_tuple)
                    .and_then(|q| {
                        q.iter()
                            .rev()
                            .find(|entry| entry.transaction_id == decoded_msg.transaction_id)
                            .map(|entry| entry.username.clone())
                    });
                match from_del {
                    Some(owner) => Some(owner),
                    None => bridge
                        .lock()
                        .await
                        .expected_owner_for_established(&ord_tuple, now_for_owner)
                        .map(str::to_owned),
                }
            } else {
                bridge
                    .lock()
                    .await
                    .expected_owner_for_established(&ord_tuple, now_for_owner)
                    .map(str::to_owned)
            };
            if let Some(owner) = expected_owner {
                auth_handler.set_expected_allocation_owner(addr, Some(owner));
            }

            let auth_res = authenticate_turn_request(
                &recording_conn_dyn,
                &nonces,
                max_nonces,
                &auth_handler,
                &realm,
                five_tuple,
                &decoded_msg,
            )
            .await;
            auth_handler.set_expected_allocation_owner(addr, None);

            let Some((username_attr, mi_key)) = auth_res else {
                continue;
            };

            match decoded_msg.typ.method {
                METHOD_ALLOCATE => {
                    handle_authenticated_allocate(
                        &recording_conn_dyn,
                        &bridge,
                        &handles,
                        &pending_waiters,
                        &mut pending_tasks,
                        &clock,
                        &shutdown_rx,
                        &relay_allocator,
                        five_tuple,
                        addr,
                        &decoded_msg,
                        username_attr,
                        mi_key,
                    )
                    .await;
                }
                METHOD_REFRESH => {
                    if is_zero_lifetime_refresh(&decoded_msg) {
                        if let Some(cached_resp) = find_completed_deletion(
                            &completed_deletions,
                            &nonces,
                            &five_tuple,
                            &username_attr.text,
                            decoded_msg.transaction_id,
                            max_allocations,
                        )
                        .await
                        {
                            let _ = recording_conn.send_to(&cached_resp, addr).await;
                            continue;
                        }
                    }
                    let Some(alloc) = existing_alloc else {
                        send_authenticated_error(
                            &recording_conn_dyn,
                            addr,
                            &decoded_msg,
                            CODE_ALLOC_MISMATCH,
                            b"Allocation Mismatch",
                            None,
                            mi_key,
                        )
                        .await;
                        continue;
                    };
                    handle_authenticated_refresh(
                        &recording_conn_dyn,
                        &bridge,
                        &handles,
                        &clock,
                        &nonces,
                        &completed_deletions,
                        max_allocations,
                        alloc,
                        &decoded_msg,
                        mi_key,
                    )
                    .await;
                }
                METHOD_CREATE_PERMISSION => {
                    let Some(alloc) = existing_alloc else {
                        send_authenticated_error(
                            &recording_conn_dyn,
                            addr,
                            &decoded_msg,
                            CODE_ALLOC_MISMATCH,
                            b"Allocation Mismatch",
                            None,
                            mi_key,
                        )
                        .await;
                        continue;
                    };
                    handle_authenticated_create_permission(
                        &recording_conn_dyn,
                        &bridge,
                        &clock,
                        &shutdown_rx,
                        &mut pending_tasks,
                        &alloc,
                        &decoded_msg,
                        peer_policy,
                        max_permissions_per_allocation,
                        mi_key,
                    )
                    .await;
                }
                METHOD_CHANNEL_BIND => {
                    let Some(alloc) = existing_alloc else {
                        send_authenticated_error(
                            &recording_conn_dyn,
                            addr,
                            &decoded_msg,
                            CODE_ALLOC_MISMATCH,
                            b"Allocation Mismatch",
                            None,
                            mi_key,
                        )
                        .await;
                        continue;
                    };
                    handle_authenticated_channel_bind(
                        &recording_conn_dyn,
                        &bridge,
                        &clock,
                        &shutdown_rx,
                        &mut pending_tasks,
                        &alloc,
                        &decoded_msg,
                        peer_policy,
                        channel_bind_timeout,
                        max_permissions_per_allocation,
                        mi_key,
                    )
                    .await;
                }
                _ => {}
            }
        }

        drop(pkt_rx);
        let live_to_close = bridge.lock().await.begin_shutdown();
        for (_, waiter) in pending_waiters.lock().await.drain() {
            let _ = waiter.cancel_tx.send_replace(true);
        }
        close_alloc_ids(&bridge, &handles, live_to_close).await;
        while pending_tasks.join_next().await.is_some() {}
        for listener in &listeners {
            listener.shutdown_listener().await;
        }
        for handle in reader_handles {
            let _ = handle.await;
        }
        drop(listeners);
        let _ = done_tx.send_replace(true);
    }

    /// Return the local UDP [`SocketAddr`] this TURN server is bound to.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Return the external/connectable UDP [`SocketAddr`] advertised for this TURN server.
    pub const fn advertised_addr(&self) -> SocketAddr {
        self.advertised_addr
    }

    /// Return the configured TURN authentication realm.
    pub fn realm(&self) -> &str {
        &self.realm
    }

    /// Return the server's compressed secp256k1 [`PublicKey<33>`].
    pub fn server_pubkey(&self) -> PublicKey<33> {
        self.auth_handler.server_pubkey()
    }

    /// Return a reference to the server's [`DelegationAuthHandler`].
    pub const fn auth_handler(&self) -> &Arc<DelegationAuthHandler> {
        &self.auth_handler
    }

    /// Return the unsigned `turn://<advertised_addr>?pubkey=<base58(server_pubkey)>` URL for this
    /// server, suitable for passing to [`TurnCredentialSigner::sign_ice_servers`].
    pub fn ice_server_base_url(&self) -> Result<String> {
        let base_url = format!("turn://{}", self.advertised_addr);
        Ok(format_turn_server_url(&base_url, self.server_pubkey())?)
    }

    /// Return the unsigned `turn://<advertised_addr>?transport=tcp&pubkey=<base58(server_pubkey)>`
    /// URL for connecting to this server over TCP.
    pub fn tcp_ice_server_base_url(&self) -> Result<String> {
        let base_url = format!("turn://{}?transport=tcp", self.advertised_addr);
        Ok(format_turn_server_url(&base_url, self.server_pubkey())?)
    }

    /// Build a complete signed `turn://<username>:<credential>@<advertised_addr>` URL for `delegatee_key`.
    pub fn ice_server_url_for(&self, delegatee_key: &DelegateeKey) -> Result<String> {
        let base_url = self.ice_server_base_url()?;
        Ok(delegatee_key.sign_ice_servers(&base_url)?)
    }

    /// Build a complete signed `turn://<username>:<credential>@<advertised_addr>?transport=tcp`
    /// URL for `delegatee_key`.
    pub fn tcp_ice_server_url_for(&self, delegatee_key: &DelegateeKey) -> Result<String> {
        let base_url = self.tcp_ice_server_base_url()?;
        Ok(delegatee_key.sign_ice_servers(&base_url)?)
    }

    /// Return the current number of active challenge nonces held in memory.
    pub async fn active_nonce_count(&self) -> usize {
        self.nonces.lock().await.len()
    }

    /// Return the current number of allocation capacity slots held (`live + pending + closing`).
    pub async fn used_allocation_slots(&self) -> usize {
        let now_ms = self.clock.now_ms();
        let expired = self.bridge.lock().await.sweep_expired_allocations(now_ms);
        if !expired.is_empty() {
            close_alloc_ids(&self.bridge, &self.handles, expired).await;
        }
        self.bridge.lock().await.used_allocation_slots()
    }

    /// Query active TURN allocations managed by this server.
    pub async fn allocations_info(
        &self,
        five_tuples: Option<Vec<FiveTuple>>,
    ) -> Result<HashMap<FiveTuple, AllocationInfo>> {
        if let Some(ref tuples) = five_tuples {
            if tuples.is_empty() {
                return Ok(HashMap::new());
            }
        }
        let now_ms = self.clock.now_ms();
        let (expired_ids, infos) = {
            let mut guard = self.bridge.lock().await;
            let expired_ids = guard.sweep_expired_allocations(now_ms);
            let mut infos = HashMap::new();
            match five_tuples {
                Some(tuples) => {
                    for tuple in tuples {
                        let ord = OrderedFiveTuple::from(tuple);
                        if let Some(alloc) = guard.live_allocations().get(&ord) {
                            infos.insert(
                                tuple,
                                AllocationInfo::new(
                                    tuple,
                                    alloc.username.clone(),
                                    alloc.relay_addr,
                                ),
                            );
                        }
                    }
                }
                None => {
                    for (&ord, alloc) in guard.live_allocations() {
                        let tuple = FiveTuple::from(ord);
                        infos.insert(
                            tuple,
                            AllocationInfo::new(tuple, alloc.username.clone(), alloc.relay_addr),
                        );
                    }
                }
            }
            (expired_ids, infos)
        };
        close_alloc_ids(&self.bridge, &self.handles, expired_ids).await;
        Ok(infos)
    }

    /// Delete all active TURN allocations associated with `username`.
    pub async fn delete_allocations_by_username(&self, username: String) -> Result<()> {
        let removed_ids = self
            .bridge
            .lock()
            .await
            .remove_by_username(&username, self.clock.now_ms());
        close_alloc_ids(&self.bridge, &self.handles, removed_ids).await;
        Ok(())
    }

    /// Return the current number of unclaimed live session reservations held in memory.
    pub async fn active_reservation_count(&self) -> usize {
        self.bridge
            .lock()
            .await
            .prune_expired_tickets(self.clock.now_ms())
    }

    /// Reserve a TURN session on this server with an unlinkable [`SignedTurnSessionTicket`] (#1292).
    ///
    /// Enforces `max_reservations` and clamps the unclaimed reservation window to
    /// `max_reservation_ttl` (`<= 60s` by default). No relay socket or `max_allocations` permit is
    /// bound until the client authenticates `Allocate` with the returned ticket.
    pub async fn reserve_exit_session(
        &self,
        ephemeral_client_pubkey: PublicKey<33>,
        ttl: Duration,
    ) -> Result<TurnSessionReservation> {
        let _ =
            rings_core::ecc::prime_order::NonIdentityPoint::<rings_core::ecc::Secp256k1>::try_from(
                ephemeral_client_pubkey,
            )?;
        if ttl.is_zero() || self.max_reservation_ttl.is_zero() {
            return Err(crate::error::TurnError::InvalidReservationTtl);
        }
        let clamped_ttl = ttl.min(self.max_reservation_ttl);
        let now_ms = self.clock.now_ms();
        let mut session_id = [0u8; 16];
        rand::thread_rng().fill(&mut session_id);
        let expires_at_ms = now_ms.saturating_add(clamped_ttl.as_millis());
        let ticket = SignedTurnSessionTicket::issue(
            session_id,
            ephemeral_client_pubkey,
            expires_at_ms,
            self.auth_handler.server_key(),
        )?;
        match self
            .bridge
            .lock()
            .await
            .reserve_ticket(session_id, clamped_ttl.as_millis(), now_ms)
        {
            Ok(_) => Ok(TurnSessionReservation {
                ticket,
                server_addr: self.advertised_addr,
                server_pubkey: self.server_pubkey(),
            }),
            Err(BridgeReserveError::InvalidTtl) => {
                Err(crate::error::TurnError::InvalidReservationTtl)
            }
            Err(BridgeReserveError::CapacityReached { max_reservations }) => {
                Err(crate::error::TurnError::ReservationCapacityReached { max_reservations })
            }
        }
    }

    /// Gracefully stop the TURN server and close all managed connections and relay allocations.
    pub async fn close(&self) -> Result<()> {
        let _ = self.shutdown_tx.send_replace(true);
        let initial_to_close = self.bridge.lock().await.begin_shutdown();
        close_alloc_ids(&self.bridge, &self.handles, initial_to_close).await;
        let mut done_rx = self.done_rx.clone();
        let _ = done_rx.wait_for(|&done| done).await;
        let remaining = self.bridge.lock().await.begin_shutdown();
        close_alloc_ids(&self.bridge, &self.handles, remaining).await;
        Ok(())
    }
}
