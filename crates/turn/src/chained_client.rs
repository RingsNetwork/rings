//! Upstream TURN client used by an intermediate relay hop (`RelayBackend::ChainedHop`) to forward
//! control (`Allocate`, `Refresh`, `CreatePermission`, `ChannelBind`) and data (`SendIndication`,
//! `DataIndication`, `ChannelData`) frames to the next TURN hop in an onion relay chain (#1292).

use std::collections::HashMap;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::PoisonError;
use std::time::Instant;

use stun::agent::TransactionId;
use stun::error_code::ErrorCodeAttribute;
use stun::error_code::CODE_STALE_NONCE;
use stun::fingerprint::FINGERPRINT;
use stun::integrity::MessageIntegrity;
use stun::message::Getter;
use stun::message::Message;
use stun::message::MessageType;
use stun::message::Method;
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
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use turn::auth::generate_auth_key;
use turn::proto::chandata::ChannelData;
use turn::proto::channum::ChannelNumber;
use turn::proto::data::Data;
use turn::proto::lifetime::Lifetime;
use turn::proto::peeraddr::PeerAddress;
use turn::proto::relayaddr::RelayedAddress;
use turn::proto::reqfamily::REQUESTED_FAMILY_IPV6;
use turn::proto::reqtrans::RequestedTransport;
use turn::proto::PROTO_UDP;

use crate::credentials::TurnCredentials;
use crate::error::Result;
use crate::error::TurnError;

const UPSTREAM_INBOUND_QUEUE_CAPACITY: usize = 512;
const UPSTREAM_MTU: usize = 65556;

/// Cancellation-safe RAII guard that aborts a spawned background task if `allocate` returns early
/// with an error or is cancelled before transferring ownership into [`UpstreamTurnRelayClient`].
struct AbortOnDrop(Option<JoinHandle<()>>);

impl AbortOnDrop {
    fn take(&mut self) -> Option<JoinHandle<()>> {
        self.0.take()
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

struct UpstreamAuthState {
    realm: String,
    nonce: String,
    mi_key: Vec<u8>,
}

#[derive(Clone, Copy)]
struct UpstreamChannelEntry {
    peer: SocketAddr,
    expires_at: Instant,
    confirmed: bool,
}

#[derive(Clone, Copy)]
struct UpstreamPeerEntry {
    channel: ChannelNumber,
    expires_at: Instant,
    confirmed: bool,
}

/// Bijective, TTL-expiring `(ChannelNumber <-> SocketAddr)` table for an upstream TURN hop.
///
/// Invariants:
/// - Expired entries are never returned for either inbound `ChannelData` or outbound `send_to`.
/// - Pending `ChannelBind` entries allow inbound `ChannelData` arriving immediately behind
///   `ChannelBind 200 OK` on the reader task to be decoded without racing the control waiter,
///   while outbound `send_to` waits until the binding is confirmed.
/// - Rebinding an existing `ChannelNumber` or `SocketAddr` atomically removes any displaced reverse
///   entry so traffic is never misrouted to a stale peer.
#[derive(Default)]
pub(crate) struct UpstreamChannelTable {
    channel_to_peer: HashMap<ChannelNumber, UpstreamChannelEntry>,
    peer_to_channel: HashMap<SocketAddr, UpstreamPeerEntry>,
}

impl UpstreamChannelTable {
    pub(crate) fn prune(&mut self, now: Instant) {
        self.channel_to_peer.retain(|_, e| e.expires_at > now);
        self.peer_to_channel.retain(|_, e| e.expires_at > now);
    }

    pub(crate) fn begin_bind(
        &mut self,
        channel: ChannelNumber,
        peer: SocketAddr,
        now: Instant,
        timeout: Duration,
    ) {
        self.prune(now);
        // Only install a pending inbound mapping if this channel isn't already bound to a
        // different peer; on confirmation, `bind` atomically replaces any displaced entries.
        self.channel_to_peer
            .entry(channel)
            .or_insert(UpstreamChannelEntry {
                peer,
                expires_at: now + timeout,
                confirmed: false,
            });
    }

    pub(crate) fn cancel_pending(&mut self, channel: ChannelNumber, peer: SocketAddr) {
        if let Some(entry) = self.channel_to_peer.get(&channel) {
            if !entry.confirmed && entry.peer == peer {
                self.channel_to_peer.remove(&channel);
            }
        }
    }

    pub(crate) fn bind(
        &mut self,
        channel: ChannelNumber,
        peer: SocketAddr,
        now: Instant,
        timeout: Duration,
    ) {
        self.prune(now);
        if let Some(old) = self.channel_to_peer.remove(&channel) {
            if self
                .peer_to_channel
                .get(&old.peer)
                .is_some_and(|e| e.channel == channel)
            {
                self.peer_to_channel.remove(&old.peer);
            }
        }
        if let Some(old) = self.peer_to_channel.remove(&peer) {
            if self
                .channel_to_peer
                .get(&old.channel)
                .is_some_and(|e| e.peer == peer)
            {
                self.channel_to_peer.remove(&old.channel);
            }
        }
        let expires_at = now + timeout;
        self.channel_to_peer.insert(channel, UpstreamChannelEntry {
            peer,
            expires_at,
            confirmed: true,
        });
        self.peer_to_channel.insert(peer, UpstreamPeerEntry {
            channel,
            expires_at,
            confirmed: true,
        });
    }

    pub(crate) fn peer_for_channel(
        &mut self,
        channel: ChannelNumber,
        now: Instant,
    ) -> Option<SocketAddr> {
        let entry = *self.channel_to_peer.get(&channel)?;
        if entry.expires_at <= now {
            self.channel_to_peer.remove(&channel);
            if self
                .peer_to_channel
                .get(&entry.peer)
                .is_some_and(|e| e.channel == channel)
            {
                self.peer_to_channel.remove(&entry.peer);
            }
            return None;
        }
        Some(entry.peer)
    }

    pub(crate) fn channel_for_peer(
        &mut self,
        peer: SocketAddr,
        now: Instant,
    ) -> Option<ChannelNumber> {
        let entry = *self.peer_to_channel.get(&peer)?;
        if entry.expires_at <= now {
            self.peer_to_channel.remove(&peer);
            if self
                .channel_to_peer
                .get(&entry.channel)
                .is_some_and(|e| e.peer == peer)
            {
                self.channel_to_peer.remove(&entry.channel);
            }
            return None;
        }
        entry.confirmed.then_some(entry.channel)
    }
}

/// Verify that an upstream STUN/TURN response matches `expected_method` and carries a valid
/// RFC 5389 `MESSAGE-INTEGRITY` attribute computed with `mi_key`.
pub(crate) fn verify_authenticated_response(
    resp: &Message,
    expected_method: Method,
    mi_key: &[u8],
) -> bool {
    if resp.typ.method != expected_method {
        return false;
    }
    MessageIntegrity(mi_key.to_vec())
        .check(&mut resp.clone())
        .is_ok()
}

/// Active upstream TURN allocation client connecting hop `T_i` to `T_{i+1}` (#1292).
pub(crate) struct UpstreamTurnRelayClient {
    socket: Arc<UdpSocket>,
    server_addr: SocketAddr,
    relayed_addr: SocketAddr,
    credentials: TurnCredentials,
    channel_bind_timeout: Duration,
    auth_state: SyncMutex<UpstreamAuthState>,
    control_waiters: Arc<SyncMutex<HashMap<TransactionId, oneshot::Sender<Message>>>>,
    channels: Arc<SyncMutex<UpstreamChannelTable>>,
    inbound_rx: Mutex<mpsc::Receiver<(Vec<u8>, SocketAddr)>>,
    payload_closed: Arc<Notify>,
    reader_shutdown: Arc<Notify>,
    reader_task: SyncMutex<Option<JoinHandle<()>>>,
}

impl UpstreamTurnRelayClient {
    /// Allocate an upstream TURN relay session on `server_addr` authenticated by `credentials`,
    /// requesting `requested_lifetime` and `REQUESTED-ADDRESS-FAMILY` matching
    /// `projected_relay_is_ipv6`, returning the client handle and the `XOR-RELAYED-ADDRESS`
    /// reported by `server_addr`.
    pub(crate) async fn allocate(
        server_addr: SocketAddr,
        credentials: TurnCredentials,
        requested_lifetime: Duration,
        channel_bind_timeout: Duration,
        projected_relay_is_ipv6: bool,
    ) -> Result<(Arc<Self>, SocketAddr)> {
        let bind_addr = if server_addr.is_ipv4() {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
        } else {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
        };
        let socket = Arc::new(UdpSocket::bind(bind_addr).await?);

        let control_waiters: Arc<SyncMutex<HashMap<TransactionId, oneshot::Sender<Message>>>> =
            Arc::new(SyncMutex::new(HashMap::new()));
        let channels: Arc<SyncMutex<UpstreamChannelTable>> =
            Arc::new(SyncMutex::new(UpstreamChannelTable::default()));
        let (inbound_tx, inbound_rx) = mpsc::channel(UPSTREAM_INBOUND_QUEUE_CAPACITY);
        let payload_closed = Arc::new(Notify::new());
        let reader_shutdown = Arc::new(Notify::new());

        let reader_socket = Arc::clone(&socket);
        let reader_waiters = Arc::clone(&control_waiters);
        let reader_channels = Arc::clone(&channels);
        let reader_stop = Arc::clone(&reader_shutdown);
        let mut reader_guard = AbortOnDrop(Some(tokio::spawn(async move {
            let mut buf = vec![0u8; UPSTREAM_MTU];
            loop {
                let notified = reader_stop.notified();
                let recv = tokio::select! {
                    _ = notified => break,
                    res = reader_socket.recv_from(&mut buf) => res,
                };
                let Ok((n, src)) = recv else {
                    break;
                };
                if src != server_addr {
                    continue;
                }
                let Some(raw) = buf.get(..n).map(<[u8]>::to_vec) else {
                    continue;
                };
                if ChannelData::is_channel_data(&raw) {
                    let mut cd = ChannelData {
                        raw,
                        ..Default::default()
                    };
                    if cd.decode().is_ok() {
                        let maybe_peer = reader_channels
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .peer_for_channel(cd.number, Instant::now());
                        if let Some(peer) = maybe_peer {
                            let _ = inbound_tx.try_send((cd.data, peer));
                        }
                    }
                    continue;
                }
                let mut msg = Message {
                    raw,
                    ..Default::default()
                };
                if msg.decode().is_err() {
                    continue;
                }
                if msg.typ.class == CLASS_INDICATION && msg.typ.method == METHOD_DATA {
                    let mut peer_attr = PeerAddress::default();
                    let mut data_attr = Data::default();
                    if peer_attr.get_from(&msg).is_ok() && data_attr.get_from(&msg).is_ok() {
                        let peer = SocketAddr::new(peer_attr.ip, peer_attr.port);
                        let _ = inbound_tx.try_send((data_attr.0, peer));
                    }
                    continue;
                }
                if msg.typ.class == CLASS_SUCCESS_RESPONSE || msg.typ.class == CLASS_ERROR_RESPONSE
                {
                    let maybe_tx = reader_waiters
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&msg.transaction_id);
                    if let Some(tx) = maybe_tx {
                        let _ = tx.send(msg);
                    }
                }
            }
        })));

        // Step 1: Unauthenticated Allocate challenge.
        let challenge_tx_id = TransactionId::new();
        let mut challenge_req = Message::new();
        let mut challenge_setters: Vec<Box<dyn Setter>> = vec![
            Box::new(Message {
                transaction_id: challenge_tx_id,
                ..Default::default()
            }),
            Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
            Box::new(RequestedTransport {
                protocol: PROTO_UDP,
            }),
        ];
        if projected_relay_is_ipv6 {
            challenge_setters.push(Box::new(REQUESTED_FAMILY_IPV6));
        }
        challenge_setters.push(Box::new(FINGERPRINT));
        challenge_req.build(&challenge_setters)?;

        let challenge_resp = Self::send_and_await_response(
            &socket,
            server_addr,
            &control_waiters,
            challenge_tx_id,
            &challenge_req.raw,
        )
        .await?;
        let mut realm_attr = Realm::new(stun::attributes::ATTR_REALM, String::new());
        let mut nonce_attr = Nonce::new(stun::attributes::ATTR_NONCE, String::new());
        realm_attr.get_from(&challenge_resp)?;
        nonce_attr.get_from(&challenge_resp)?;

        let mi_key = generate_auth_key(
            &credentials.username,
            &realm_attr.text,
            &credentials.credential,
        );

        // Step 2: Authenticated Allocate request.
        let alloc_tx_id = TransactionId::new();
        let mut alloc_req = Message::new();
        let mut alloc_setters: Vec<Box<dyn Setter>> = vec![
            Box::new(Message {
                transaction_id: alloc_tx_id,
                ..Default::default()
            }),
            Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
            Box::new(RequestedTransport {
                protocol: PROTO_UDP,
            }),
            Box::new(Lifetime(requested_lifetime)),
        ];
        if projected_relay_is_ipv6 {
            alloc_setters.push(Box::new(REQUESTED_FAMILY_IPV6));
        }
        alloc_setters.push(Box::new(Username::new(
            stun::attributes::ATTR_USERNAME,
            credentials.username.clone(),
        )));
        alloc_setters.push(Box::new(realm_attr.clone()));
        alloc_setters.push(Box::new(nonce_attr.clone()));
        alloc_setters.push(Box::new(MessageIntegrity(mi_key.clone())));
        alloc_setters.push(Box::new(FINGERPRINT));
        alloc_req.build(&alloc_setters)?;

        let alloc_resp = Self::send_and_await_response(
            &socket,
            server_addr,
            &control_waiters,
            alloc_tx_id,
            &alloc_req.raw,
        )
        .await?;
        if alloc_resp.typ.class != CLASS_SUCCESS_RESPONSE {
            let mut err_attr = ErrorCodeAttribute::default();
            let reason = if err_attr.get_from(&alloc_resp).is_ok() {
                format!(
                    "upstream Allocate rejected with {}: {}",
                    err_attr.code.0,
                    String::from_utf8_lossy(&err_attr.reason)
                )
            } else {
                "upstream Allocate rejected".to_string()
            };
            return Err(TurnError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                reason,
            )));
        }
        if !verify_authenticated_response(&alloc_resp, METHOD_ALLOCATE, &mi_key) {
            return Err(TurnError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "upstream Allocate response failed MESSAGE-INTEGRITY verification",
            )));
        }
        let mut relayed_attr = RelayedAddress::default();
        relayed_attr.get_from(&alloc_resp)?;
        let relayed_addr = SocketAddr::new(relayed_attr.ip, relayed_attr.port);

        let client = Arc::new(Self {
            socket,
            server_addr,
            relayed_addr,
            credentials,
            channel_bind_timeout,
            auth_state: SyncMutex::new(UpstreamAuthState {
                realm: realm_attr.text,
                nonce: nonce_attr.text,
                mi_key,
            }),
            control_waiters,
            channels,
            inbound_rx: Mutex::new(inbound_rx),
            payload_closed,
            reader_shutdown,
            reader_task: SyncMutex::new(reader_guard.take()),
        });
        Ok((client, relayed_addr))
    }

    /// Return the `XOR-RELAYED-ADDRESS` returned by the upstream TURN hop during `Allocate`.
    pub(crate) const fn relayed_addr(&self) -> SocketAddr {
        self.relayed_addr
    }

    async fn send_and_await_response(
        socket: &UdpSocket,
        server_addr: SocketAddr,
        waiters: &SyncMutex<HashMap<TransactionId, oneshot::Sender<Message>>>,
        tx_id: TransactionId,
        raw: &[u8],
    ) -> Result<Message> {
        let (tx, mut rx) = oneshot::channel();
        waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(tx_id, tx);

        // RFC 5389 §7.2.1 bounded UDP retransmission schedule using the same TransactionId and
        // bytes so lost responses retrieve cached server replies without re-consuming tickets.
        const RETRANSMIT_WINDOWS: [Duration; 4] = [
            Duration::from_millis(500),
            Duration::from_millis(1_000),
            Duration::from_millis(2_000),
            Duration::from_millis(1_500),
        ];

        for wait_window in RETRANSMIT_WINDOWS {
            if let Err(err) = socket.send_to(raw, server_addr).await {
                waiters
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&tx_id);
                return Err(err.into());
            }
            match tokio::time::timeout(wait_window, &mut rx).await {
                Ok(Ok(msg)) => return Ok(msg),
                Ok(Err(_)) => break,
                Err(_) => continue,
            }
        }

        waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&tx_id);
        Err(TurnError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "upstream TURN hop request timed out",
        )))
    }

    async fn execute_authenticated_request<F>(
        &self,
        expected_method: Method,
        build_attrs: F,
    ) -> Result<Message>
    where
        F: Fn(TransactionId, &str, &str, &str, &[u8]) -> Result<Vec<u8>>,
    {
        let mut retry_nonce: Option<String> = None;
        for attempt in 0..2 {
            let (realm, nonce, mi_key) = {
                let state = self
                    .auth_state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let active_nonce = retry_nonce
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| state.nonce.clone());
                (state.realm.clone(), active_nonce, state.mi_key.clone())
            };
            let tx_id = TransactionId::new();
            let raw = build_attrs(tx_id, &self.credentials.username, &realm, &nonce, &mi_key)?;
            let resp = Self::send_and_await_response(
                &self.socket,
                self.server_addr,
                &self.control_waiters,
                tx_id,
                &raw,
            )
            .await?;
            if resp.typ.class == CLASS_SUCCESS_RESPONSE {
                if !verify_authenticated_response(&resp, expected_method, &mi_key) {
                    return Err(TurnError::Io(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "upstream control response failed MESSAGE-INTEGRITY verification",
                    )));
                }
                if let Some(accepted_nonce) = retry_nonce.take() {
                    let mut state = self
                        .auth_state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    state.nonce = accepted_nonce;
                }
                return Ok(resp);
            }
            if attempt == 0
                && resp.typ.class == CLASS_ERROR_RESPONSE
                && resp.typ.method == expected_method
            {
                let mut err_attr = ErrorCodeAttribute::default();
                let mut new_nonce = Nonce::new(stun::attributes::ATTR_NONCE, String::new());
                if err_attr.get_from(&resp).is_ok()
                    && err_attr.code == CODE_STALE_NONCE
                    && new_nonce.get_from(&resp).is_ok()
                {
                    retry_nonce = Some(new_nonce.text);
                    continue;
                }
            }
            return Err(TurnError::Turn(turn::Error::ErrNoSuchUser));
        }
        Err(TurnError::Turn(turn::Error::ErrNoSuchUser))
    }

    /// Cascade `CreatePermission` for `peer_ips` to the upstream TURN hop.
    pub(crate) async fn create_permission(&self, peer_ips: &[IpAddr]) -> Result<()> {
        self.execute_authenticated_request(
            METHOD_CREATE_PERMISSION,
            |tx_id, username, realm, nonce, mi_key| {
                let mut msg = Message::new();
                let mut setters: Vec<Box<dyn Setter>> = vec![
                    Box::new(Message {
                        transaction_id: tx_id,
                        ..Default::default()
                    }),
                    Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                ];
                for &ip in peer_ips {
                    setters.push(Box::new(PeerAddress { ip, port: 0 }));
                }
                setters.push(Box::new(Username::new(
                    stun::attributes::ATTR_USERNAME,
                    username.to_string(),
                )));
                setters.push(Box::new(Realm::new(
                    stun::attributes::ATTR_REALM,
                    realm.to_string(),
                )));
                setters.push(Box::new(Nonce::new(
                    stun::attributes::ATTR_NONCE,
                    nonce.to_string(),
                )));
                setters.push(Box::new(MessageIntegrity(mi_key.to_vec())));
                setters.push(Box::new(FINGERPRINT));
                msg.build(&setters)?;
                Ok(msg.raw)
            },
        )
        .await
        .map(|_| ())
    }

    /// Cascade `ChannelBind(channel, peer)` to the upstream TURN hop and record the channel mapping
    /// locally with TTL expiration for `ChannelData` forwarding.
    pub(crate) async fn channel_bind(
        &self,
        channel: ChannelNumber,
        peer: SocketAddr,
    ) -> Result<()> {
        self.channels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .begin_bind(channel, peer, Instant::now(), self.channel_bind_timeout);
        let res = self
            .execute_authenticated_request(
                METHOD_CHANNEL_BIND,
                |tx_id, username, realm, nonce, mi_key| {
                    let mut msg = Message::new();
                    let setters: Vec<Box<dyn Setter>> = vec![
                        Box::new(Message {
                            transaction_id: tx_id,
                            ..Default::default()
                        }),
                        Box::new(MessageType::new(METHOD_CHANNEL_BIND, CLASS_REQUEST)),
                        Box::new(channel),
                        Box::new(PeerAddress {
                            ip: peer.ip(),
                            port: peer.port(),
                        }),
                        Box::new(Username::new(
                            stun::attributes::ATTR_USERNAME,
                            username.to_string(),
                        )),
                        Box::new(Realm::new(stun::attributes::ATTR_REALM, realm.to_string())),
                        Box::new(Nonce::new(stun::attributes::ATTR_NONCE, nonce.to_string())),
                        Box::new(MessageIntegrity(mi_key.to_vec())),
                        Box::new(FINGERPRINT),
                    ];
                    msg.build(&setters)?;
                    Ok(msg.raw)
                },
            )
            .await;
        if let Err(err) = res {
            self.channels
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cancel_pending(channel, peer);
            return Err(err);
        }
        self.channels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bind(channel, peer, Instant::now(), self.channel_bind_timeout);
        Ok(())
    }

    /// Cascade `Refresh(lifetime)` to the upstream TURN hop.
    pub(crate) async fn refresh(&self, lifetime: Duration) -> Result<()> {
        self.execute_authenticated_request(
            METHOD_REFRESH,
            |tx_id, username, realm, nonce, mi_key| {
                let mut msg = Message::new();
                let setters: Vec<Box<dyn Setter>> = vec![
                    Box::new(Message {
                        transaction_id: tx_id,
                        ..Default::default()
                    }),
                    Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)),
                    Box::new(Lifetime(lifetime)),
                    Box::new(Username::new(
                        stun::attributes::ATTR_USERNAME,
                        username.to_string(),
                    )),
                    Box::new(Realm::new(stun::attributes::ATTR_REALM, realm.to_string())),
                    Box::new(Nonce::new(stun::attributes::ATTR_NONCE, nonce.to_string())),
                    Box::new(MessageIntegrity(mi_key.to_vec())),
                    Box::new(FINGERPRINT),
                ];
                msg.build(&setters)?;
                Ok(msg.raw)
            },
        )
        .await
        .map(|_| ())
    }

    /// Forward an outbound payload to `target` via the upstream TURN hop (using `ChannelData` when
    /// an unexpired channel is bound for `target`, or `SendIndication` otherwise).
    pub(crate) async fn send_to(
        &self,
        payload: &[u8],
        target: SocketAddr,
    ) -> webrtc_util::Result<usize> {
        let maybe_channel = self
            .channels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .channel_for_peer(target, Instant::now());
        if let Some(number) = maybe_channel {
            let mut cd = ChannelData {
                data: payload.to_vec(),
                number,
                raw: Vec::new(),
            };
            cd.encode();
            self.socket.send_to(&cd.raw, self.server_addr).await?;
            Ok(payload.len())
        } else {
            let mut msg = Message::new();
            msg.build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_SEND, CLASS_INDICATION)),
                Box::new(PeerAddress {
                    ip: target.ip(),
                    port: target.port(),
                }),
                Box::new(Data(payload.to_vec())),
            ])
            .map_err(|e| webrtc_util::Error::Other(e.to_string()))?;
            self.socket.send_to(&msg.raw, self.server_addr).await?;
            Ok(payload.len())
        }
    }

    /// Receive the next inbound payload forwarded back from the upstream TURN hop.
    pub(crate) async fn recv_datagram(
        &self,
        buf: &mut [u8],
    ) -> webrtc_util::Result<(usize, SocketAddr)> {
        let mut rx = self.inbound_rx.lock().await;
        let notified = self.payload_closed.notified();
        let item = tokio::select! {
            _ = notified => None,
            v = rx.recv() => v,
        };
        let Some((data, src_addr)) = item else {
            return Err(webrtc_util::Error::Other(
                "upstream TURN relay closed".to_string(),
            ));
        };
        let n = data.len().min(buf.len());
        if let Some(dst) = buf.get_mut(..n) {
            if let Some(src) = data.get(..n) {
                dst.copy_from_slice(src);
            }
        }
        Ok((n, src_addr))
    }

    /// Stop the data-plane inbound receiver immediately while keeping the control-plane reader
    /// alive so [`Self::finish_close`] can complete the `Refresh(0)` deletion exchange promptly
    /// without waiting for the upstream control timeout.
    pub(crate) fn begin_close(&self) {
        self.payload_closed.notify_waiters();
    }

    /// Send `Refresh(0)` to release the upstream TURN allocation, then shut down the control-plane
    /// reader task.
    pub(crate) async fn finish_close(&self) {
        let _ = self.refresh(Duration::ZERO).await;
        self.reader_shutdown.notify_waiters();
        if let Some(handle) = self
            .reader_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            handle.abort();
        }
    }
}

impl Drop for UpstreamTurnRelayClient {
    fn drop(&mut self) {
        self.payload_closed.notify_waiters();
        self.reader_shutdown.notify_waiters();
        if let Some(handle) = self
            .reader_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_channel_table_expires_and_evicts_displaced_reverse_entries() {
        let t0 = Instant::now();
        let ttl = Duration::from_secs(600);
        let peer_a: SocketAddr = "127.0.0.1:40001".parse().unwrap();
        let peer_b: SocketAddr = "127.0.0.1:40002".parse().unwrap();
        let ch1 = ChannelNumber(0x4000);
        let ch2 = ChannelNumber(0x4001);

        let mut table = UpstreamChannelTable::default();
        table.bind(ch1, peer_a, t0, ttl);
        assert_eq!(table.peer_for_channel(ch1, t0), Some(peer_a));
        assert_eq!(table.channel_for_peer(peer_a, t0), Some(ch1));

        // Rebinding `ch1` to `peer_b` must evict `peer_a`'s reverse mapping.
        table.bind(ch1, peer_b, t0 + Duration::from_secs(1), ttl);
        assert_eq!(
            table.channel_for_peer(peer_a, t0 + Duration::from_secs(1)),
            None
        );
        assert_eq!(
            table.peer_for_channel(ch1, t0 + Duration::from_secs(1)),
            Some(peer_b)
        );
        assert_eq!(
            table.channel_for_peer(peer_b, t0 + Duration::from_secs(1)),
            Some(ch1)
        );

        // Rebinding `peer_b` to `ch2` must evict `ch1`'s forward mapping.
        table.bind(ch2, peer_b, t0 + Duration::from_secs(2), ttl);
        assert_eq!(
            table.peer_for_channel(ch1, t0 + Duration::from_secs(2)),
            None
        );
        assert_eq!(
            table.peer_for_channel(ch2, t0 + Duration::from_secs(2)),
            Some(peer_b)
        );

        // Advancing past TTL expires both directions.
        let after_ttl = t0 + ttl + Duration::from_secs(5);
        assert_eq!(table.peer_for_channel(ch2, after_ttl), None);
        assert_eq!(table.channel_for_peer(peer_b, after_ttl), None);
    }

    #[test]
    fn verify_authenticated_response_rejects_forged_mi_and_wrong_method() {
        let mi_key = generate_auth_key("user", "rings.network", "secret-pass");
        let wrong_key = generate_auth_key("user", "rings.network", "wrong-pass");

        let mut valid = Message::new();
        valid
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(
                    METHOD_CREATE_PERMISSION,
                    CLASS_SUCCESS_RESPONSE,
                )),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        assert!(verify_authenticated_response(
            &valid,
            METHOD_CREATE_PERMISSION,
            &mi_key
        ));
        assert!(!verify_authenticated_response(
            &valid,
            METHOD_CHANNEL_BIND,
            &mi_key
        ));
        assert!(!verify_authenticated_response(
            &valid,
            METHOD_CREATE_PERMISSION,
            &wrong_key
        ));

        let mut unsigned = Message::new();
        unsigned
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(
                    METHOD_CREATE_PERMISSION,
                    CLASS_SUCCESS_RESPONSE,
                )),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        assert!(!verify_authenticated_response(
            &unsigned,
            METHOD_CREATE_PERMISSION,
            &mi_key
        ));
    }
}
