use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::PoisonError;

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
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::sync::Notify;
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
use turn::proto::PROTO_UDP;
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
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);
const NONCE_LIFETIME: Duration = Duration::from_secs(3600);

struct PolicyRelayConn {
    inner: SyncMutex<Option<Arc<dyn Conn + Send + Sync>>>,
    peer_policy: RelayPeerPolicy,
    closed_notify: Notify,
    recv_lock: Mutex<()>,
}

impl PolicyRelayConn {
    fn new(inner: Arc<dyn Conn + Send + Sync>, peer_policy: RelayPeerPolicy) -> Self {
        Self {
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

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        if !self.peer_policy.is_allowed_addr(target) {
            return Err(webrtc_util::Error::Other(
                "relay destination address denied by policy".to_string(),
            ));
        }
        let inner = self.clone_inner()?;
        inner.send_to(buf, target).await
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

enum ChannelBindError {
    Conflict,
    QuotaExceeded,
}

struct AllocationState {
    permissions: HashMap<IpAddr, Instant>,
    channel_bindings: HashMap<ChannelNumber, (SocketAddr, Instant)>,
}

struct ManagedAllocation {
    five_tuple: FiveTuple,
    username: String,
    relay_addr: SocketAddr,
    relay_conn: Arc<PolicyRelayConn>,
    state: SyncMutex<AllocationState>,
    expiry_tx: watch::Sender<Instant>,
}

impl ManagedAllocation {
    fn new(
        five_tuple: FiveTuple,
        username: String,
        relay_addr: SocketAddr,
        relay_conn: Arc<PolicyRelayConn>,
        lifetime: Duration,
    ) -> Arc<Self> {
        let (expiry_tx, _) = watch::channel(Instant::now() + lifetime);
        Arc::new(Self {
            five_tuple,
            username,
            relay_addr,
            relay_conn,
            state: SyncMutex::new(AllocationState {
                permissions: HashMap::new(),
                channel_bindings: HashMap::new(),
            }),
            expiry_tx,
        })
    }

    fn info(&self) -> AllocationInfo {
        AllocationInfo::new(self.five_tuple, self.username.clone(), self.relay_addr)
    }

    fn is_expired(&self) -> bool {
        *self.expiry_tx.borrow() <= Instant::now()
    }

    fn refresh(&self, lifetime: Duration) {
        let _ = self.expiry_tx.send_replace(Instant::now() + lifetime);
    }

    async fn close(&self) {
        let _ = self.expiry_tx.send_replace(Instant::now());
        let _ = self.relay_conn.close().await;
    }

    fn has_permission(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.permissions.retain(|_, &mut exp| exp > now);
        state.permissions.contains_key(&ip)
    }

    fn try_install_permissions(&self, peer_ips: &[IpAddr], max_permissions: usize) -> bool {
        let now = Instant::now();
        let new_exp = now + PERMISSION_TIMEOUT;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.permissions.retain(|_, &mut exp| exp > now);
        let mut projected_len = state.permissions.len();
        let mut seen_new = HashSet::new();
        for &ip in peer_ips {
            if !state.permissions.contains_key(&ip) && seen_new.insert(ip) {
                projected_len += 1;
            }
        }
        if projected_len > max_permissions {
            return false;
        }
        for &ip in peer_ips {
            state.permissions.insert(ip, new_exp);
        }
        true
    }

    fn try_bind_channel(
        &self,
        channel: ChannelNumber,
        peer: SocketAddr,
        channel_timeout: Duration,
        max_permissions: usize,
    ) -> std::result::Result<(), ChannelBindError> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.permissions.retain(|_, &mut exp| exp > now);
        state.channel_bindings.retain(|_, (_, exp)| *exp > now);

        for (&bound_ch, &(bound_peer, _)) in &state.channel_bindings {
            if (bound_ch == channel && bound_peer != peer)
                || (bound_peer == peer && bound_ch != channel)
            {
                return Err(ChannelBindError::Conflict);
            }
        }

        if !state.permissions.contains_key(&peer.ip()) && state.permissions.len() >= max_permissions
        {
            return Err(ChannelBindError::QuotaExceeded);
        }

        state
            .permissions
            .insert(peer.ip(), now + PERMISSION_TIMEOUT);
        state
            .channel_bindings
            .insert(channel, (peer, now + channel_timeout));
        Ok(())
    }

    fn inbound_forwarding_channel(&self, peer: SocketAddr) -> Option<Option<ChannelNumber>> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.permissions.retain(|_, &mut exp| exp > now);
        state.channel_bindings.retain(|_, (_, exp)| *exp > now);
        if !state.permissions.contains_key(&peer.ip()) {
            return None;
        }
        let channel = state
            .channel_bindings
            .iter()
            .find_map(|(&ch, &(bound_peer, _))| (bound_peer == peer).then_some(ch));
        Some(channel)
    }

    fn outbound_channel_peer(&self, channel: ChannelNumber) -> Option<SocketAddr> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.permissions.retain(|_, &mut exp| exp > now);
        state.channel_bindings.retain(|_, (_, exp)| *exp > now);
        let &(peer, _) = state.channel_bindings.get(&channel)?;
        state.permissions.contains_key(&peer.ip()).then_some(peer)
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
    nonces: &Mutex<HashMap<String, (SocketAddr, Instant)>>,
    max_nonces: usize,
    realm: &str,
    src_addr: SocketAddr,
    m: &Message,
    code: ErrorCode,
    reason: &[u8],
) {
    prune_and_bound_nonces(nonces, max_nonces, true).await;
    let nonce = generate_nonce();
    nonces
        .lock()
        .await
        .insert(nonce.clone(), (src_addr, Instant::now()));
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
    nonces: &Mutex<HashMap<String, (SocketAddr, Instant)>>,
    max_nonces: usize,
    auth_handler: &DelegationAuthHandler,
    realm: &str,
    src_addr: SocketAddr,
    m: &Message,
) -> Option<(Username, Vec<u8>)> {
    if !m.contains(ATTR_MESSAGE_INTEGRITY) {
        issue_nonce_challenge(
            conn,
            nonces,
            max_nonces,
            realm,
            src_addr,
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
            Some((owner_addr, created_at)) if owner_addr == src_addr => {
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
            src_addr,
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

async fn remove_allocation_if_current(
    allocations: &Mutex<HashMap<FiveTuple, Arc<ManagedAllocation>>>,
    alloc: &Arc<ManagedAllocation>,
) {
    let should_close = {
        let mut guard = allocations.lock().await;
        if guard
            .get(&alloc.five_tuple)
            .is_some_and(|current| Arc::ptr_eq(current, alloc))
        {
            guard.remove(&alloc.five_tuple);
            true
        } else {
            false
        }
    };
    if should_close {
        alloc.close().await;
    }
}

async fn try_expire_allocation_if_current(
    allocations: &Mutex<HashMap<FiveTuple, Arc<ManagedAllocation>>>,
    alloc: &Arc<ManagedAllocation>,
) -> bool {
    let should_close = {
        let mut guard = allocations.lock().await;
        match guard.get(&alloc.five_tuple) {
            Some(current) if Arc::ptr_eq(current, alloc) => {
                if !current.is_expired() {
                    return false;
                }
                guard.remove(&alloc.five_tuple);
                true
            }
            _ => return true,
        }
    };
    if should_close {
        alloc.close().await;
    }
    true
}

async fn try_refresh_allocation(
    allocations: &Mutex<HashMap<FiveTuple, Arc<ManagedAllocation>>>,
    alloc: &Arc<ManagedAllocation>,
    lifetime: Duration,
) -> bool {
    let expired_to_close = {
        let mut guard = allocations.lock().await;
        match guard.get(&alloc.five_tuple) {
            Some(current) if Arc::ptr_eq(current, alloc) => {
                if !current.is_expired() {
                    current.refresh(lifetime);
                    return true;
                }
                guard.remove(&alloc.five_tuple)
            }
            _ => None,
        }
    };
    if let Some(expired) = expired_to_close {
        expired.close().await;
    }
    false
}

async fn get_active_allocation(
    allocations: &Mutex<HashMap<FiveTuple, Arc<ManagedAllocation>>>,
    five_tuple: &FiveTuple,
) -> Option<Arc<ManagedAllocation>> {
    let expired = {
        let mut guard = allocations.lock().await;
        let current = guard.get(five_tuple).cloned()?;
        if !current.is_expired() {
            return Some(current);
        }
        guard.remove(five_tuple)
    };
    if let Some(alloc) = expired {
        alloc.close().await;
    }
    None
}

async fn active_allocation_count(
    allocations: &Mutex<HashMap<FiveTuple, Arc<ManagedAllocation>>>,
) -> usize {
    let expired: Vec<Arc<ManagedAllocation>>;
    let count = {
        let mut guard = allocations.lock().await;
        let expired_tuples: Vec<FiveTuple> = guard
            .iter()
            .filter(|(_, alloc)| alloc.is_expired())
            .map(|(k, _)| *k)
            .collect();
        expired = expired_tuples
            .into_iter()
            .filter_map(|k| guard.remove(&k))
            .collect();
        guard.len()
    };
    for alloc in expired {
        alloc.close().await;
    }
    count
}

async fn relay_packet_handler(
    alloc: Arc<ManagedAllocation>,
    allocations: Arc<Mutex<HashMap<FiveTuple, Arc<ManagedAllocation>>>>,
    turn_socket: Arc<dyn Conn + Send + Sync>,
    client_addr: SocketAddr,
) {
    let mut buffer = vec![0u8; INBOUND_MTU];
    let mut expiry_rx = alloc.expiry_tx.subscribe();
    loop {
        let deadline = *expiry_rx.borrow_and_update();
        if deadline <= Instant::now() {
            if try_expire_allocation_if_current(&allocations, &alloc).await {
                return;
            }
            continue;
        }
        let sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(sleep);

        tokio::select! {
            _ = &mut sleep => {
                if try_expire_allocation_if_current(&allocations, &alloc).await {
                    return;
                }
            }
            changed = expiry_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            recv_res = alloc.relay_conn.recv_datagram(&mut buffer) => {
                let Ok((n, src_addr)) = recv_res else {
                    break;
                };
                if *expiry_rx.borrow() <= Instant::now() {
                    if try_expire_allocation_if_current(&allocations, &alloc).await {
                        return;
                    }
                    continue;
                }
                let Some(payload) = buffer.get(..n) else {
                    continue;
                };
                // RFC 5766 Section 10.3: verify that an active permission exists for the peer's IP
                // before forwarding via either ChannelData or DataIndication.
                let Some(maybe_channel) = alloc.inbound_forwarding_channel(src_addr) else {
                    continue;
                };
                if let Some(number) = maybe_channel {
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
    remove_allocation_if_current(&allocations, &alloc).await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_authenticated_allocate(
    conn: &Arc<dyn Conn + Send + Sync>,
    allocations: &Arc<Mutex<HashMap<FiveTuple, Arc<ManagedAllocation>>>>,
    relay_generator: &PolicyRelayAddressGenerator,
    allocate_responses: &mut HashMap<FiveTuple, (TransactionId, Vec<u8>)>,
    max_allocations: usize,
    five_tuple: FiveTuple,
    src_addr: SocketAddr,
    m: &Message,
    username_attr: Username,
    relay_is_ipv4: bool,
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
        if !lifetime.0.is_zero() && lifetime.0 < MAXIMUM_ALLOCATION_LIFETIME {
            lifetime_duration = lifetime.0;
        }
    }

    if active_allocation_count(allocations).await >= max_allocations {
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
    }

    let Ok((relay_conn, relay_addr)) = relay_generator
        .allocate_relay_conn(use_ipv4, need_even_port)
        .await
    else {
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

    let alloc = ManagedAllocation::new(
        five_tuple,
        username_attr.text,
        relay_addr,
        relay_conn,
        lifetime_duration,
    );
    {
        let mut guard = allocations.lock().await;
        guard.insert(five_tuple, Arc::clone(&alloc));
    }

    tokio::spawn(relay_packet_handler(
        Arc::clone(&alloc),
        Arc::clone(allocations),
        Arc::clone(conn),
        src_addr,
    ));

    let built = {
        let mut resp = Message::new();
        let setters: Vec<Box<dyn Setter>> = vec![
            Box::new(Message {
                transaction_id: m.transaction_id,
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
    if let Some(raw) = built {
        {
            let guard = allocations.lock().await;
            allocate_responses.retain(|tuple, _| guard.contains_key(tuple));
        }
        allocate_responses.insert(five_tuple, (m.transaction_id, raw.clone()));
        let _ = conn.send_to(&raw, src_addr).await;
    }
}

async fn handle_authenticated_refresh(
    conn: &Arc<dyn Conn + Send + Sync>,
    allocations: &Mutex<HashMap<FiveTuple, Arc<ManagedAllocation>>>,
    allocate_responses: &mut HashMap<FiveTuple, (TransactionId, Vec<u8>)>,
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
        lifetime_duration = lifetime.0.min(MAXIMUM_ALLOCATION_LIFETIME);
    }

    if lifetime_duration.is_zero() {
        allocate_responses.remove(&alloc.five_tuple);
        remove_allocation_if_current(allocations, &alloc).await;
    } else if !try_refresh_allocation(allocations, &alloc, lifetime_duration).await {
        allocate_responses.remove(&alloc.five_tuple);
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
        let _ = conn.send_to(&raw, src_addr).await;
    }
}

async fn handle_authenticated_create_permission(
    conn: &Arc<dyn Conn + Send + Sync>,
    alloc: &ManagedAllocation,
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

    if !alloc.try_install_permissions(&candidate_ips, max_permissions_per_allocation) {
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

async fn handle_authenticated_channel_bind(
    conn: &Arc<dyn Conn + Send + Sync>,
    alloc: &ManagedAllocation,
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

    match alloc.try_bind_channel(
        channel,
        target_sock,
        channel_bind_timeout,
        max_permissions_per_allocation,
    ) {
        Ok(()) => {}
        Err(ChannelBindError::Conflict) => {
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
        Err(ChannelBindError::QuotaExceeded) => {
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
            Box::new(MessageIntegrity(mi_key)),
            Box::new(FINGERPRINT),
        ];
        resp.build(&setters).is_ok().then_some(resp.raw)
    };
    if let Some(raw) = built {
        let _ = conn.send_to(&raw, src_addr).await;
    }
}

async fn delete_all_allocations(allocations: &Mutex<HashMap<FiveTuple, Arc<ManagedAllocation>>>) {
    let removed: Vec<Arc<ManagedAllocation>> = {
        let mut guard = allocations.lock().await;
        guard.drain().map(|(_, alloc)| alloc).collect()
    };
    for alloc in removed {
        alloc.close().await;
    }
}

/// A running Rings TURN server backed by [`DelegationAuthHandler`].
pub struct TurnServer {
    local_addr: SocketAddr,
    advertised_addr: SocketAddr,
    realm: String,
    auth_handler: Arc<DelegationAuthHandler>,
    allocations: Arc<Mutex<HashMap<FiveTuple, Arc<ManagedAllocation>>>>,
    nonces: Arc<Mutex<HashMap<String, (SocketAddr, Instant)>>>,
    shutdown_tx: watch::Sender<bool>,
    done_rx: watch::Receiver<bool>,
}

impl TurnServer {
    /// Bind the UDP listener socket and start the TURN server with [`DelegationAuthHandler`].
    pub async fn start(config: TurnServerConfig) -> Result<Self> {
        config.relay.validate()?;
        if let Some(advertised_ip) = config.advertised_ip {
            if advertised_ip.is_ipv4() != config.listen_addr.ip().is_ipv4() {
                return Err(crate::error::TurnError::ListenerAddressFamilyMismatch {
                    advertised_ip,
                    listen_ip: config.listen_addr.ip(),
                });
            }
        }

        let socket = UdpSocket::bind(config.listen_addr).await?;
        let local_addr = socket.local_addr()?;
        let relay_advertised = config.relay.advertised_ip();
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
        let conn: Arc<dyn Conn + Send + Sync> = Arc::new(socket);

        let auth_handler = Arc::new(DelegationAuthHandler::with_server_key_and_clock(
            config.admission,
            config.server_key,
            config.clock,
        ));
        let relay_is_ipv4 = relay_bind_ip_is_ipv4(&config.relay);
        let relay_generator = Arc::new(PolicyRelayAddressGenerator::new(
            config.relay,
            config.peer_policy,
        ));
        let allocations = Arc::new(Mutex::new(HashMap::new()));
        let nonces = Arc::new(Mutex::new(HashMap::new()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        let channel_bind_timeout = if config.channel_bind_timeout.is_zero() {
            DEFAULT_LIFETIME
        } else {
            config.channel_bind_timeout
        };

        tokio::spawn(Self::read_loop(
            conn,
            Arc::clone(&allocations),
            relay_generator,
            Arc::clone(&nonces),
            Arc::clone(&auth_handler),
            config.realm.clone(),
            channel_bind_timeout,
            config.peer_policy,
            config.max_nonces.max(1),
            config.max_allocations.max(1),
            config.max_permissions_per_allocation.max(1),
            relay_is_ipv4,
            shutdown_rx,
            done_tx,
        ));

        Ok(Self {
            local_addr,
            advertised_addr,
            realm: config.realm,
            auth_handler,
            allocations,
            nonces,
            shutdown_tx,
            done_rx,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn read_loop(
        conn: Arc<dyn Conn + Send + Sync>,
        allocations: Arc<Mutex<HashMap<FiveTuple, Arc<ManagedAllocation>>>>,
        relay_generator: Arc<PolicyRelayAddressGenerator>,
        nonces: Arc<Mutex<HashMap<String, (SocketAddr, Instant)>>>,
        auth_handler: Arc<DelegationAuthHandler>,
        realm: String,
        channel_bind_timeout: Duration,
        peer_policy: RelayPeerPolicy,
        max_nonces: usize,
        max_allocations: usize,
        max_permissions_per_allocation: usize,
        relay_is_ipv4: bool,
        mut shutdown_rx: watch::Receiver<bool>,
        done_tx: watch::Sender<bool>,
    ) {
        let mut buf = vec![0u8; INBOUND_MTU];
        let Ok(local_dst_addr) = conn.local_addr() else {
            let _ = done_tx.send_replace(true);
            return;
        };
        let recording_conn = Arc::new(RecordingConn::new(Arc::clone(&conn), local_dst_addr));
        let recording_conn_dyn: Arc<dyn Conn + Send + Sync> =
            Arc::clone(&recording_conn) as Arc<dyn Conn + Send + Sync>;
        let mut allocate_responses: HashMap<FiveTuple, (TransactionId, Vec<u8>)> = HashMap::new();

        loop {
            if *shutdown_rx.borrow_and_update() {
                break;
            }
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
                res = shutdown_rx.changed() => {
                    if res.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                    continue;
                }
            };

            let Some(payload) = buf.get(..n) else {
                continue;
            };
            let five_tuple = FiveTuple {
                src_addr: addr,
                dst_addr: local_dst_addr,
                protocol: PROTO_UDP,
            };

            if ChannelData::is_channel_data(payload) {
                let mut channel_data = ChannelData {
                    raw: payload.to_vec(),
                    ..Default::default()
                };
                if channel_data.decode().is_ok() {
                    if let Some(alloc) = get_active_allocation(&allocations, &five_tuple).await {
                        if let Some(peer) = alloc.outbound_channel_peer(channel_data.number) {
                            let _ = alloc.relay_conn.send_to(&channel_data.data, peer).await;
                        }
                    }
                }
                continue;
            }

            let mut decoded_msg = Message {
                raw: payload.to_vec(),
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
                                get_active_allocation(&allocations, &five_tuple).await
                            {
                                if alloc.has_permission(dst.ip()) {
                                    let _ = alloc.relay_conn.send_to(&data.0, dst).await;
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

            let existing_alloc = get_active_allocation(&allocations, &five_tuple).await;
            if existing_alloc.is_none() {
                allocate_responses.remove(&five_tuple);
            }

            if decoded_msg.typ.method == METHOD_REFRESH
                || decoded_msg.typ.method == METHOD_CREATE_PERMISSION
                || decoded_msg.typ.method == METHOD_CHANNEL_BIND
            {
                if let Some(alloc) = &existing_alloc {
                    auth_handler.set_expected_allocation_owner(addr, Some(alloc.username.clone()));
                }
            }

            let auth_res = authenticate_turn_request(
                &recording_conn_dyn,
                &nonces,
                max_nonces,
                &auth_handler,
                &realm,
                addr,
                &decoded_msg,
            )
            .await;
            auth_handler.set_expected_allocation_owner(addr, None);

            let Some((username_attr, mi_key)) = auth_res else {
                continue;
            };

            match decoded_msg.typ.method {
                METHOD_ALLOCATE => {
                    if let Some(alloc) = &existing_alloc {
                        if username_attr.text == alloc.username {
                            if let Some((saved_tx_id, cached_resp)) =
                                allocate_responses.get(&five_tuple)
                            {
                                if *saved_tx_id == decoded_msg.transaction_id {
                                    let _ = recording_conn.send_to(cached_resp, addr).await;
                                    continue;
                                }
                            }
                        }
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
                    }
                    handle_authenticated_allocate(
                        &recording_conn_dyn,
                        &allocations,
                        &relay_generator,
                        &mut allocate_responses,
                        max_allocations,
                        five_tuple,
                        addr,
                        &decoded_msg,
                        username_attr,
                        relay_is_ipv4,
                        mi_key,
                    )
                    .await;
                }
                METHOD_REFRESH => {
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
                        &allocations,
                        &mut allocate_responses,
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

        delete_all_allocations(&allocations).await;
        recording_conn.shutdown_listener().await;
        drop(conn);
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
        let expired: Vec<Arc<ManagedAllocation>>;
        let mut infos = HashMap::new();
        {
            let mut guard = self.allocations.lock().await;
            let expired_tuples: Vec<FiveTuple> = guard
                .iter()
                .filter(|(_, alloc)| alloc.is_expired())
                .map(|(k, _)| *k)
                .collect();
            expired = expired_tuples
                .into_iter()
                .filter_map(|t| guard.remove(&t))
                .collect();

            match five_tuples {
                Some(tuples) => {
                    for tuple in tuples {
                        if let Some(alloc) = guard.get(&tuple) {
                            infos.insert(tuple, alloc.info());
                        }
                    }
                }
                None => {
                    for (tuple, alloc) in guard.iter() {
                        infos.insert(*tuple, alloc.info());
                    }
                }
            }
        }
        for alloc in expired {
            alloc.close().await;
        }
        Ok(infos)
    }

    /// Delete all active TURN allocations associated with `username`.
    pub async fn delete_allocations_by_username(&self, username: String) -> Result<()> {
        let removed: Vec<Arc<ManagedAllocation>> = {
            let mut guard = self.allocations.lock().await;
            let keys: Vec<FiveTuple> = guard
                .iter()
                .filter(|(_, alloc)| alloc.username == username || alloc.is_expired())
                .map(|(k, _)| *k)
                .collect();
            keys.into_iter().filter_map(|k| guard.remove(&k)).collect()
        };
        for alloc in removed {
            alloc.close().await;
        }
        Ok(())
    }

    /// Gracefully stop the TURN server and close all managed connections and relay allocations.
    pub async fn close(&self) -> Result<()> {
        let _ = self.shutdown_tx.send_replace(true);
        let mut done_rx = self.done_rx.clone();
        let _ = done_rx.wait_for(|&done| done).await;
        delete_all_allocations(&self.allocations).await;
        Ok(())
    }
}
