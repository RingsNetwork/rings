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
use rings_transport::turn_tcp_framing::encode_turn_tcp_frame;
use rings_transport::turn_tcp_framing::is_stun_control_response;
use rings_transport::turn_tcp_framing::read_turn_tcp_frame_body;
use rings_transport::turn_tcp_framing::STUN_HEADER_BYTES;
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
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time::Duration;
use tokio::time::Instant;
use turn::allocation::five_tuple::FiveTuple;
use turn::allocation::AllocationInfo;
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
use crate::config::RelayAddressConfig;
use crate::config::RelayPeerPolicy;
use crate::config::TurnServerConfig;
use crate::config::DEFAULT_PORT_RANGE_MAX_RETRIES;
use crate::credentials::format_turn_server_url;
use crate::credentials::SignedTurnSessionTicket;
use crate::credentials::TurnCredentialSigner;
use crate::credentials::VerifiedTurnAuth;
use crate::error::Result;
use crate::relay_backend::BeginAllocateOutcome;
use crate::relay_backend::CompleteAllocateDecision;
use crate::relay_backend::CorePermissionError;
use crate::relay_backend::CoreReserveError;
use crate::relay_backend::RefreshDecision;
use crate::relay_backend::TurnAllocationCore;
use crate::relay_backend::TurnDatagramRelay;
use crate::relay_backend::TurnDatagramRelayFactory;
use crate::relay_backend::TurnFiveTupleKey;
use crate::relay_backend::TurnRelayCloseHandle;

const INBOUND_MTU: usize = STUN_HEADER_BYTES + (u16::MAX as usize);
const STUN_ATTR_HEADER_BYTES: usize = 4;
const PROTO_TCP: Protocol = Protocol(6);
const MAX_ALLOCATION_OUTBOUND_QUEUE: usize = 64;
const MAX_ALLOCATION_CONTROL_QUEUE: usize = 16;
const RELAY_SEND_TIMEOUT: Duration = Duration::from_secs(1);
const RELAY_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
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
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);
const NONCE_LIFETIME: Duration = Duration::from_secs(3600);
const MAX_DELETION_TXS_PER_TUPLE: usize = 16;

struct PolicyRelayConn {
    relayed_addr: SocketAddr,
    inner: SyncMutex<Option<Arc<dyn Conn + Send + Sync>>>,
    peer_policy: RelayPeerPolicy,
    closed_notify: Notify,
    recv_lock: Mutex<()>,
}

impl PolicyRelayConn {
    fn new(
        relayed_addr: SocketAddr,
        inner: Arc<dyn Conn + Send + Sync>,
        peer_policy: RelayPeerPolicy,
    ) -> Self {
        Self {
            relayed_addr,
            inner: SyncMutex::new(Some(inner)),
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

    async fn close(&self) -> webrtc_util::Result<()> {
        let taken = <Self as TurnDatagramRelay>::begin_close(self);
        <Self as TurnDatagramRelay>::finish_close(self, taken).await;
        Ok(())
    }
}

#[async_trait]
impl TurnDatagramRelay for PolicyRelayConn {
    fn relayed_addr(&self) -> SocketAddr {
        self.relayed_addr
    }

    async fn recv_datagram(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        let _guard = self.recv_lock.lock().await;
        loop {
            let notified = self.closed_notify.notified();
            let inner = self
                .clone_inner()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let res = tokio::select! {
                _ = notified => return Err(std::io::Error::other("relay socket closed")),
                res = inner.recv_from(buf) => res,
            };
            let (n, src_addr) = match res {
                Ok(v) => v,
                Err(err) => {
                    if self.clone_inner().is_err() {
                        return Err(std::io::Error::other(err.to_string()));
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

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
        if !self.peer_policy.is_allowed_addr(target) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "relay destination address denied by policy",
            ));
        }
        let inner = self
            .clone_inner()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        inner
            .send_to(buf, target)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    fn begin_close(&self) -> TurnRelayCloseHandle {
        let taken = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.closed_notify.notify_waiters();
        taken.map(|conn| Box::new(conn) as Box<dyn std::any::Any + Send>)
    }

    async fn finish_close(&self, handle: TurnRelayCloseHandle) {
        let _guard = self.recv_lock.lock().await;
        if let Some(boxed) = handle {
            if let Ok(inner) = boxed.downcast::<Arc<dyn Conn + Send + Sync>>() {
                let _ = inner.close().await;
            }
        }
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
        let policy_conn = Arc::new(PolicyRelayConn::new(relay_addr, conn, self.peer_policy));
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
impl TurnDatagramRelayFactory for PolicyRelayAddressGenerator {
    fn supports_family(&self, use_ipv4: bool) -> bool {
        use_ipv4 == relay_bind_ip_is_ipv4(&self.relay)
    }

    async fn allocate_relay(
        &self,
        use_ipv4: bool,
        need_even_port: bool,
    ) -> std::io::Result<Arc<dyn TurnDatagramRelay>> {
        let (conn, _addr) = self
            .allocate_relay_conn(use_ipv4, need_even_port)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(conn)
    }
}

struct ChannelBindingTable {
    bindings: HashMap<ChannelNumber, (SocketAddr, u128)>,
}

type CompletedDeletionCache = Arc<Mutex<HashMap<FiveTuple, VecDeque<CompletedDeletionEntry>>>>;

/// Result of reserving a TURN relay session on a [`TurnServer`] (#1292).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnSessionReservation {
    /// Server-signed unlinkable session ticket used to derive [`crate::credentials::TurnCredentials`].
    pub ticket: SignedTurnSessionTicket,
    /// This TURN server's connectable advertised address.
    pub server_addr: SocketAddr,
    /// This TURN server's compressed secp256k1 [`PublicKey<33>`].
    pub server_pubkey: PublicKey<33>,
}

struct AllocationPinholeJob {
    src_addr: SocketAddr,
    peer_ips: Vec<IpAddr>,
    req_msg: Message,
    mi_key: Vec<u8>,
    success_response: Option<Vec<u8>>,
}

struct AllocationWorkerReceivers {
    outbound_data_rx: mpsc::Receiver<(Vec<u8>, SocketAddr)>,
    control_rx: mpsc::Receiver<AllocationPinholeJob>,
}

enum PinholeEnqueueError {
    Full,
    Closed,
}

struct ManagedAllocation {
    open_id: u64,
    five_tuple: FiveTuple,
    tuple_key: TurnFiveTupleKey,
    username: String,
    relay_addr: SocketAddr,
    client_conn: Arc<dyn Conn + Send + Sync>,
    relay_conn: Arc<dyn TurnDatagramRelay>,
    channels: SyncMutex<ChannelBindingTable>,
    outbound_data_tx: mpsc::Sender<(Vec<u8>, SocketAddr)>,
    control_tx: mpsc::Sender<AllocationPinholeJob>,
    closed_tx: watch::Sender<bool>,
    expiry_tx: watch::Sender<Instant>,
    tcp_authenticated_tx: Option<watch::Sender<Option<u64>>>,
    relay_permit: SyncMutex<Option<OwnedSemaphorePermit>>,
}

impl ManagedAllocation {
    #[allow(clippy::too_many_arguments)]
    fn new(
        open_id: u64,
        five_tuple: FiveTuple,
        username: String,
        relay_addr: SocketAddr,
        client_conn: Arc<dyn Conn + Send + Sync>,
        relay_conn: Arc<dyn TurnDatagramRelay>,
        lifetime: Duration,
        tcp_authenticated_tx: Option<watch::Sender<Option<u64>>>,
        relay_permit: OwnedSemaphorePermit,
    ) -> (Arc<Self>, AllocationWorkerReceivers) {
        if let Some(ref tx) = tcp_authenticated_tx {
            let _ = tx.send_replace(Some(open_id));
        }
        let (outbound_data_tx, outbound_data_rx) = mpsc::channel(MAX_ALLOCATION_OUTBOUND_QUEUE);
        let (control_tx, control_rx) = mpsc::channel(MAX_ALLOCATION_CONTROL_QUEUE);
        let (closed_tx, _) = watch::channel(false);
        let (expiry_tx, _) = watch::channel(Instant::now() + lifetime);
        (
            Arc::new(Self {
                open_id,
                five_tuple,
                tuple_key: TurnFiveTupleKey::from(five_tuple),
                username,
                relay_addr,
                client_conn,
                relay_conn,
                channels: SyncMutex::new(ChannelBindingTable {
                    bindings: HashMap::new(),
                }),
                outbound_data_tx,
                control_tx,
                closed_tx,
                expiry_tx,
                tcp_authenticated_tx,
                relay_permit: SyncMutex::new(Some(relay_permit)),
            }),
            AllocationWorkerReceivers {
                outbound_data_rx,
                control_rx,
            },
        )
    }

    fn info(&self) -> AllocationInfo {
        AllocationInfo::new(self.five_tuple, self.username.clone(), self.relay_addr)
    }

    fn refresh_timer(&self, lifetime: Duration) {
        let _ = self.expiry_tx.send_replace(Instant::now() + lifetime);
    }

    fn begin_close(&self) -> TurnRelayCloseHandle {
        let _ = self.closed_tx.send_replace(true);
        if let Some(ref tx) = self.tcp_authenticated_tx {
            let open_id = self.open_id;
            tx.send_if_modified(|current| {
                if *current == Some(open_id) {
                    *current = None;
                    true
                } else {
                    false
                }
            });
        }
        let _ = self.expiry_tx.send_replace(Instant::now());
        self.relay_conn.begin_close()
    }

    async fn finish_close(&self, taken: TurnRelayCloseHandle) {
        self.relay_conn.finish_close(taken).await;
        let _ = self
            .relay_permit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    async fn recv_payload_from_peer(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.relay_conn.recv_datagram(buf).await
    }

    fn enqueue_payload_to_peer(&self, payload: &[u8], target: SocketAddr) {
        let _ = self.outbound_data_tx.try_send((payload.to_vec(), target));
    }

    fn try_reserve_pinhole_slot(
        &self,
    ) -> std::result::Result<mpsc::Permit<'_, AllocationPinholeJob>, PinholeEnqueueError> {
        self.control_tx.try_reserve().map_err(|err| match err {
            mpsc::error::TrySendError::Full(()) => PinholeEnqueueError::Full,
            mpsc::error::TrySendError::Closed(()) => PinholeEnqueueError::Closed,
        })
    }

    fn has_channel_conflict(&self, channel: ChannelNumber, peer: SocketAddr, now_ms: u128) -> bool {
        let mut table = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        table.bindings.retain(|_, (_, exp)| *exp > now_ms);
        table.bindings.iter().any(|(&bound_ch, &(bound_peer, _))| {
            (bound_ch == channel && bound_peer != peer)
                || (bound_peer == peer && bound_ch != channel)
        })
    }

    fn commit_channel_binding(
        &self,
        channel: ChannelNumber,
        peer: SocketAddr,
        channel_timeout: Duration,
        now_ms: u128,
    ) {
        let mut table = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        table.bindings.retain(|_, (_, exp)| *exp > now_ms);
        table.bindings.insert(
            channel,
            (peer, now_ms.saturating_add(channel_timeout.as_millis())),
        );
    }

    fn matching_inbound_channel(&self, peer: SocketAddr, now_ms: u128) -> Option<ChannelNumber> {
        let mut table = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        table.bindings.retain(|_, (_, exp)| *exp > now_ms);
        table
            .bindings
            .iter()
            .find_map(|(&ch, &(bound_peer, _))| (bound_peer == peer).then_some(ch))
    }

    fn outbound_channel_peer(&self, channel: ChannelNumber, now_ms: u128) -> Option<SocketAddr> {
        let mut table = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        table.bindings.retain(|_, (_, exp)| *exp > now_ms);
        table.bindings.get(&channel).map(|&(peer, _)| peer)
    }
}

struct ServerAllocationBook {
    core: TurnAllocationCore,
    by_open_id: HashMap<u64, Arc<ManagedAllocation>>,
    by_tuple: HashMap<FiveTuple, Arc<ManagedAllocation>>,
    allocate_responses: HashMap<FiveTuple, (TransactionId, Vec<u8>)>,
    pending_cancels: HashMap<u64, watch::Sender<bool>>,
    cancelling_open_ids: HashSet<u64>,
    relay_permits: Arc<Semaphore>,
}

impl ServerAllocationBook {
    fn new(
        max_allocations: usize,
        max_reservations: usize,
        max_permissions_per_allocation: usize,
    ) -> Self {
        let max_allocations = max_allocations.clamp(1, Semaphore::MAX_PERMITS);
        Self {
            core: TurnAllocationCore::new(
                max_allocations,
                max_reservations,
                max_permissions_per_allocation,
            ),
            by_open_id: HashMap::new(),
            by_tuple: HashMap::new(),
            allocate_responses: HashMap::new(),
            pending_cancels: HashMap::new(),
            cancelling_open_ids: HashSet::new(),
            relay_permits: Arc::new(Semaphore::new(max_allocations)),
        }
    }

    fn cancel_pending_open_ids(&mut self, open_ids: &[u64]) {
        for &open_id in open_ids {
            if let Some(cancel_tx) = self.pending_cancels.remove(&open_id) {
                let _ = cancel_tx.send_replace(true);
                self.cancelling_open_ids.insert(open_id);
            }
        }
    }

    fn take_open_ids(&mut self, open_ids: &[u64]) -> Vec<Arc<ManagedAllocation>> {
        let mut out = Vec::with_capacity(open_ids.len());
        for &open_id in open_ids {
            if let Some(alloc) = self.by_open_id.remove(&open_id) {
                if self
                    .by_tuple
                    .get(&alloc.five_tuple)
                    .is_some_and(|cur| cur.open_id == open_id)
                {
                    self.by_tuple.remove(&alloc.five_tuple);
                    self.allocate_responses.remove(&alloc.five_tuple);
                }
                out.push(alloc);
            }
        }
        out
    }

    fn prune_and_take_expired(&mut self, now_ms: u128) -> Vec<Arc<ManagedAllocation>> {
        let evicted = self.core.prune_expired(now_ms);
        self.take_open_ids(&evicted)
    }

    fn prune_expired_allocations_and_take(&mut self, now_ms: u128) -> Vec<Arc<ManagedAllocation>> {
        let evicted = self.core.prune_expired_allocations(now_ms);
        self.take_open_ids(&evicted)
    }

    fn get_active(
        &mut self,
        five_tuple: &FiveTuple,
        now_ms: u128,
    ) -> (Option<Arc<ManagedAllocation>>, Vec<Arc<ManagedAllocation>>) {
        let expired = self.prune_expired_allocations_and_take(now_ms);
        let found = self.by_tuple.get(five_tuple).cloned();
        (found, expired)
    }

    fn expected_owner_for_request(
        &self,
        five_tuple: &FiveTuple,
        tx_id: TransactionId,
        is_allocate: bool,
    ) -> Option<String> {
        let tuple_key = TurnFiveTupleKey::from(*five_tuple);
        if is_allocate {
            if let Some(pending) = self.core.pending_opens().get(&tuple_key) {
                if pending.tx_id == tx_id.0 {
                    return Some(pending.username.clone());
                }
            }
            if let Some(active) = self.core.active_allocations().get(&tuple_key) {
                if active.tx_id == tx_id.0 {
                    return Some(active.username.clone());
                }
            }
            return None;
        }
        self.core
            .active_allocations()
            .get(&tuple_key)
            .map(|active| active.username.clone())
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
        mut start_rx: watch::Receiver<bool>,
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
            while !*start_rx.borrow_and_update() {
                if *shutdown_rx.borrow_and_update() {
                    drop(listener);
                    return;
                }
                tokio::select! {
                    res = start_rx.changed() => {
                        if res.is_err() {
                            drop(listener);
                            return;
                        }
                    }
                    res = shutdown_rx.changed() => {
                        if res.is_err() || *shutdown_rx.borrow() {
                            drop(listener);
                            return;
                        }
                    }
                }
            }

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
                                res = read_turn_tcp_frame_body(&mut reader, prefix, INBOUND_MTU, TCP_FRAME_BODY_TIMEOUT) => res,
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

#[allow(clippy::too_many_arguments)]
async fn authenticate_turn_request(
    conn: &Arc<dyn Conn + Send + Sync>,
    nonces: &Mutex<HashMap<String, (FiveTuple, Instant)>>,
    max_nonces: usize,
    auth_handler: &DelegationAuthHandler,
    realm: &str,
    five_tuple: FiveTuple,
    expected_owner: Option<&str>,
    m: &Message,
) -> Option<(Username, VerifiedTurnAuth, Vec<u8>)> {
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

    let (verified_auth, key) =
        match auth_handler.authenticate(&username_attr.text, &realm_attr.text, expected_owner) {
            Ok(verified_and_key) => verified_and_key,
            Err(error) => {
                tracing::debug!(
                    %error,
                    src_addr = %src_addr,
                    realm = %realm_attr.text,
                    "rejected TURN request authentication"
                );
                send_unauthenticated_error(conn, src_addr, m, CODE_UNAUTHORIZED, b"Unauthorized")
                    .await;
                return None;
            }
        };
    if MessageIntegrity(key.clone()).check(&mut m.clone()).is_err() {
        send_unauthenticated_error(conn, src_addr, m, CODE_UNAUTHORIZED, b"Unauthorized").await;
        return None;
    }

    Some((username_attr, verified_auth, key))
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

async fn close_allocations(removed: Vec<Arc<ManagedAllocation>>) {
    if removed.is_empty() {
        return;
    }
    let closing: Vec<_> = removed
        .into_iter()
        .map(|alloc| {
            let taken = alloc.begin_close();
            (alloc, taken)
        })
        .collect();
    let _ = tokio::spawn(async move {
        for (alloc, taken) in closing {
            alloc.finish_close(taken).await;
        }
    })
    .await;
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

async fn remove_allocation_if_current(
    book: &SyncMutex<ServerAllocationBook>,
    alloc: &Arc<ManagedAllocation>,
) {
    let removed = {
        let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
        if guard
            .core
            .remove_if_open_id(&alloc.tuple_key, alloc.open_id)
        {
            guard.take_open_ids(&[alloc.open_id])
        } else {
            Vec::new()
        }
    };
    close_allocations(removed).await;
}

async fn try_expire_allocation_if_current(
    book: &SyncMutex<ServerAllocationBook>,
    alloc: &Arc<ManagedAllocation>,
) -> bool {
    let removed = {
        let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
        let is_current = guard
            .by_tuple
            .get(&alloc.five_tuple)
            .is_some_and(|cur| cur.open_id == alloc.open_id);
        if !is_current {
            return true;
        }
        if *alloc.expiry_tx.borrow() > Instant::now() {
            return false;
        }
        let _ = guard
            .core
            .remove_if_open_id(&alloc.tuple_key, alloc.open_id);
        guard.take_open_ids(&[alloc.open_id])
    };
    close_allocations(removed).await;
    true
}

async fn get_active_allocation(
    book: &SyncMutex<ServerAllocationBook>,
    five_tuple: &FiveTuple,
    now_ms: u128,
) -> Option<Arc<ManagedAllocation>> {
    let (found, expired) = {
        let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
        guard.get_active(five_tuple, now_ms)
    };
    close_allocations(expired).await;
    found
}

async fn relay_packet_handler(
    alloc: Arc<ManagedAllocation>,
    book: Arc<SyncMutex<ServerAllocationBook>>,
    clock: SharedClock,
    turn_socket: Arc<dyn Conn + Send + Sync>,
    client_addr: SocketAddr,
) {
    let mut buffer = vec![0u8; INBOUND_MTU];
    let mut expiry_rx = alloc.expiry_tx.subscribe();
    loop {
        let deadline = *expiry_rx.borrow_and_update();
        if deadline <= Instant::now() {
            if try_expire_allocation_if_current(&book, &alloc).await {
                return;
            }
            continue;
        }
        let sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(sleep);

        tokio::select! {
            _ = &mut sleep => {
                if try_expire_allocation_if_current(&book, &alloc).await {
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
                if *expiry_rx.borrow() <= Instant::now() {
                    if try_expire_allocation_if_current(&book, &alloc).await {
                        return;
                    }
                    continue;
                }
                let Some(payload) = buffer.get(..n) else {
                    continue;
                };
                // RFC 5766 Section 10.3: verify in `TurnAllocationCore` that an active permission
                // exists for the peer's IP before forwarding via ChannelData or DataIndication.
                let now_ms = clock.now_ms();
                let permitted = {
                    let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
                    guard.core.has_permission(&alloc.tuple_key, src_addr.ip(), now_ms)
                };
                if !permitted {
                    continue;
                }
                if let Some(number) = alloc.matching_inbound_channel(src_addr, now_ms) {
                    let mut channel_data = ChannelData {
                        data: payload.to_vec(),
                        number,
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
    remove_allocation_if_current(&book, &alloc).await;
}

async fn allocation_outbound_worker(
    alloc: Arc<ManagedAllocation>,
    book: Arc<SyncMutex<ServerAllocationBook>>,
    mut control_rx: mpsc::Receiver<AllocationPinholeJob>,
    mut data_rx: mpsc::Receiver<(Vec<u8>, SocketAddr)>,
) {
    enum WorkerItem {
        Closed,
        Control(AllocationPinholeJob),
        Data(Vec<u8>, SocketAddr),
    }

    let mut closed_rx = alloc.closed_tx.subscribe();
    loop {
        if *closed_rx.borrow_and_update() {
            break;
        }
        let next = tokio::select! {
            biased;
            _ = closed_rx.wait_for(|&closed| closed) => WorkerItem::Closed,
            maybe_job = control_rx.recv() => match maybe_job {
                Some(job) => WorkerItem::Control(job),
                None => WorkerItem::Closed,
            },
            maybe_pkt = data_rx.recv() => match maybe_pkt {
                Some((payload, dst)) => WorkerItem::Data(payload, dst),
                None => WorkerItem::Closed,
            },
        };
        match next {
            WorkerItem::Closed => break,
            WorkerItem::Control(job) => {
                let pinhole_res = tokio::select! {
                    _ = closed_rx.wait_for(|&closed| closed) => return,
                    res = tokio::time::timeout(
                        RELAY_CONTROL_TIMEOUT,
                        alloc.relay_conn.allow_peer_ips(&job.peer_ips, PERMISSION_TIMEOUT),
                    ) => res,
                };
                match pinhole_res {
                    Ok(Ok(())) => {
                        if let Some(raw) = job.success_response {
                            let _ = alloc.client_conn.send_to(&raw, job.src_addr).await;
                        }
                    }
                    Ok(Err(_)) | Err(_) => {
                        remove_allocation_if_current(&book, &alloc).await;
                        send_authenticated_error(
                            &alloc.client_conn,
                            job.src_addr,
                            &job.req_msg,
                            CODE_ALLOC_MISMATCH,
                            b"Relay Session Closed",
                            None,
                            job.mi_key,
                        )
                        .await;
                        return;
                    }
                }
            }
            WorkerItem::Data(payload, dst) => {
                tokio::select! {
                    _ = closed_rx.wait_for(|&closed| closed) => break,
                    _ = tokio::time::timeout(
                        RELAY_SEND_TIMEOUT,
                        alloc.relay_conn.send_to(&payload, dst),
                    ) => {}
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn complete_async_allocate_open(
    book: Arc<SyncMutex<ServerAllocationBook>>,
    clock: SharedClock,
    mut shutdown_rx: watch::Receiver<bool>,
    mut cancel_rx: watch::Receiver<bool>,
    relay_permit: OwnedSemaphorePermit,
    relay_factory: Arc<dyn TurnDatagramRelayFactory>,
    relay_turn_socket: Arc<dyn Conn + Send + Sync>,
    five_tuple: FiveTuple,
    tuple_key: TurnFiveTupleKey,
    open_id: u64,
    src_addr: SocketAddr,
    req_msg: Message,
    username: String,
    use_ipv4: bool,
    need_even_port: bool,
    lifetime_duration: Duration,
    tcp_authenticated_tx: Option<watch::Sender<Option<u64>>>,
    mi_key: Vec<u8>,
) {
    let open_res = if *shutdown_rx.borrow_and_update() || *cancel_rx.borrow_and_update() {
        None
    } else {
        tokio::select! {
            res = relay_factory.allocate_relay(use_ipv4, need_even_port) => Some(res),
            _ = shutdown_rx.wait_for(|&shut| shut) => None,
            _ = cancel_rx.wait_for(|&cancelled| cancelled) => None,
        }
    };

    let (maybe_relay, outcome_addr) = match open_res {
        Some(Ok(relay)) => {
            let addr = relay.relayed_addr();
            (Some(relay), Ok(addr))
        }
        Some(Err(_)) | None => (None, Err(())),
    };

    let now_ms = clock.now_ms();
    let decision = {
        let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
        guard.pending_cancels.remove(&open_id);
        let dec = guard
            .core
            .complete_allocate(tuple_key, open_id, outcome_addr, now_ms);
        if maybe_relay.is_none() {
            guard.cancelling_open_ids.remove(&open_id);
        }
        dec
    };

    match decision {
        CompleteAllocateDecision::AbortRelay => {
            if let Some(relay) = maybe_relay {
                let handle = relay.begin_close();
                relay.finish_close(handle).await;
                let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
                guard.cancelling_open_ids.remove(&open_id);
            }
            drop(relay_permit);
        }
        CompleteAllocateDecision::RespondOpenError { .. } => {
            if let Some(relay) = maybe_relay {
                let handle = relay.begin_close();
                relay.finish_close(handle).await;
                let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
                guard.cancelling_open_ids.remove(&open_id);
            }
            drop(relay_permit);
            send_authenticated_error(
                &relay_turn_socket,
                src_addr,
                &req_msg,
                CODE_INSUFFICIENT_CAPACITY,
                b"Insufficient Capacity",
                None,
                mi_key,
            )
            .await;
        }
        CompleteAllocateDecision::Installed {
            open_id,
            relayed_addr,
            tx_id,
        } => {
            let Some(relay_conn) = maybe_relay else {
                drop(relay_permit);
                return;
            };
            let built = {
                let mut resp = Message::new();
                let setters: Vec<Box<dyn Setter>> = vec![
                    Box::new(Message {
                        transaction_id: TransactionId(tx_id),
                        ..Default::default()
                    }),
                    Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_SUCCESS_RESPONSE)),
                    Box::new(RelayedAddress {
                        ip: relayed_addr.ip(),
                        port: relayed_addr.port(),
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

            let (alloc, worker_rxs) = ManagedAllocation::new(
                open_id,
                five_tuple,
                username,
                relayed_addr,
                Arc::clone(&relay_turn_socket),
                relay_conn,
                lifetime_duration,
                tcp_authenticated_tx,
                relay_permit,
            );

            let still_live = {
                let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
                guard.cancelling_open_ids.remove(&open_id);
                if guard.core.is_shutting_down()
                    || guard
                        .core
                        .active_allocations()
                        .get(&tuple_key)
                        .is_none_or(|active| active.open_id != open_id)
                {
                    false
                } else {
                    if let Some(ref raw) = built {
                        guard
                            .allocate_responses
                            .insert(five_tuple, (TransactionId(tx_id), raw.clone()));
                    }
                    guard.by_open_id.insert(open_id, Arc::clone(&alloc));
                    guard.by_tuple.insert(five_tuple, Arc::clone(&alloc));
                    true
                }
            };

            if !still_live {
                let taken = alloc.begin_close();
                alloc.finish_close(taken).await;
                return;
            }

            tokio::spawn(allocation_outbound_worker(
                Arc::clone(&alloc),
                Arc::clone(&book),
                worker_rxs.control_rx,
                worker_rxs.outbound_data_rx,
            ));
            tokio::spawn(relay_packet_handler(
                Arc::clone(&alloc),
                Arc::clone(&book),
                clock,
                Arc::clone(&relay_turn_socket),
                src_addr,
            ));

            if let Some(raw) = built {
                let _ = relay_turn_socket.send_to(&raw, src_addr).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_authenticated_allocate(
    conn: &Arc<dyn Conn + Send + Sync>,
    book: &Arc<SyncMutex<ServerAllocationBook>>,
    open_tasks: &mut JoinSet<()>,
    clock: &SharedClock,
    shutdown_rx: &watch::Receiver<bool>,
    relay_factory: &Arc<dyn TurnDatagramRelayFactory>,
    five_tuple: FiveTuple,
    src_addr: SocketAddr,
    m: &Message,
    username_attr: Username,
    verified_auth: &VerifiedTurnAuth,
    mi_key: Vec<u8>,
) {
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
    if !relay_factory.supports_family(requested_ipv4) {
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
            return;
        }
        _ => Arc::clone(conn),
    };
    let tcp_authenticated_tx = tcp_peer_ctx.map(|(_, tx)| tx);

    let ticket_session_id = verified_auth
        .session_ticket()
        .map(|signed_ticket| signed_ticket.ticket.session_id);

    let tuple_key = TurnFiveTupleKey::from(five_tuple);
    let mut now_ms = clock.now_ms();
    for _ in 0..2 {
        let pre_expired = {
            let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
            guard.prune_expired_allocations_and_take(now_ms)
        };
        if pre_expired.is_empty() {
            break;
        }
        close_allocations(pre_expired).await;
        now_ms = clock.now_ms();
    }

    let should_yield_for_cancel = {
        let guard = book.lock().unwrap_or_else(PoisonError::into_inner);
        !guard.cancelling_open_ids.is_empty()
            && guard.relay_permits.available_permits() == 0
            && !guard.core.active_allocations().contains_key(&tuple_key)
            && !guard.core.pending_opens().contains_key(&tuple_key)
    };
    if should_yield_for_cancel {
        tokio::task::yield_now().await;
    }

    let (decision, expired_allocs, cached_replay, pending_ctx) = {
        let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
        let maybe_permit = if !guard.core.active_allocations().contains_key(&tuple_key)
            && !guard.core.pending_opens().contains_key(&tuple_key)
        {
            Arc::clone(&guard.relay_permits).try_acquire_owned().ok()
        } else {
            None
        };
        let extra_busy_slots = if maybe_permit.is_some() {
            0
        } else {
            guard.core.max_allocations()
        };
        let alloc_step = guard.core.begin_allocate_with_busy_slots(
            tuple_key,
            m.transaction_id.0,
            username_attr.text.clone(),
            ticket_session_id,
            lifetime_duration.as_millis(),
            now_ms,
            extra_busy_slots,
        );
        let expired = guard.take_open_ids(alloc_step.expired_open_ids());
        let decision = alloc_step.outcome;
        let cached = if matches!(decision, BeginAllocateOutcome::ReplayActive { .. }) {
            guard
                .allocate_responses
                .get(&five_tuple)
                .and_then(|(saved_tx_id, resp)| {
                    (*saved_tx_id == m.transaction_id).then(|| resp.clone())
                })
        } else {
            None
        };
        let pending_ctx = match (&decision, maybe_permit) {
            (&BeginAllocateOutcome::StartOpen { open_id }, Some(permit)) => {
                let (cancel_tx, cancel_rx) = watch::channel(false);
                guard.pending_cancels.insert(open_id, cancel_tx);
                Some((permit, cancel_rx))
            }
            _ => None,
        };
        (decision, expired, cached, pending_ctx)
    };
    close_allocations(expired_allocs).await;

    match decision {
        BeginAllocateOutcome::ReplayActive { .. } => {
            if let Some(raw) = cached_replay {
                let _ = conn.send_to(&raw, src_addr).await;
            }
        }
        BeginAllocateOutcome::CoalescePending { .. } | BeginAllocateOutcome::RejectShuttingDown => {
        }
        BeginAllocateOutcome::RejectAllocationMismatch => {
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
        BeginAllocateOutcome::RejectTicketUnknown => {
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
        BeginAllocateOutcome::RejectTicketExpired => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_UNAUTHORIZED,
                b"Session Ticket Expired",
                None,
                mi_key,
            )
            .await;
        }
        BeginAllocateOutcome::RejectInsufficientCapacity => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_INSUFFICIENT_CAPACITY,
                b"Allocation Capacity Reached",
                None,
                mi_key,
            )
            .await;
        }
        BeginAllocateOutcome::StartOpen { open_id } => {
            let Some((relay_permit, cancel_rx)) = pending_ctx else {
                {
                    let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
                    let _ = guard
                        .core
                        .complete_allocate(tuple_key, open_id, Err(()), now_ms);
                }
                send_authenticated_error(
                    conn,
                    src_addr,
                    m,
                    CODE_INSUFFICIENT_CAPACITY,
                    b"Allocation Capacity Reached",
                    None,
                    mi_key,
                )
                .await;
                return;
            };
            open_tasks.spawn(complete_async_allocate_open(
                Arc::clone(book),
                Arc::clone(clock),
                shutdown_rx.clone(),
                cancel_rx,
                relay_permit,
                Arc::clone(relay_factory),
                relay_turn_socket,
                five_tuple,
                tuple_key,
                open_id,
                src_addr,
                m.clone(),
                username_attr.text,
                use_ipv4,
                need_even_port,
                lifetime_duration,
                tcp_authenticated_tx,
                mi_key,
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_authenticated_refresh(
    conn: &Arc<dyn Conn + Send + Sync>,
    book: &SyncMutex<ServerAllocationBook>,
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

    let now_ms = clock.now_ms();
    let (decision, to_close) = {
        let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
        let is_current = guard
            .by_tuple
            .get(&alloc.five_tuple)
            .is_some_and(|cur| cur.open_id == alloc.open_id);
        if !is_current {
            (
                RefreshDecision::Mismatch {
                    expired_open_id: None,
                },
                Vec::new(),
            )
        } else {
            let dec = guard
                .core
                .refresh(&alloc.tuple_key, lifetime_duration.as_millis(), now_ms);
            let removed = match dec {
                RefreshDecision::Refreshed { .. } => {
                    alloc.refresh_timer(lifetime_duration);
                    Vec::new()
                }
                RefreshDecision::Deleted { open_id } => guard.take_open_ids(&[open_id]),
                RefreshDecision::Mismatch {
                    expired_open_id: Some(open_id),
                } => guard.take_open_ids(&[open_id]),
                RefreshDecision::Mismatch {
                    expired_open_id: None,
                } => {
                    guard.allocate_responses.remove(&alloc.five_tuple);
                    Vec::new()
                }
            };
            (dec, removed)
        }
    };
    close_allocations(to_close).await;

    if matches!(decision, RefreshDecision::Mismatch { .. }) {
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

async fn handle_authenticated_create_permission(
    conn: &Arc<dyn Conn + Send + Sync>,
    book: &SyncMutex<ServerAllocationBook>,
    clock: &SharedClock,
    alloc: &Arc<ManagedAllocation>,
    m: &Message,
    peer_policy: RelayPeerPolicy,
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
        candidate_ips.push(peer_addr.ip);
    }

    let pinhole_permit = match alloc.try_reserve_pinhole_slot() {
        Ok(permit) => permit,
        Err(PinholeEnqueueError::Full) => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_INSUFFICIENT_CAPACITY,
                b"Control Queue Full",
                None,
                mi_key,
            )
            .await;
            return;
        }
        Err(PinholeEnqueueError::Closed) => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_ALLOC_MISMATCH,
                b"Relay Session Closed",
                None,
                mi_key,
            )
            .await;
            return;
        }
    };

    let now_ms = clock.now_ms();
    let (perm_res, to_close) = {
        let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
        let is_current = guard
            .by_tuple
            .get(&alloc.five_tuple)
            .is_some_and(|cur| cur.open_id == alloc.open_id);
        if !is_current {
            (
                Err(CorePermissionError::AllocationMismatch {
                    expired_open_id: None,
                }),
                Vec::new(),
            )
        } else {
            let res = guard.core.install_permissions(
                &alloc.tuple_key,
                &candidate_ips,
                PERMISSION_TIMEOUT.as_millis(),
                now_ms,
            );
            let removed = match res {
                Err(CorePermissionError::AllocationMismatch {
                    expired_open_id: Some(open_id),
                }) => guard.take_open_ids(&[open_id]),
                Err(CorePermissionError::AllocationMismatch {
                    expired_open_id: None,
                }) => {
                    guard.allocate_responses.remove(&alloc.five_tuple);
                    Vec::new()
                }
                Ok(()) | Err(CorePermissionError::QuotaExceeded) => Vec::new(),
            };
            (res, removed)
        }
    };
    close_allocations(to_close).await;
    match perm_res {
        Ok(()) => {}
        Err(CorePermissionError::AllocationMismatch { .. }) => {
            drop(pinhole_permit);
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
        Err(CorePermissionError::QuotaExceeded) => {
            drop(pinhole_permit);
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
            Box::new(MessageIntegrity(mi_key.clone())),
            Box::new(FINGERPRINT),
        ];
        resp.build(&setters).is_ok().then_some(resp.raw)
    };
    pinhole_permit.send(AllocationPinholeJob {
        src_addr,
        peer_ips: candidate_ips,
        req_msg: m.clone(),
        mi_key,
        success_response: built,
    });
}

#[allow(clippy::too_many_arguments)]
async fn handle_authenticated_channel_bind(
    conn: &Arc<dyn Conn + Send + Sync>,
    book: &SyncMutex<ServerAllocationBook>,
    clock: &SharedClock,
    alloc: &Arc<ManagedAllocation>,
    m: &Message,
    peer_policy: RelayPeerPolicy,
    channel_bind_timeout: Duration,
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

    let now_ms = clock.now_ms();
    if alloc.has_channel_conflict(channel, target_sock, now_ms) {
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

    let pinhole_permit = match alloc.try_reserve_pinhole_slot() {
        Ok(permit) => permit,
        Err(PinholeEnqueueError::Full) => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_INSUFFICIENT_CAPACITY,
                b"Control Queue Full",
                None,
                mi_key,
            )
            .await;
            return;
        }
        Err(PinholeEnqueueError::Closed) => {
            send_authenticated_error(
                conn,
                src_addr,
                m,
                CODE_ALLOC_MISMATCH,
                b"Relay Session Closed",
                None,
                mi_key,
            )
            .await;
            return;
        }
    };

    let (perm_res, to_close) = {
        let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
        let is_current = guard
            .by_tuple
            .get(&alloc.five_tuple)
            .is_some_and(|cur| cur.open_id == alloc.open_id);
        if !is_current {
            (
                Err(CorePermissionError::AllocationMismatch {
                    expired_open_id: None,
                }),
                Vec::new(),
            )
        } else {
            let res = guard.core.install_permissions(
                &alloc.tuple_key,
                &[target_sock.ip()],
                PERMISSION_TIMEOUT.as_millis(),
                now_ms,
            );
            let removed = match res {
                Err(CorePermissionError::AllocationMismatch {
                    expired_open_id: Some(open_id),
                }) => guard.take_open_ids(&[open_id]),
                Err(CorePermissionError::AllocationMismatch {
                    expired_open_id: None,
                }) => {
                    guard.allocate_responses.remove(&alloc.five_tuple);
                    Vec::new()
                }
                Ok(()) | Err(CorePermissionError::QuotaExceeded) => Vec::new(),
            };
            (res, removed)
        }
    };
    close_allocations(to_close).await;
    match perm_res {
        Ok(()) => {}
        Err(CorePermissionError::AllocationMismatch { .. }) => {
            drop(pinhole_permit);
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
        Err(CorePermissionError::QuotaExceeded) => {
            drop(pinhole_permit);
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
            Box::new(MessageIntegrity(mi_key.clone())),
            Box::new(FINGERPRINT),
        ];
        resp.build(&setters).is_ok().then_some(resp.raw)
    };
    alloc.commit_channel_binding(channel, target_sock, channel_bind_timeout, now_ms);
    pinhole_permit.send(AllocationPinholeJob {
        src_addr,
        peer_ips: vec![target_sock.ip()],
        req_msg: m.clone(),
        mi_key,
        success_response: built,
    });
}

async fn shutdown_all_allocations(book: &SyncMutex<ServerAllocationBook>) {
    let removed = {
        let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
        let pending_ids: Vec<u64> = guard.pending_cancels.keys().copied().collect();
        guard.cancel_pending_open_ids(&pending_ids);
        let ids = guard.core.begin_shutdown();
        let mut out = guard.take_open_ids(&ids);
        guard.by_tuple.clear();
        guard.allocate_responses.clear();
        out.extend(guard.by_open_id.drain().map(|(_, alloc)| alloc));
        out
    };
    close_allocations(removed).await;
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
    start_rx: watch::Receiver<bool>,
) -> Result<(SocketAddr, Vec<Arc<RecordingConn>>)> {
    if !listen_addr.ip().is_unspecified() {
        if listen_addr.port() != 0 {
            let socket = UdpSocket::bind(listen_addr).await?;
            let local_addr = socket.local_addr()?;
            let tcp_listener = TcpListener::bind(local_addr).await?;
            let udp_conn: Arc<dyn Conn + Send + Sync> = Arc::new(socket);
            let tcp_conn: Arc<dyn Conn + Send + Sync> = TcpTurnListenerConn::spawn(
                tcp_listener,
                local_addr,
                connection_semaphore,
                start_rx,
            );
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
                    let tcp_conn: Arc<dyn Conn + Send + Sync> = TcpTurnListenerConn::spawn(
                        tcp_listener,
                        local_addr,
                        connection_semaphore,
                        start_rx,
                    );
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
                        start_rx.clone(),
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
                                start_rx.clone(),
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
    max_reservations: usize,
    max_reservation_ttl: Duration,
    book: Arc<SyncMutex<ServerAllocationBook>>,
    nonces: Arc<Mutex<HashMap<String, (FiveTuple, Instant)>>>,
    start_tx: watch::Sender<bool>,
    shutdown_tx: watch::Sender<bool>,
    done_rx: watch::Receiver<bool>,
}

impl TurnServer {
    /// Bind the UDP listener socket and start the TURN server with [`DelegationAuthHandler`].
    pub async fn start(config: TurnServerConfig) -> Result<Self> {
        Self::start_with_gate(config, true).await
    }

    /// Bind the TURN listener sockets and initialize [`TurnServer`] paused so callers can finish
    /// attaching runtime dependencies (such as [`crate::TurnDatagramRelayFactory`]) before calling
    /// [`Self::resume`] to begin reading incoming packets.
    pub async fn start_paused(config: TurnServerConfig) -> Result<Self> {
        Self::start_with_gate(config, false).await
    }

    /// Unblock a server started with [`Self::start_paused`] so its listener reader tasks begin
    /// accepting and processing TURN requests.
    pub fn resume(&self) {
        let _ = self.start_tx.send_replace(true);
    }

    async fn start_with_gate(config: TurnServerConfig, initially_running: bool) -> Result<Self> {
        config.relay.validate()?;
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
        let (start_tx, start_rx) = watch::channel(initially_running);
        let (local_addr, listeners) = bind_listener_sockets(
            config.listen_addr,
            &extra_ips,
            connection_semaphore,
            start_rx.clone(),
        )
        .await?;
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
        let relay_factory: Arc<dyn TurnDatagramRelayFactory> = match config.custom_relay_factory {
            Some(factory) => factory,
            None => Arc::new(PolicyRelayAddressGenerator::new(
                config.relay,
                config.peer_policy,
            )),
        };
        let max_allocations = config.max_allocations.clamp(1, Semaphore::MAX_PERMITS);
        let max_reservations = config.max_reservations;
        let max_reservation_ttl = config.max_reservation_ttl.min(MAXIMUM_ALLOCATION_LIFETIME);
        let max_permissions = config.max_permissions_per_allocation.max(1);
        let book = Arc::new(SyncMutex::new(ServerAllocationBook::new(
            max_allocations,
            max_reservations,
            max_permissions,
        )));
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
            Arc::clone(&book),
            Arc::clone(&clock),
            relay_factory,
            Arc::clone(&nonces),
            Arc::clone(&auth_handler),
            config.realm.clone(),
            channel_bind_timeout,
            config.peer_policy,
            config.max_nonces.max(1),
            max_allocations,
            start_rx,
            shutdown_rx,
            done_tx,
        ));

        Ok(Self {
            local_addr,
            advertised_addr,
            realm: config.realm,
            auth_handler,
            clock,
            max_reservations,
            max_reservation_ttl,
            book,
            nonces,
            start_tx,
            shutdown_tx,
            done_rx,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn read_loop(
        listeners: Vec<Arc<RecordingConn>>,
        book: Arc<SyncMutex<ServerAllocationBook>>,
        clock: SharedClock,
        relay_factory: Arc<dyn TurnDatagramRelayFactory>,
        nonces: Arc<Mutex<HashMap<String, (FiveTuple, Instant)>>>,
        auth_handler: Arc<DelegationAuthHandler>,
        realm: String,
        channel_bind_timeout: Duration,
        peer_policy: RelayPeerPolicy,
        max_nonces: usize,
        max_allocations: usize,
        mut start_rx: watch::Receiver<bool>,
        mut shutdown_rx: watch::Receiver<bool>,
        done_tx: watch::Sender<bool>,
    ) {
        while !*start_rx.borrow_and_update() {
            if *shutdown_rx.borrow_and_update() {
                for listener in &listeners {
                    listener.shutdown_listener().await;
                }
                drop(listeners);
                let _ = done_tx.send_replace(true);
                return;
            }
            tokio::select! {
                res = start_rx.changed() => {
                    if res.is_err() {
                        for listener in &listeners {
                            listener.shutdown_listener().await;
                        }
                        drop(listeners);
                        let _ = done_tx.send_replace(true);
                        return;
                    }
                }
                res = shutdown_rx.changed() => {
                    if res.is_err() || *shutdown_rx.borrow() {
                        for listener in &listeners {
                            listener.shutdown_listener().await;
                        }
                        drop(listeners);
                        let _ = done_tx.send_replace(true);
                        return;
                    }
                }
            }
        }

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
        let mut open_tasks: JoinSet<()> = JoinSet::new();

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
                Some(_) = open_tasks.join_next(), if !open_tasks.is_empty() => {
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

            if payload.is_empty() && recording_conn.protocol == PROTO_TCP {
                let (removed, had_pending_cancel) = {
                    let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
                    let tuple_key = TurnFiveTupleKey::from(five_tuple);
                    guard.allocate_responses.remove(&five_tuple);
                    let pending_open_id = guard
                        .core
                        .pending_opens()
                        .get(&tuple_key)
                        .map(|p| p.open_id);
                    if let Some(pid) = pending_open_id {
                        guard.cancel_pending_open_ids(&[pid]);
                    }
                    let active_removed = match guard.core.disconnect_tuple(&tuple_key) {
                        Some(open_id) => guard.take_open_ids(&[open_id]),
                        None => Vec::new(),
                    };
                    (active_removed, pending_open_id.is_some())
                };
                if had_pending_cancel {
                    tokio::task::yield_now().await;
                }
                close_allocations(removed).await;
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
                    let now_ms = clock.now_ms();
                    if let Some(alloc) = get_active_allocation(&book, &five_tuple, now_ms).await {
                        if let Some(peer) = alloc.outbound_channel_peer(channel_data.number, now_ms)
                        {
                            let permitted = {
                                let mut guard = book.lock().unwrap_or_else(PoisonError::into_inner);
                                guard
                                    .core
                                    .has_permission(&alloc.tuple_key, peer.ip(), now_ms)
                            };
                            if permitted {
                                alloc.enqueue_payload_to_peer(&channel_data.data, peer);
                            }
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
                            let now_ms = clock.now_ms();
                            if let Some(alloc) =
                                get_active_allocation(&book, &five_tuple, now_ms).await
                            {
                                let permitted = {
                                    let mut guard =
                                        book.lock().unwrap_or_else(PoisonError::into_inner);
                                    guard
                                        .core
                                        .has_permission(&alloc.tuple_key, dst.ip(), now_ms)
                                };
                                if permitted {
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

            let now_ms = clock.now_ms();
            let existing_alloc = get_active_allocation(&book, &five_tuple, now_ms).await;

            let expected_owner = if decoded_msg.typ.method == METHOD_ALLOCATE {
                let guard = book.lock().unwrap_or_else(PoisonError::into_inner);
                guard.expected_owner_for_request(&five_tuple, decoded_msg.transaction_id, true)
            } else if is_zero_lifetime_refresh(&decoded_msg) {
                completed_deletions
                    .lock()
                    .await
                    .get(&five_tuple)
                    .and_then(|q| {
                        q.iter()
                            .rev()
                            .find(|entry| entry.transaction_id == decoded_msg.transaction_id)
                            .map(|entry| entry.username.clone())
                    })
                    .or_else(|| existing_alloc.as_ref().map(|alloc| alloc.username.clone()))
            } else {
                existing_alloc.as_ref().map(|alloc| alloc.username.clone())
            };
            let auth_res = authenticate_turn_request(
                &recording_conn_dyn,
                &nonces,
                max_nonces,
                &auth_handler,
                &realm,
                five_tuple,
                expected_owner.as_deref(),
                &decoded_msg,
            )
            .await;

            let Some((username_attr, verified_auth, mi_key)) = auth_res else {
                continue;
            };

            match decoded_msg.typ.method {
                METHOD_ALLOCATE => {
                    handle_authenticated_allocate(
                        &recording_conn_dyn,
                        &book,
                        &mut open_tasks,
                        &clock,
                        &shutdown_rx,
                        &relay_factory,
                        five_tuple,
                        addr,
                        &decoded_msg,
                        username_attr,
                        &verified_auth,
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
                        &book,
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
                        &book,
                        &clock,
                        &alloc,
                        &decoded_msg,
                        peer_policy,
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
                        &book,
                        &clock,
                        &alloc,
                        &decoded_msg,
                        peer_policy,
                        channel_bind_timeout,
                        mi_key,
                    )
                    .await;
                }
                _ => {}
            }
        }

        drop(pkt_rx);
        shutdown_all_allocations(&book).await;
        while open_tasks.join_next().await.is_some() {}
        shutdown_all_allocations(&book).await;
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
        let expired: Vec<Arc<ManagedAllocation>>;
        let mut infos = HashMap::new();
        {
            let mut guard = self.book.lock().unwrap_or_else(PoisonError::into_inner);
            expired = guard.prune_and_take_expired(now_ms);
            match five_tuples {
                Some(tuples) => {
                    for tuple in tuples {
                        if let Some(alloc) = guard.by_tuple.get(&tuple) {
                            infos.insert(tuple, alloc.info());
                        }
                    }
                }
                None => {
                    for (tuple, alloc) in &guard.by_tuple {
                        infos.insert(*tuple, alloc.info());
                    }
                }
            }
        }
        close_allocations(expired).await;
        Ok(infos)
    }

    /// Delete all active TURN allocations associated with `username`.
    pub async fn delete_allocations_by_username(&self, username: String) -> Result<()> {
        let now_ms = self.clock.now_ms();
        let (removed, had_pending_cancel) = {
            let mut guard = self.book.lock().unwrap_or_else(PoisonError::into_inner);
            let pending_ids: Vec<u64> = guard
                .core
                .pending_opens()
                .values()
                .filter(|p| p.username == username)
                .map(|p| p.open_id)
                .collect();
            let had_pending = !pending_ids.is_empty();
            guard.cancel_pending_open_ids(&pending_ids);
            let ids = guard.core.remove_by_username(&username, now_ms);
            (guard.take_open_ids(&ids), had_pending)
        };
        if had_pending_cancel {
            tokio::task::yield_now().await;
        }
        close_allocations(removed).await;
        Ok(())
    }

    /// Return the current number of unclaimed live session reservations held in memory.
    pub async fn active_reservation_count(&self) -> usize {
        let now_ms = self.clock.now_ms();
        let (count, expired) = {
            let mut guard = self.book.lock().unwrap_or_else(PoisonError::into_inner);
            let expired = guard.prune_and_take_expired(now_ms);
            (guard.core.reservation_len(), expired)
        };
        close_allocations(expired).await;
        count
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
        let expires_at_ms = now_ms.saturating_add(clamped_ttl.as_millis());

        let mut session_id = [0u8; 16];
        rand::thread_rng().fill(&mut session_id);
        let ticket = SignedTurnSessionTicket::issue(
            session_id,
            ephemeral_client_pubkey,
            expires_at_ms,
            self.auth_handler.server_key(),
        )?;

        let reserve_res = {
            let mut guard = self.book.lock().unwrap_or_else(PoisonError::into_inner);
            guard.core.reserve_ticket(session_id, expires_at_ms, now_ms)
        };
        if let Err(err) = reserve_res {
            return match err {
                CoreReserveError::CapacityReached
                | CoreReserveError::DuplicateSessionId
                | CoreReserveError::ShuttingDown => {
                    Err(crate::error::TurnError::ReservationCapacityReached {
                        max_reservations: self.max_reservations,
                    })
                }
            };
        }

        Ok(TurnSessionReservation {
            ticket,
            server_addr: self.advertised_addr,
            server_pubkey: self.server_pubkey(),
        })
    }

    /// Gracefully stop the TURN server and close all managed connections and relay allocations.
    pub async fn close(&self) -> Result<()> {
        let _ = self.shutdown_tx.send_replace(true);
        let mut done_rx = self.done_rx.clone();
        let _ = done_rx.wait_for(|&done| done).await;
        shutdown_all_allocations(&self.book).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::net::Ipv4Addr;
    use std::net::SocketAddr;

    use turn::allocation::five_tuple::FiveTuple;
    use turn::proto::PROTO_UDP;

    use super::ServerAllocationBook;
    use crate::relay_backend::BeginAllocateOutcome;
    use crate::relay_backend::TurnFiveTupleKey;

    #[test]
    fn get_active_preserves_expired_ticket_reservations_for_allocate_rejection() {
        let mut book = ServerAllocationBook::new(2, 4, 4);
        let five_tuple = FiveTuple {
            src_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41_000),
            dst_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3_478),
            protocol: PROTO_UDP,
        };
        let sid = [0x42u8; 16];
        book.core.reserve_ticket(sid, 1_500, 1_000).unwrap();

        let (found, expired) = book.get_active(&five_tuple, 1_501);
        assert!(found.is_none());
        assert!(expired.is_empty());

        let step = book.core.begin_allocate(
            TurnFiveTupleKey::from(five_tuple),
            [7u8; 12],
            "ticket-user".to_string(),
            Some(sid),
            1_000,
            1_501,
        );
        assert_eq!(step.outcome, BeginAllocateOutcome::RejectTicketExpired);
    }
}
