use std::collections::HashMap;
use std::collections::HashSet;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::PoisonError;
use std::sync::Weak;

use async_trait::async_trait;
use rand::Rng;
use rings_core::delegation::format_turn_server_url;
use rings_core::delegation::DelegateeKey;
use rings_core::ecc::PublicKey;
use stun::agent::TransactionId;
use stun::attributes::AttrType;
use stun::attributes::ATTR_DONT_FRAGMENT;
use stun::attributes::ATTR_EVEN_PORT;
use stun::attributes::ATTR_FINGERPRINT;
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
use stun::error_code::CODE_BAD_REQUEST;
use stun::error_code::CODE_FORBIDDEN;
use stun::error_code::CODE_INSUFFICIENT_CAPACITY;
use stun::error_code::CODE_PEER_ADDR_FAMILY_MISMATCH;
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
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::time::Duration;
use tokio::time::Instant;
use turn::allocation::allocation_manager::Manager;
use turn::allocation::allocation_manager::ManagerConfig;
use turn::allocation::channel_bind::ChannelBind;
use turn::allocation::five_tuple::FiveTuple;
use turn::allocation::permission::Permission;
use turn::allocation::Allocation;
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
use turn::proto::PROTO_UDP;
use turn::relay::RelayAddressGenerator;
use turn::server::request::Request;
use webrtc_util::vnet::net::Net;
use webrtc_util::Conn;

use crate::auth::DelegationAuthHandler;
use crate::config::RelayAddressConfig;
use crate::config::RelayPeerPolicy;
use crate::config::TurnServerConfig;
use crate::config::DEFAULT_PORT_RANGE_MAX_RETRIES;
use crate::error::Result;

const INBOUND_MTU: usize = 65_535;
const STUN_HEADER_BYTES: usize = 20;
const STUN_ATTR_HEADER_BYTES: usize = 4;
const MAXIMUM_ALLOCATION_LIFETIME: Duration = Duration::from_secs(3600);
const NONCE_LIFETIME: Duration = Duration::from_secs(3600);

struct PolicyRelayConn {
    inner: SyncMutex<Option<Arc<dyn Conn + Send + Sync>>>,
    local_addr: SocketAddr,
    peer_policy: RelayPeerPolicy,
    closed_notify: Notify,
    recv_lock: Mutex<()>,
}

impl PolicyRelayConn {
    fn new(
        inner: Arc<dyn Conn + Send + Sync>,
        local_addr: SocketAddr,
        peer_policy: RelayPeerPolicy,
    ) -> Self {
        Self {
            inner: SyncMutex::new(Some(inner)),
            local_addr,
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
}

#[async_trait]
impl Conn for PolicyRelayConn {
    async fn connect(&self, addr: SocketAddr) -> webrtc_util::Result<()> {
        if !self.peer_policy.is_allowed_addr(addr) {
            return Err(webrtc_util::Error::Other(
                "relay destination address denied by policy".to_string(),
            ));
        }
        let inner = self.clone_inner()?;
        inner.connect(addr).await
    }

    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        let _guard = self.recv_lock.lock().await;
        loop {
            let notified = self.closed_notify.notified();
            let inner = self.clone_inner()?;
            let res = tokio::select! {
                _ = notified => return Err(webrtc_util::Error::Other("relay socket closed".to_string())),
                res = inner.recv(buf) => res,
            };
            match res {
                Ok(n) => return Ok(n),
                Err(err) => {
                    if self.clone_inner().is_err() {
                        return Err(err);
                    }
                    tracing::debug!("ignoring transient relay recv error: {err}");
                }
            }
        }
    }

    async fn recv_from(&self, _buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        // `turn::allocation::Allocation::packet_handler` calls `Conn::recv_from` with a fixed
        // 1500-byte buffer (`RTP_MTU`), and its `Err(_)` branch removes `five_tuple` from the
        // manager's map without checking allocation identity. Keep this future pending forever so
        // `Allocation::packet_handler` only exits via its `drop_rx` arm when the `Allocation` is
        // dropped, while `relay_packet_handler` reads complete UDP datagrams up to `INBOUND_MTU`
        // (65,535 bytes) via `PolicyRelayConn::recv_datagram`.
        std::future::pending().await
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        let inner = self.clone_inner()?;
        inner.send(buf).await
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        if !self.peer_policy.is_allowed_addr(target) {
            return Err(webrtc_util::Error::Other(
                "relay destination address denied by policy".to_string(),
            ));
        }
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
        let taken = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.closed_notify.notify_waiters();
        let _guard = self.recv_lock.lock().await;
        if let Some(inner) = taken {
            let _ = inner.close().await;
        }
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

struct PolicyRelayAddressGenerator {
    relay: RelayAddressConfig,
    peer_policy: RelayPeerPolicy,
    require_even_port: Arc<AtomicBool>,
    last_allocated_conn: Arc<SyncMutex<Option<Arc<PolicyRelayConn>>>>,
    net: Arc<Net>,
}

impl PolicyRelayAddressGenerator {
    fn new(
        relay: RelayAddressConfig,
        peer_policy: RelayPeerPolicy,
        require_even_port: Arc<AtomicBool>,
        last_allocated_conn: Arc<SyncMutex<Option<Arc<PolicyRelayConn>>>>,
    ) -> Self {
        Self {
            relay,
            peer_policy,
            require_even_port,
            last_allocated_conn,
            net: Arc::new(Net::new(None)),
        }
    }

    async fn bind_port(
        &self,
        use_ipv4: bool,
        bind_ip: IpAddr,
        relay_ip_override: Option<IpAddr>,
        port: u16,
    ) -> std::result::Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), turn::Error> {
        if use_ipv4 != bind_ip.is_ipv4() {
            return Err(turn::Error::ErrInvalidRequestedFamilyValue);
        }
        let addr = SocketAddr::new(bind_ip, port);
        let conn = self.net.bind(addr).await?;
        let bound_local_addr = conn.local_addr()?;
        let mut relay_addr = bound_local_addr;
        if let Some(relay_ip) = relay_ip_override {
            relay_addr.set_ip(relay_ip);
        }
        let policy_conn = Arc::new(PolicyRelayConn::new(
            conn,
            bound_local_addr,
            self.peer_policy,
        ));
        *self
            .last_allocated_conn
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&policy_conn));
        let wrapped: Arc<dyn Conn + Send + Sync> = policy_conn;
        Ok((wrapped, relay_addr))
    }
}

#[async_trait]
impl RelayAddressGenerator for PolicyRelayAddressGenerator {
    fn validate(&self) -> std::result::Result<(), turn::Error> {
        self.relay
            .validate()
            .map_err(|_| turn::Error::ErrMaxPortLessThanMinPort)
    }

    async fn allocate_conn(
        &self,
        use_ipv4: bool,
        requested_port: u16,
    ) -> std::result::Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), turn::Error> {
        let need_even = requested_port == 0 && self.require_even_port.swap(false, Ordering::AcqRel);
        match self.relay {
            RelayAddressConfig::None { bind_ip } => {
                if requested_port != 0 {
                    return self
                        .bind_port(use_ipv4, bind_ip, None, requested_port)
                        .await;
                }
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
                if requested_port != 0 {
                    return self
                        .bind_port(use_ipv4, bind_ip, Some(relay_ip), requested_port)
                        .await;
                }
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
                if requested_port != 0 {
                    if requested_port < min_port || requested_port > max_port {
                        return Err(turn::Error::ErrMaxRetriesExceeded);
                    }
                    return self
                        .bind_port(use_ipv4, bind_ip, Some(relay_ip), requested_port)
                        .await;
                }
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

struct RecordingConn {
    inner: SyncMutex<Option<Arc<dyn Conn + Send + Sync>>>,
    local_addr: SocketAddr,
}

impl RecordingConn {
    fn new(inner: Arc<dyn Conn + Send + Sync>, local_addr: SocketAddr) -> Self {
        Self {
            inner: SyncMutex::new(Some(inner)),
            local_addr,
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
        // `turn::allocation::Allocation::close` invokes `turn_socket.close()`. Keep this a no-op
        // so closing a single client allocation does not tear down the shared server listener;
        // `shutdown_listener` is called explicitly when `TurnServer::read_loop` exits.
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

#[derive(Clone)]
enum ServerCommand {
    Close(Arc<mpsc::Receiver<()>>),
}

async fn prune_and_bound_nonces(
    nonces: &Mutex<HashMap<String, (SocketAddr, Instant)>>,
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

async fn extract_client_nonces(
    nonces: &Mutex<HashMap<String, (SocketAddr, Instant)>>,
    client_addr: SocketAddr,
) -> HashMap<String, Instant> {
    let now = Instant::now();
    let guard = nonces.lock().await;
    guard
        .iter()
        .filter(|(_, (owner_addr, created_at))| {
            *owner_addr == client_addr
                && now.checked_duration_since(*created_at).unwrap_or_default() < NONCE_LIFETIME
        })
        .map(|(nonce, (_, created_at))| (nonce.clone(), *created_at))
        .collect()
}

async fn sync_client_nonces(
    nonces: &Mutex<HashMap<String, (SocketAddr, Instant)>>,
    client_addr: SocketAddr,
    before_keys: &HashSet<String>,
    after_map: HashMap<String, Instant>,
    max_nonces: usize,
) {
    {
        let mut guard = nonces.lock().await;
        for key in before_keys {
            if !after_map.contains_key(key) {
                guard.remove(key);
            }
        }
        for (nonce, created_at) in after_map {
            if !before_keys.contains(&nonce) {
                guard.insert(nonce, (client_addr, created_at));
            }
        }
    }
    prune_and_bound_nonces(nonces, max_nonces, false).await;
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

fn verify_authenticated_request(
    m: &Message,
    client_nonces: &HashMap<String, Instant>,
    auth_handler: &DelegationAuthHandler,
    src_addr: SocketAddr,
) -> Option<Vec<u8>> {
    if !m.contains(ATTR_MESSAGE_INTEGRITY) {
        return None;
    }
    let mut nonce_attr = Nonce::new(ATTR_NONCE, String::new());
    nonce_attr.get_from(m).ok()?;
    let created_at = client_nonces.get(&nonce_attr.text)?;
    if Instant::now()
        .checked_duration_since(*created_at)
        .unwrap_or_default()
        >= NONCE_LIFETIME
    {
        return None;
    }
    let mut realm_attr = Realm::new(ATTR_REALM, String::new());
    realm_attr.get_from(m).ok()?;
    let mut username_attr = Username::new(ATTR_USERNAME, String::new());
    username_attr.get_from(m).ok()?;
    let key = auth_handler
        .auth_handle(
            &username_attr.to_string(),
            &realm_attr.to_string(),
            src_addr,
        )
        .ok()?;
    let mi = MessageIntegrity(key.clone());
    mi.check(&mut m.clone()).ok()?;
    Some(key)
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

async fn relay_packet_handler(
    alloc: Weak<Allocation>,
    relay_conn: Arc<PolicyRelayConn>,
    turn_socket: Arc<dyn Conn + Send + Sync>,
    client_addr: SocketAddr,
) {
    let mut buffer = vec![0u8; INBOUND_MTU];
    while let Ok((n, src_addr)) = relay_conn.recv_datagram(&mut buffer).await {
        let Some(payload) = buffer.get(..n) else {
            continue;
        };
        let Some(alloc) = alloc.upgrade() else {
            break;
        };
        // RFC 5766 Section 10.3: always verify that an active permission exists for the peer's IP
        // before forwarding via either ChannelData or DataIndication.
        if !alloc.has_permission(&src_addr).await {
            continue;
        }
        if let Some(number) = alloc.get_channel_number(&src_addr).await {
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

async fn check_permission_budget(
    alloc: &Allocation,
    permission_tracker: &mut HashMap<FiveTuple, HashSet<IpAddr>>,
    five_tuple: FiveTuple,
    candidate_ips: &[IpAddr],
    max_permissions_per_allocation: usize,
) -> Option<HashSet<IpAddr>> {
    let tracked = permission_tracker.entry(five_tuple).or_default();
    let existing_ips: Vec<IpAddr> = tracked.iter().copied().collect();
    for ip in existing_ips {
        if !alloc.has_permission(&SocketAddr::new(ip, 0)).await {
            tracked.remove(&ip);
        }
    }
    let mut projected = tracked.clone();
    for &ip in candidate_ips {
        projected.insert(ip);
    }
    if projected.len() > max_permissions_per_allocation {
        return None;
    }
    Some(projected)
}

#[allow(clippy::too_many_arguments)]
async fn handle_authenticated_allocate(
    conn: &Arc<dyn Conn + Send + Sync>,
    allocation_manager: &Manager,
    require_even_port: &AtomicBool,
    last_allocated_conn: &SyncMutex<Option<Arc<PolicyRelayConn>>>,
    allocate_responses: &mut HashMap<FiveTuple, (TransactionId, Vec<u8>)>,
    permission_tracker: &mut HashMap<FiveTuple, HashSet<IpAddr>>,
    five_tuple: FiveTuple,
    src_addr: SocketAddr,
    m: &Message,
    relay_is_ipv4: bool,
    mi_key: Vec<u8>,
) {
    let mut username_attr = Username::new(ATTR_USERNAME, String::new());
    if username_attr.get_from(m).is_err() {
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_BAD_REQUEST,
            b"Missing Username",
            None,
            mi_key,
        )
        .await;
        return;
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
    if requested_ipv4 != relay_is_ipv4 {
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
    let mut lifetime = Lifetime::default();
    if lifetime.get_from(m).is_ok() && lifetime.0 < MAXIMUM_ALLOCATION_LIFETIME {
        lifetime_duration = lifetime.0;
    }

    *last_allocated_conn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    require_even_port.store(need_even_port, Ordering::Release);
    let create_res = allocation_manager
        .create_allocation(
            five_tuple,
            Arc::clone(conn),
            0,
            lifetime_duration,
            username_attr,
            use_ipv4,
        )
        .await;
    require_even_port.store(false, Ordering::Release);
    let allocated_relay_conn = last_allocated_conn
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();

    let Ok(alloc) = create_res else {
        send_authenticated_error(
            conn,
            src_addr,
            m,
            CODE_INSUFFICIENT_CAPACITY,
            b"Insufficient Capacity",
            None,
            mi_key,
        )
        .await;
        return;
    };

    if let Some(relay_conn) = allocated_relay_conn {
        tokio::spawn(relay_packet_handler(
            Arc::downgrade(&alloc),
            relay_conn,
            Arc::clone(conn),
            src_addr,
        ));
    }

    let Some(alloc_info) = allocation_manager
        .get_allocations_info(Some(vec![five_tuple]))
        .await
        .remove(&five_tuple)
    else {
        return;
    };

    let built = {
        let mut resp = Message::new();
        let setters: Vec<Box<dyn Setter>> = vec![
            Box::new(Message {
                transaction_id: m.transaction_id,
                ..Default::default()
            }),
            Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_SUCCESS_RESPONSE)),
            Box::new(RelayedAddress {
                ip: alloc_info.relay_addr.ip(),
                port: alloc_info.relay_addr.port(),
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
    if let Some(raw) = built {
        let active = allocation_manager.get_allocations_info(None).await;
        allocate_responses.retain(|tuple, _| active.contains_key(tuple));
        permission_tracker.retain(|tuple, _| active.contains_key(tuple));
        permission_tracker.remove(&five_tuple);
        allocate_responses.insert(five_tuple, (m.transaction_id, raw.clone()));
        let _ = conn.send_to(&raw, src_addr).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_authenticated_create_permission(
    conn: &Arc<dyn Conn + Send + Sync>,
    allocation_manager: &Manager,
    permission_tracker: &mut HashMap<FiveTuple, HashSet<IpAddr>>,
    five_tuple: FiveTuple,
    relay_addr: SocketAddr,
    m: &Message,
    peer_policy: RelayPeerPolicy,
    max_permissions_per_allocation: usize,
    mi_key: Vec<u8>,
) {
    let src_addr = five_tuple.src_addr;
    let Some(alloc) = allocation_manager.get_allocation(&five_tuple).await else {
        return;
    };

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

    let Some(projected) = check_permission_budget(
        &alloc,
        permission_tracker,
        five_tuple,
        &candidate_ips,
        max_permissions_per_allocation,
    )
    .await
    else {
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
    };

    for peer_addr in peer_addrs {
        alloc
            .add_permission(Permission::new(SocketAddr::new(
                peer_addr.ip,
                peer_addr.port,
            )))
            .await;
    }
    permission_tracker.insert(five_tuple, projected);

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

#[allow(clippy::too_many_arguments)]
async fn handle_authenticated_channel_bind(
    conn: &Arc<dyn Conn + Send + Sync>,
    allocation_manager: &Manager,
    permission_tracker: &mut HashMap<FiveTuple, HashSet<IpAddr>>,
    five_tuple: FiveTuple,
    relay_addr: SocketAddr,
    m: &Message,
    peer_policy: RelayPeerPolicy,
    channel_bind_timeout: Duration,
    max_permissions_per_allocation: usize,
    mi_key: Vec<u8>,
) {
    let src_addr = five_tuple.src_addr;
    let Some(alloc) = allocation_manager.get_allocation(&five_tuple).await else {
        return;
    };

    let mut channel = ChannelNumber::default();
    let mut peer_addr = PeerAddress::default();
    if channel.get_from(m).is_err() || peer_addr.get_from(m).is_err() {
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

    let Some(projected) = check_permission_budget(
        &alloc,
        permission_tracker,
        five_tuple,
        &[peer_addr.ip],
        max_permissions_per_allocation,
    )
    .await
    else {
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
    };

    if alloc
        .add_channel_bind(ChannelBind::new(channel, target_sock), channel_bind_timeout)
        .await
        .is_err()
    {
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
    permission_tracker.insert(five_tuple, projected);

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

async fn delete_all_allocations(allocation_manager: &Manager) {
    let tuples: Vec<FiveTuple> = allocation_manager
        .get_allocations_info(None)
        .await
        .into_keys()
        .collect();
    for five_tuple in tuples {
        allocation_manager.delete_allocation(&five_tuple).await;
    }
    let _ = allocation_manager.close().await;
}

/// A running Rings TURN server backed by [`DelegationAuthHandler`].
pub struct TurnServer {
    local_addr: SocketAddr,
    advertised_addr: SocketAddr,
    realm: String,
    auth_handler: Arc<DelegationAuthHandler>,
    allocation_manager: Arc<Manager>,
    nonces: Arc<Mutex<HashMap<String, (SocketAddr, Instant)>>>,
    command_tx: Mutex<Option<broadcast::Sender<ServerCommand>>>,
}

impl TurnServer {
    /// Bind the UDP listener socket and start the TURN server with [`DelegationAuthHandler`].
    pub async fn start(config: TurnServerConfig) -> Result<Self> {
        config.relay.validate()?;

        let socket = UdpSocket::bind(config.listen_addr).await?;
        let local_addr = socket.local_addr()?;
        let advertised_ip = match config.relay.advertised_ip() {
            ip if !ip.is_unspecified() => ip,
            _ if !local_addr.ip().is_unspecified() => local_addr.ip(),
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        };
        let advertised_addr = SocketAddr::new(advertised_ip, local_addr.port());
        let conn: Arc<dyn Conn + Send + Sync> = Arc::new(socket);

        let auth_handler = Arc::new(DelegationAuthHandler::with_server_key_and_clock(
            config.admission,
            config.server_key,
            config.clock,
        ));
        let relay_is_ipv4 = relay_bind_ip_is_ipv4(&config.relay);
        let require_even_port = Arc::new(AtomicBool::new(false));
        let last_allocated_conn = Arc::new(SyncMutex::new(None));
        let relay_addr_generator = Box::new(PolicyRelayAddressGenerator::new(
            config.relay,
            config.peer_policy,
            Arc::clone(&require_even_port),
            Arc::clone(&last_allocated_conn),
        ));
        let allocation_manager = Arc::new(Manager::new(ManagerConfig {
            relay_addr_generator,
            alloc_close_notify: None,
        }));
        let nonces = Arc::new(Mutex::new(HashMap::new()));
        let (command_tx, command_rx) = broadcast::channel(16);
        let channel_bind_timeout = if config.channel_bind_timeout.is_zero() {
            DEFAULT_LIFETIME
        } else {
            config.channel_bind_timeout
        };

        tokio::spawn(Self::read_loop(
            conn,
            Arc::clone(&allocation_manager),
            Arc::clone(&nonces),
            Arc::clone(&auth_handler),
            config.realm.clone(),
            channel_bind_timeout,
            config.peer_policy,
            config.max_nonces.max(1),
            config.max_permissions_per_allocation.max(1),
            relay_is_ipv4,
            require_even_port,
            last_allocated_conn,
            command_rx,
        ));

        Ok(Self {
            local_addr,
            advertised_addr,
            realm: config.realm,
            auth_handler,
            allocation_manager,
            nonces,
            command_tx: Mutex::new(Some(command_tx)),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn read_loop(
        conn: Arc<dyn Conn + Send + Sync>,
        allocation_manager: Arc<Manager>,
        nonces: Arc<Mutex<HashMap<String, (SocketAddr, Instant)>>>,
        auth_handler: Arc<DelegationAuthHandler>,
        realm: String,
        channel_bind_timeout: Duration,
        peer_policy: RelayPeerPolicy,
        max_nonces: usize,
        max_permissions_per_allocation: usize,
        relay_is_ipv4: bool,
        require_even_port: Arc<AtomicBool>,
        last_allocated_conn: Arc<SyncMutex<Option<Arc<PolicyRelayConn>>>>,
        mut command_rx: broadcast::Receiver<ServerCommand>,
    ) {
        let mut buf = vec![0u8; INBOUND_MTU];
        let (close_tx, mut close_rx) = oneshot::channel::<Option<Arc<mpsc::Receiver<()>>>>();

        tokio::spawn(async move {
            loop {
                match command_rx.recv().await {
                    Ok(ServerCommand::Close(closed_rx)) => {
                        let _ = close_tx.send(Some(closed_rx));
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = close_tx.send(None);
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });

        let Ok(local_dst_addr) = conn.local_addr() else {
            return;
        };
        let recording_conn = Arc::new(RecordingConn::new(Arc::clone(&conn), local_dst_addr));
        let recording_conn_dyn: Arc<dyn Conn + Send + Sync> =
            Arc::clone(&recording_conn) as Arc<dyn Conn + Send + Sync>;
        let mut allocate_responses: HashMap<FiveTuple, (TransactionId, Vec<u8>)> = HashMap::new();
        let mut permission_tracker: HashMap<FiveTuple, HashSet<IpAddr>> = HashMap::new();
        let mut closed_guard: Option<Arc<mpsc::Receiver<()>>> = None;

        loop {
            let (n, addr) = tokio::select! {
                v = recording_conn.recv_from(&mut buf) => {
                    match v {
                        Ok(v) => v,
                        Err(err) => {
                            if recording_conn.is_closed() {
                                tracing::debug!("exit TURN read loop on closed listener: {err}");
                                break;
                            }
                            tracing::debug!("ignoring transient TURN listener recv_from error: {err}");
                            continue;
                        }
                    }
                }
                res = &mut close_rx => {
                    closed_guard = res.ok().flatten();
                    break;
                }
            };

            let Some(payload) = buf.get(..n) else {
                continue;
            };
            let mut decoded_msg = Message {
                raw: payload.to_vec(),
                ..Default::default()
            };
            let is_stun_msg = decoded_msg.decode().is_ok();
            if is_stun_msg && !sanitize_authenticated_message(&mut decoded_msg) {
                continue;
            }
            let five_tuple = FiveTuple {
                src_addr: addr,
                dst_addr: local_dst_addr,
                protocol: PROTO_UDP,
            };

            if is_stun_msg
                && decoded_msg.typ.class == CLASS_INDICATION
                && decoded_msg.typ.method == METHOD_SEND
                && !check_message_peer_addresses(&decoded_msg, peer_policy)
            {
                continue;
            }

            if is_stun_msg && decoded_msg.typ.class == CLASS_REQUEST {
                let has_mi = decoded_msg.contains(ATTR_MESSAGE_INTEGRITY);
                prune_and_bound_nonces(&nonces, max_nonces, !has_mi).await;

                let existing_alloc = allocation_manager
                    .get_allocations_info(Some(vec![five_tuple]))
                    .await
                    .remove(&five_tuple);
                if existing_alloc.is_none() {
                    allocate_responses.remove(&five_tuple);
                    permission_tracker.remove(&five_tuple);
                }

                if decoded_msg.typ.method == METHOD_ALLOCATE {
                    if let Some(info) = &existing_alloc {
                        if let Some((saved_tx_id, cached_resp)) =
                            allocate_responses.get(&five_tuple)
                        {
                            if *saved_tx_id == decoded_msg.transaction_id {
                                let client_nonces = extract_client_nonces(&nonces, addr).await;
                                auth_handler.set_expected_allocation_owner(
                                    addr,
                                    Some(info.username.clone()),
                                );
                                let valid_retransmit = verify_authenticated_request(
                                    &decoded_msg,
                                    &client_nonces,
                                    &auth_handler,
                                    addr,
                                )
                                .is_some();
                                auth_handler.set_expected_allocation_owner(addr, None);
                                if valid_retransmit {
                                    let _ = recording_conn.send_to(cached_resp, addr).await;
                                    continue;
                                }
                            }
                        }
                    } else {
                        let client_nonces = extract_client_nonces(&nonces, addr).await;
                        if let Some(mi_key) = verify_authenticated_request(
                            &decoded_msg,
                            &client_nonces,
                            &auth_handler,
                            addr,
                        ) {
                            handle_authenticated_allocate(
                                &recording_conn_dyn,
                                &allocation_manager,
                                &require_even_port,
                                &last_allocated_conn,
                                &mut allocate_responses,
                                &mut permission_tracker,
                                five_tuple,
                                addr,
                                &decoded_msg,
                                relay_is_ipv4,
                                mi_key,
                            )
                            .await;
                            continue;
                        }
                    }
                }

                if decoded_msg.typ.method == METHOD_REFRESH
                    || decoded_msg.typ.method == METHOD_CREATE_PERMISSION
                    || decoded_msg.typ.method == METHOD_CHANNEL_BIND
                {
                    if let Some(info) = &existing_alloc {
                        auth_handler
                            .set_expected_allocation_owner(addr, Some(info.username.clone()));
                    }
                }

                if decoded_msg.typ.method == METHOD_CREATE_PERMISSION {
                    if let Some(info) = &existing_alloc {
                        let client_nonces = extract_client_nonces(&nonces, addr).await;
                        if let Some(mi_key) = verify_authenticated_request(
                            &decoded_msg,
                            &client_nonces,
                            &auth_handler,
                            addr,
                        ) {
                            auth_handler.set_expected_allocation_owner(addr, None);
                            handle_authenticated_create_permission(
                                &recording_conn_dyn,
                                &allocation_manager,
                                &mut permission_tracker,
                                five_tuple,
                                info.relay_addr,
                                &decoded_msg,
                                peer_policy,
                                max_permissions_per_allocation,
                                mi_key,
                            )
                            .await;
                            continue;
                        }
                    }
                }

                if decoded_msg.typ.method == METHOD_CHANNEL_BIND {
                    if let Some(info) = &existing_alloc {
                        let client_nonces = extract_client_nonces(&nonces, addr).await;
                        if let Some(mi_key) = verify_authenticated_request(
                            &decoded_msg,
                            &client_nonces,
                            &auth_handler,
                            addr,
                        ) {
                            auth_handler.set_expected_allocation_owner(addr, None);
                            handle_authenticated_channel_bind(
                                &recording_conn_dyn,
                                &allocation_manager,
                                &mut permission_tracker,
                                five_tuple,
                                info.relay_addr,
                                &decoded_msg,
                                peer_policy,
                                channel_bind_timeout,
                                max_permissions_per_allocation,
                                mi_key,
                            )
                            .await;
                            continue;
                        }
                    }
                }
            }

            let client_nonces = if is_stun_msg && decoded_msg.typ.class == CLASS_REQUEST {
                extract_client_nonces(&nonces, addr).await
            } else {
                HashMap::new()
            };
            let before_nonce_keys: HashSet<String> = client_nonces.keys().cloned().collect();
            let req_nonces = Arc::new(Mutex::new(client_nonces));
            let request_buff = if is_stun_msg {
                decoded_msg.raw.clone()
            } else {
                payload.to_vec()
            };

            let mut r = Request {
                conn: Arc::clone(&recording_conn_dyn),
                src_addr: addr,
                buff: request_buff,
                allocation_manager: Arc::clone(&allocation_manager),
                nonces: Arc::clone(&req_nonces),
                auth_handler: Arc::clone(&auth_handler)
                    as Arc<dyn turn::auth::AuthHandler + Send + Sync>,
                realm: realm.clone(),
                channel_bind_timeout,
            };

            if let Err(err) = r.handle_request().await {
                tracing::debug!(%addr, %err, "TURN request handling returned error");
            }

            require_even_port.store(false, Ordering::Release);
            auth_handler.set_expected_allocation_owner(addr, None);

            if is_stun_msg && decoded_msg.typ.class == CLASS_REQUEST {
                let after_map = req_nonces.lock().await.clone();
                sync_client_nonces(&nonces, addr, &before_nonce_keys, after_map, max_nonces).await;
            }
        }

        delete_all_allocations(&allocation_manager).await;
        recording_conn.shutdown_listener().await;
        drop(conn);
        drop(closed_guard);
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
    /// server, suitable for passing to [`DelegateeKey::sign_ice_servers`] or
    /// [`SecretKey::sign_ice_servers`](rings_core::ecc::SecretKey::sign_ice_servers).
    pub fn ice_server_base_url(&self) -> Result<String> {
        let base_url = format!("turn://{}", self.advertised_addr);
        Ok(format_turn_server_url(&base_url, self.server_pubkey())?)
    }

    /// Build a complete signed `turn://<username>:<credential>@<advertised_addr>` URL for `delegatee_key`.
    pub fn ice_server_url_for(&self, delegatee_key: &DelegateeKey) -> Result<String> {
        let base_url = self.ice_server_base_url()?;
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
        Ok(self
            .allocation_manager
            .get_allocations_info(five_tuples)
            .await)
    }

    /// Delete all active TURN allocations associated with `username`.
    pub async fn delete_allocations_by_username(&self, username: String) -> Result<()> {
        self.allocation_manager
            .delete_allocations_by_username(&username)
            .await;
        Ok(())
    }

    /// Gracefully stop the TURN server and close all managed connections and relay allocations.
    pub async fn close(&self) -> Result<()> {
        let tx = {
            let mut command_tx = self.command_tx.lock().await;
            command_tx.take()
        };
        if let Some(tx) = tx {
            if tx.receiver_count() > 0 {
                let (closed_tx, closed_rx) = mpsc::channel(1);
                let _ = tx.send(ServerCommand::Close(Arc::new(closed_rx)));
                closed_tx.closed().await;
            }
        }
        delete_all_allocations(&self.allocation_manager).await;
        Ok(())
    }
}
