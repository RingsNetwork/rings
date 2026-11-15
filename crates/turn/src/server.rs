use std::collections::HashMap;
use std::collections::HashSet;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::PoisonError;

use async_trait::async_trait;
use rand::Rng;
use rings_core::delegation::format_turn_server_url;
use rings_core::delegation::DelegateeKey;
use rings_core::ecc::PublicKey;
use stun::agent::TransactionId;
use stun::attributes::ATTR_EVEN_PORT;
use stun::attributes::ATTR_MESSAGE_INTEGRITY;
use stun::attributes::ATTR_NONCE;
use stun::attributes::ATTR_REALM;
use stun::attributes::ATTR_USERNAME;
use stun::attributes::ATTR_XOR_PEER_ADDRESS;
use stun::error_code::ErrorCodeAttribute;
use stun::error_code::CODE_FORBIDDEN;
use stun::fingerprint::FINGERPRINT;
use stun::integrity::MessageIntegrity;
use stun::message::Getter;
use stun::message::Message;
use stun::message::MessageType;
use stun::message::Setter;
use stun::message::CLASS_ERROR_RESPONSE;
use stun::message::CLASS_INDICATION;
use stun::message::CLASS_REQUEST;
use stun::message::METHOD_ALLOCATE;
use stun::message::METHOD_CHANNEL_BIND;
use stun::message::METHOD_CREATE_PERMISSION;
use stun::message::METHOD_REFRESH;
use stun::message::METHOD_SEND;
use stun::textattrs::Nonce;
use stun::textattrs::Realm;
use stun::textattrs::Username;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::Instant;
use turn::allocation::allocation_manager::Manager;
use turn::allocation::allocation_manager::ManagerConfig;
use turn::allocation::five_tuple::FiveTuple;
use turn::allocation::AllocationInfo;
use turn::auth::AuthHandler;
use turn::proto::lifetime::DEFAULT_LIFETIME;
use turn::proto::peeraddr::PeerAddress;
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

const INBOUND_MTU: usize = 1500;
const NONCE_LIFETIME: Duration = Duration::from_secs(3600);

struct PolicyRelayConn {
    inner: Arc<dyn Conn + Send + Sync>,
    peer_policy: RelayPeerPolicy,
}

#[async_trait]
impl Conn for PolicyRelayConn {
    async fn connect(&self, addr: SocketAddr) -> webrtc_util::Result<()> {
        if !self.peer_policy.is_allowed_addr(addr) {
            return Err(webrtc_util::Error::Other(
                "relay destination address denied by policy".to_string(),
            ));
        }
        self.inner.connect(addr).await
    }

    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        self.inner.recv(buf).await
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        loop {
            let (n, src_addr) = self.inner.recv_from(buf).await?;
            if self.peer_policy.is_allowed_addr(src_addr) {
                return Ok((n, src_addr));
            }
        }
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        self.inner.send(buf).await
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        if !self.peer_policy.is_allowed_addr(target) {
            return Err(webrtc_util::Error::Other(
                "relay destination address denied by policy".to_string(),
            ));
        }
        self.inner.send_to(buf, target).await
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        self.inner.remote_addr()
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        self.inner.close().await
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

struct PolicyRelayAddressGenerator {
    relay: RelayAddressConfig,
    peer_policy: RelayPeerPolicy,
    require_even_port: Arc<AtomicBool>,
    net: Arc<Net>,
}

impl PolicyRelayAddressGenerator {
    fn new(
        relay: RelayAddressConfig,
        peer_policy: RelayPeerPolicy,
        require_even_port: Arc<AtomicBool>,
    ) -> Self {
        Self {
            relay,
            peer_policy,
            require_even_port,
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
        let addr = self
            .net
            .resolve_addr(use_ipv4, &format!("{bind_ip}:{port}"))
            .await?;
        let conn = self.net.bind(addr).await?;
        let mut relay_addr = conn.local_addr()?;
        if let Some(relay_ip) = relay_ip_override {
            relay_addr.set_ip(relay_ip);
        }
        let wrapped: Arc<dyn Conn + Send + Sync> = Arc::new(PolicyRelayConn {
            inner: conn,
            peer_policy: self.peer_policy,
        });
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
    inner: Arc<dyn Conn + Send + Sync>,
    recording: AtomicBool,
    last_sent: SyncMutex<Option<Vec<u8>>>,
}

impl RecordingConn {
    fn new(inner: Arc<dyn Conn + Send + Sync>) -> Self {
        Self {
            inner,
            recording: AtomicBool::new(false),
            last_sent: SyncMutex::new(None),
        }
    }

    fn set_recording(&self, enabled: bool) {
        if enabled {
            *self
                .last_sent
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
        }
        self.recording.store(enabled, Ordering::Release);
    }

    fn take_last_sent(&self) -> Option<Vec<u8>> {
        self.recording.store(false, Ordering::Release);
        self.last_sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

#[async_trait]
impl Conn for RecordingConn {
    async fn connect(&self, addr: SocketAddr) -> webrtc_util::Result<()> {
        self.inner.connect(addr).await
    }

    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        self.inner.recv(buf).await
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        self.inner.recv_from(buf).await
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        self.inner.send(buf).await
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        if self.recording.load(Ordering::Acquire) {
            *self
                .last_sent
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(buf.to_vec());
        }
        self.inner.send_to(buf, target).await
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        self.inner.remote_addr()
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        self.inner.close().await
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

fn check_message_peer_addresses(m: &Message, peer_policy: RelayPeerPolicy) -> bool {
    let ignore_port = m.typ.method == METHOD_CREATE_PERMISSION;
    for attr in &m.attributes.0 {
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

async fn send_forbidden_error(
    conn: &Arc<dyn Conn + Send + Sync>,
    dst: SocketAddr,
    m: &Message,
    mi_key: Vec<u8>,
) {
    let built = {
        let mut err_msg = Message::new();
        let msg_type = MessageType::new(m.typ.method, CLASS_ERROR_RESPONSE);
        let err_attr = ErrorCodeAttribute {
            code: CODE_FORBIDDEN,
            reason: b"Peer Address Forbidden".to_vec(),
        };
        let setters: Vec<Box<dyn Setter>> = vec![
            Box::new(Message {
                transaction_id: m.transaction_id,
                ..Default::default()
            }),
            Box::new(msg_type),
            Box::new(err_attr),
            Box::new(MessageIntegrity(mi_key)),
            Box::new(FINGERPRINT),
        ];
        err_msg.build(&setters).is_ok().then_some(err_msg.raw)
    };
    if let Some(raw) = built {
        let _ = conn.send_to(&raw, dst).await;
    }
}

/// A running Rings TURN server backed by [`DelegationAuthHandler`].
pub struct TurnServer {
    local_addr: SocketAddr,
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
        let conn: Arc<dyn Conn + Send + Sync> = Arc::new(socket);

        let auth_handler = Arc::new(DelegationAuthHandler::with_server_key_and_clock(
            config.admission,
            config.server_key,
            config.clock,
        ));
        let require_even_port = Arc::new(AtomicBool::new(false));
        let relay_addr_generator = Box::new(PolicyRelayAddressGenerator::new(
            config.relay,
            config.peer_policy,
            Arc::clone(&require_even_port),
        ));
        let (alloc_close_tx, alloc_close_rx) = mpsc::channel(256);
        let allocation_manager = Arc::new(Manager::new(ManagerConfig {
            relay_addr_generator,
            alloc_close_notify: Some(alloc_close_tx),
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
            require_even_port,
            alloc_close_rx,
            command_rx,
        ));

        Ok(Self {
            local_addr,
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
        require_even_port: Arc<AtomicBool>,
        mut alloc_close_rx: mpsc::Receiver<AllocationInfo>,
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
        let recording_conn = Arc::new(RecordingConn::new(Arc::clone(&conn)));
        let recording_conn_dyn: Arc<dyn Conn + Send + Sync> =
            Arc::clone(&recording_conn) as Arc<dyn Conn + Send + Sync>;
        let mut allocate_responses: HashMap<FiveTuple, (TransactionId, Vec<u8>)> = HashMap::new();
        let mut closed_guard: Option<Arc<mpsc::Receiver<()>>> = None;

        loop {
            while let Ok(closed_info) = alloc_close_rx.try_recv() {
                allocate_responses.remove(&closed_info.five_tuple);
            }

            let (n, addr) = tokio::select! {
                v = conn.recv_from(&mut buf) => {
                    match v {
                        Ok(v) => v,
                        Err(err) => {
                            tracing::debug!("exit TURN read loop on socket error: {err}");
                            break;
                        }
                    }
                }
                Some(closed_info) = alloc_close_rx.recv() => {
                    allocate_responses.remove(&closed_info.five_tuple);
                    continue;
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

            let mut is_fresh_allocate = false;

            if is_stun_msg && decoded_msg.typ.class == CLASS_REQUEST {
                let has_mi = decoded_msg.contains(ATTR_MESSAGE_INTEGRITY);
                prune_and_bound_nonces(&nonces, max_nonces, !has_mi).await;

                let existing_alloc = allocation_manager
                    .get_allocations_info(Some(vec![five_tuple]))
                    .await
                    .remove(&five_tuple);
                if existing_alloc.is_none() {
                    allocate_responses.remove(&five_tuple);
                }

                if decoded_msg.typ.method == METHOD_ALLOCATE {
                    require_even_port
                        .store(decoded_msg.contains(ATTR_EVEN_PORT), Ordering::Release);
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
                                    require_even_port.store(false, Ordering::Release);
                                    let _ = conn.send_to(cached_resp, addr).await;
                                    continue;
                                }
                            }
                        }
                    } else {
                        is_fresh_allocate = true;
                        recording_conn.set_recording(true);
                    }
                } else {
                    require_even_port.store(false, Ordering::Release);
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

                if (decoded_msg.typ.method == METHOD_CREATE_PERMISSION
                    || decoded_msg.typ.method == METHOD_CHANNEL_BIND)
                    && existing_alloc.is_some()
                    && !check_message_peer_addresses(&decoded_msg, peer_policy)
                {
                    let client_nonces = extract_client_nonces(&nonces, addr).await;
                    if let Some(mi_key) = verify_authenticated_request(
                        &decoded_msg,
                        &client_nonces,
                        &auth_handler,
                        addr,
                    ) {
                        auth_handler.set_expected_allocation_owner(addr, None);
                        send_forbidden_error(&conn, addr, &decoded_msg, mi_key).await;
                        continue;
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

            let mut r = Request {
                conn: Arc::clone(&recording_conn_dyn),
                src_addr: addr,
                buff: payload.to_vec(),
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

            if is_fresh_allocate {
                let recorded = recording_conn.take_last_sent();
                if let Some(resp_bytes) = recorded {
                    if allocation_manager
                        .get_allocations_info(Some(vec![five_tuple]))
                        .await
                        .contains_key(&five_tuple)
                    {
                        allocate_responses
                            .insert(five_tuple, (decoded_msg.transaction_id, resp_bytes));
                    }
                }
            }

            if is_stun_msg && decoded_msg.typ.class == CLASS_REQUEST {
                let after_map = req_nonces.lock().await.clone();
                sync_client_nonces(&nonces, addr, &before_nonce_keys, after_map, max_nonces).await;
            }
        }

        let _ = allocation_manager.close().await;
        let _ = conn.close().await;
        drop(closed_guard);
    }

    /// Return the local UDP [`SocketAddr`] this TURN server is bound to.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
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

    /// Return the unsigned `turn://<local_addr>?pubkey=<base58(server_pubkey)>` URL for this
    /// server, suitable for passing to [`DelegateeKey::sign_ice_servers`] or
    /// [`SecretKey::sign_ice_servers`](rings_core::ecc::SecretKey::sign_ice_servers).
    pub fn ice_server_base_url(&self) -> Result<String> {
        let base_url = format!("turn://{}", self.local_addr);
        Ok(format_turn_server_url(&base_url, self.server_pubkey())?)
    }

    /// Build a complete signed `turn://<username>:<credential>@<local_addr>` URL for `delegatee_key`.
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
        let _ = self.allocation_manager.close().await;
        Ok(())
    }
}
