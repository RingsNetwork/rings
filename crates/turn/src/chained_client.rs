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

use stun::agent::TransactionId;
use stun::error_code::ErrorCodeAttribute;
use stun::error_code::CODE_STALE_NONCE;
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

const UPSTREAM_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const UPSTREAM_INBOUND_QUEUE_CAPACITY: usize = 512;
const UPSTREAM_MTU: usize = 65556;

struct UpstreamAuthState {
    realm: String,
    nonce: String,
    mi_key: Vec<u8>,
}

/// Active upstream TURN allocation client connecting hop `T_i` to `T_{i+1}` (#1292).
pub(crate) struct UpstreamTurnRelayClient {
    socket: Arc<UdpSocket>,
    server_addr: SocketAddr,
    relayed_addr: SocketAddr,
    credentials: TurnCredentials,
    auth_state: SyncMutex<UpstreamAuthState>,
    control_waiters: Arc<SyncMutex<HashMap<TransactionId, oneshot::Sender<Message>>>>,
    channel_to_peer: Arc<SyncMutex<HashMap<ChannelNumber, SocketAddr>>>,
    peer_to_channel: Arc<SyncMutex<HashMap<SocketAddr, ChannelNumber>>>,
    inbound_rx: Mutex<mpsc::Receiver<(Vec<u8>, SocketAddr)>>,
    closed_notify: Arc<Notify>,
    reader_task: SyncMutex<Option<JoinHandle<()>>>,
}

impl UpstreamTurnRelayClient {
    /// Allocate an upstream TURN relay session on `server_addr` authenticated by `credentials`,
    /// returning the client handle and the `XOR-RELAYED-ADDRESS` reported by `server_addr`.
    pub(crate) async fn allocate(
        server_addr: SocketAddr,
        credentials: TurnCredentials,
    ) -> Result<(Arc<Self>, SocketAddr)> {
        let bind_addr = if server_addr.is_ipv4() {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        } else {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)
        };
        let socket = Arc::new(UdpSocket::bind(bind_addr).await?);

        let control_waiters: Arc<SyncMutex<HashMap<TransactionId, oneshot::Sender<Message>>>> =
            Arc::new(SyncMutex::new(HashMap::new()));
        let channel_to_peer: Arc<SyncMutex<HashMap<ChannelNumber, SocketAddr>>> =
            Arc::new(SyncMutex::new(HashMap::new()));
        let peer_to_channel: Arc<SyncMutex<HashMap<SocketAddr, ChannelNumber>>> =
            Arc::new(SyncMutex::new(HashMap::new()));
        let (inbound_tx, inbound_rx) = mpsc::channel(UPSTREAM_INBOUND_QUEUE_CAPACITY);
        let closed_notify = Arc::new(Notify::new());

        let reader_socket = Arc::clone(&socket);
        let reader_waiters = Arc::clone(&control_waiters);
        let reader_channels = Arc::clone(&channel_to_peer);
        let reader_closed = Arc::clone(&closed_notify);
        let reader_task = tokio::spawn(async move {
            let mut buf = vec![0u8; UPSTREAM_MTU];
            loop {
                let notified = reader_closed.notified();
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
                            .get(&cd.number)
                            .copied();
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
        });

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
        if server_addr.is_ipv6() {
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
        ];
        if server_addr.is_ipv6() {
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
            reader_task.abort();
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
        let mut relayed_attr = RelayedAddress::default();
        relayed_attr.get_from(&alloc_resp)?;
        let relayed_addr = SocketAddr::new(relayed_attr.ip, relayed_attr.port);

        let client = Arc::new(Self {
            socket,
            server_addr,
            relayed_addr,
            credentials,
            auth_state: SyncMutex::new(UpstreamAuthState {
                realm: realm_attr.text,
                nonce: nonce_attr.text,
                mi_key,
            }),
            control_waiters,
            channel_to_peer,
            peer_to_channel,
            inbound_rx: Mutex::new(inbound_rx),
            closed_notify,
            reader_task: SyncMutex::new(Some(reader_task)),
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
        let (tx, rx) = oneshot::channel();
        waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(tx_id, tx);
        if let Err(err) = socket.send_to(raw, server_addr).await {
            waiters
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&tx_id);
            return Err(err.into());
        }
        match tokio::time::timeout(UPSTREAM_CONTROL_TIMEOUT, rx).await {
            Ok(Ok(msg)) => Ok(msg),
            _ => {
                waiters
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&tx_id);
                Err(TurnError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "upstream TURN hop request timed out",
                )))
            }
        }
    }

    async fn execute_authenticated_request<F>(&self, build_attrs: F) -> Result<Message>
    where
        F: Fn(TransactionId, &str, &str, &str, &[u8]) -> Result<Vec<u8>>,
    {
        for attempt in 0..2 {
            let (realm, nonce, mi_key) = {
                let state = self
                    .auth_state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                (state.realm.clone(), state.nonce.clone(), state.mi_key.clone())
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
                return Ok(resp);
            }
            if attempt == 0 && resp.typ.class == CLASS_ERROR_RESPONSE {
                let mut err_attr = ErrorCodeAttribute::default();
                let mut new_nonce = Nonce::new(stun::attributes::ATTR_NONCE, String::new());
                if err_attr.get_from(&resp).is_ok()
                    && err_attr.code == CODE_STALE_NONCE
                    && new_nonce.get_from(&resp).is_ok()
                {
                    let mut state = self
                        .auth_state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    state.nonce = new_nonce.text;
                    continue;
                }
            }
            return Err(TurnError::Turn(turn::Error::ErrNoSuchUser));
        }
        Err(TurnError::Turn(turn::Error::ErrNoSuchUser))
    }

    /// Cascade `CreatePermission` for `peer_ips` to the upstream TURN hop.
    pub(crate) async fn create_permission(&self, peer_ips: &[IpAddr]) -> Result<()> {
        self.execute_authenticated_request(|tx_id, username, realm, nonce, mi_key| {
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
        })
        .await
        .map(|_| ())
    }

    /// Cascade `ChannelBind(channel, peer)` to the upstream TURN hop and record the channel mapping
    /// locally for `ChannelData` forwarding.
    pub(crate) async fn channel_bind(
        &self,
        channel: ChannelNumber,
        peer: SocketAddr,
    ) -> Result<()> {
        self.execute_authenticated_request(|tx_id, username, realm, nonce, mi_key| {
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
                Box::new(Realm::new(
                    stun::attributes::ATTR_REALM,
                    realm.to_string(),
                )),
                Box::new(Nonce::new(
                    stun::attributes::ATTR_NONCE,
                    nonce.to_string(),
                )),
                Box::new(MessageIntegrity(mi_key.to_vec())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters)?;
            Ok(msg.raw)
        })
        .await?;
        self.channel_to_peer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(channel, peer);
        self.peer_to_channel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(peer, channel);
        Ok(())
    }

    /// Cascade `Refresh(lifetime)` to the upstream TURN hop.
    pub(crate) async fn refresh(&self, lifetime: Duration) -> Result<()> {
        self.execute_authenticated_request(|tx_id, username, realm, nonce, mi_key| {
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
                Box::new(Realm::new(
                    stun::attributes::ATTR_REALM,
                    realm.to_string(),
                )),
                Box::new(Nonce::new(
                    stun::attributes::ATTR_NONCE,
                    nonce.to_string(),
                )),
                Box::new(MessageIntegrity(mi_key.to_vec())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters)?;
            Ok(msg.raw)
        })
        .await
        .map(|_| ())
    }

    /// Forward an outbound payload to `target` via the upstream TURN hop (using `ChannelData` when
    /// a channel is bound for `target`, or `SendIndication` otherwise).
    pub(crate) async fn send_to(
        &self,
        payload: &[u8],
        target: SocketAddr,
    ) -> webrtc_util::Result<usize> {
        let maybe_channel = self
            .peer_to_channel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&target)
            .copied();
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
        let notified = self.closed_notify.notified();
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

    pub(crate) fn begin_close(&self) {
        self.closed_notify.notify_waiters();
    }

    pub(crate) async fn finish_close(&self) {
        let _ = self.refresh(Duration::ZERO).await;
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
