//! Native TCP adapter for route-aware onion circuits.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::session::SessionSk;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::lookup_host;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Extensions;
use crate::extension::ext::Scope;
use crate::onion::circuit::encode_initial_forward;
use crate::onion::circuit::route_first_hop;
use crate::onion::circuit::send_backward;
use crate::onion::circuit::OnionAuthenticatedPayload;
use crate::onion::circuit::OnionBackwardNonce;
use crate::onion::circuit::OnionCircuitCapabilities;
use crate::onion::circuit::OnionCircuitHandler;
use crate::onion::circuit::OnionCircuitId;
use crate::onion::circuit::OnionCircuitPayload;
use crate::onion::circuit::OnionCircuitProtocol;
use crate::onion::circuit::OnionCircuitShell;
use crate::onion::circuit::OnionClientReturn;
use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::exit_accounting::OnionExitLease;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion_proxy::ONION_PROXY_TCP_SERVICE;

const TCP_BUF: usize = 30_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum OnionTcpPayload {
    Open { target: String },
    Data { bytes: Bytes },
    Shutdown,
    Close,
    Error { message: String },
}

fn encode_tcp_payload(payload: OnionTcpPayload) -> Result<OnionCircuitPayload> {
    bincode::serialize(&payload)
        .map(|body| OnionCircuitPayload::new(ONION_PROXY_TCP_SERVICE, Bytes::from(body)))
        .map_err(|_| Error::EncodeError)
}

fn decode_tcp_payload(payload: OnionCircuitPayload) -> Result<Option<OnionTcpPayload>> {
    if !payload.is_service(ONION_PROXY_TCP_SERVICE) {
        return Ok(None);
    }
    bincode::deserialize(payload.body.as_ref())
        .map(Some)
        .map_err(|_| Error::DecodeError)
}

/// Native handle for opening TCP streams over route-aware onion circuits.
#[derive(Clone)]
pub struct NativeOnionCircuitHandle {
    runtime: Arc<OnionTcpRuntime>,
    scope: Scope,
}

impl NativeOnionCircuitHandle {
    /// Install the route-aware onion circuit protocol.
    pub fn install(
        extensions: &Extensions,
        session_sk: SessionSk,
        allow_relay: bool,
        exit_policy: Option<OnionExitPolicy>,
    ) -> Result<Self> {
        let allow_exit = exit_policy.is_some();
        let runtime = Arc::new(OnionTcpRuntime::new(session_sk.clone(), exit_policy));
        let capabilities = OnionCircuitCapabilities::from_registration(allow_relay, allow_exit);
        extensions.register(
            OnionCircuitProtocol::new(capabilities),
            OnionCircuitShell::new(session_sk, NativeOnionCircuitHandler {
                runtime: runtime.clone(),
            }),
        )?;
        Ok(Self {
            runtime,
            scope: Scope::new(extensions.core(), ONION_CIRCUIT_NAMESPACE.to_string()),
        })
    }

    /// Relay an already-accepted TCP stream over `route`.
    pub async fn relay_tcp_stream(
        &self,
        stream: TcpStream,
        route: OnionRoute,
        target: OnionProxyTarget,
    ) -> Result<()> {
        self.runtime
            .open_client_stream(self.scope.clone(), stream, route, target)
            .await
    }
}

#[derive(Clone)]
struct NativeOnionCircuitHandler {
    runtime: Arc<OnionTcpRuntime>,
}

#[async_trait::async_trait]
impl OnionCircuitHandler for NativeOnionCircuitHandler {
    async fn handle_exit(
        &self,
        scope: &Scope,
        from: Did,
        circuit_id: OnionCircuitId,
        return_peer: Did,
        client: OnionClientReturn,
        payload: OnionCircuitPayload,
    ) -> Result<()> {
        self.runtime
            .handle_exit_payload(
                scope.clone(),
                from,
                circuit_id,
                return_peer,
                client,
                payload,
            )
            .await
    }

    async fn handle_client(
        &self,
        _scope: &Scope,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: OnionAuthenticatedPayload,
    ) -> Result<()> {
        self.runtime
            .handle_client_payload(from, circuit_id, payload)
            .await
    }
}

struct OnionTcpRuntime {
    session_sk: SessionSk,
    client_return: OnionClientReturn,
    client_streams: Mutex<HashMap<TcpStreamKey, ClientStream>>,
    exit_streams: Mutex<HashMap<TcpStreamKey, ExitStream>>,
    exit_policy: Option<OnionExitPolicy>,
    accounting: OnionExitAccounting,
}

impl OnionTcpRuntime {
    fn new(session_sk: SessionSk, exit_policy: Option<OnionExitPolicy>) -> Self {
        let client_return = OnionClientReturn::new(session_sk.session_public_key());
        Self {
            session_sk,
            client_return,
            client_streams: Mutex::new(HashMap::new()),
            exit_streams: Mutex::new(HashMap::new()),
            exit_policy,
            accounting: OnionExitAccounting::default(),
        }
    }

    async fn open_client_stream(
        self: &Arc<Self>,
        scope: Scope,
        stream: TcpStream,
        route: OnionRoute,
        target: OnionProxyTarget,
    ) -> Result<()> {
        let expected_return_peer = route_first_hop(&route)?;
        let expected_exit = route.exit.clone();
        let (tx, rx) = mpsc::channel(32);
        let key = self.insert_client_stream(expected_return_peer, expected_exit, tx)?;
        let (to, payload) = encode_initial_forward(
            self.client_return,
            &route,
            key.circuit_id,
            encode_tcp_payload(OnionTcpPayload::Open {
                target: target.authority(),
            })?,
        )?;
        if let Err(error) = scope.send(to, payload).await {
            self.remove_client_stream(key);
            return Err(error);
        }
        spawn_client_stream(
            self.clone(),
            scope,
            key,
            stream,
            route,
            self.client_return,
            rx,
        );
        Ok(())
    }

    async fn handle_exit_payload(
        self: &Arc<Self>,
        scope: Scope,
        from: Did,
        circuit_id: OnionCircuitId,
        return_peer: Did,
        client: OnionClientReturn,
        payload: OnionCircuitPayload,
    ) -> Result<()> {
        let key = TcpStreamKey { circuit_id };
        let Some(payload) = decode_tcp_payload(payload)? else {
            return Ok(());
        };
        match payload {
            OnionTcpPayload::Open { target } => {
                self.open_exit_stream(TcpExitOpen {
                    scope,
                    key,
                    circuit_id,
                    return_peer,
                    client,
                    expected_forward_peer: from,
                    target,
                })
                .await
            }
            OnionTcpPayload::Data { bytes } => {
                self.send_exit_inbound(key, from, TcpInbound::Data(bytes))
                    .await
            }
            OnionTcpPayload::Shutdown => {
                self.send_exit_inbound(key, from, TcpInbound::Shutdown)
                    .await
            }
            OnionTcpPayload::Close => self.send_exit_inbound(key, from, TcpInbound::Close).await,
            OnionTcpPayload::Error { .. } => Ok(()),
        }
    }

    async fn handle_client_payload(
        self: &Arc<Self>,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: OnionAuthenticatedPayload,
    ) -> Result<()> {
        let key = TcpStreamKey { circuit_id };
        let payload = self.verify_client_payload(key, from, payload)?;
        let Some(payload) = decode_tcp_payload(payload)? else {
            return Ok(());
        };
        match payload {
            OnionTcpPayload::Data { bytes } => {
                self.send_client_inbound(key, from, TcpInbound::Data(bytes))
                    .await
            }
            OnionTcpPayload::Shutdown => {
                self.send_client_inbound(key, from, TcpInbound::Shutdown)
                    .await
            }
            OnionTcpPayload::Close => self.send_client_inbound(key, from, TcpInbound::Close).await,
            OnionTcpPayload::Error { message } => {
                self.send_client_inbound(key, from, TcpInbound::Error(message))
                    .await
            }
            OnionTcpPayload::Open { .. } => Ok(()),
        }
    }

    async fn open_exit_stream(self: &Arc<Self>, request: TcpExitOpen) -> Result<()> {
        let TcpExitOpen {
            scope,
            key,
            circuit_id,
            return_peer,
            client,
            expected_forward_peer,
            target,
        } = request;
        let Some(policy) = &self.exit_policy else {
            return send_tcp_backward(
                &scope,
                &self.session_sk,
                circuit_id,
                return_peer,
                client,
                OnionTcpPayload::Error {
                    message: "native onion TCP exit is not enabled locally".to_string(),
                },
            )
            .await;
        };
        let target = OnionProxyTarget::parse_authority(&target)?;
        let authority = target.authority();
        if !policy.allows_target(&authority) {
            return send_tcp_backward(
                &scope,
                &self.session_sk,
                circuit_id,
                return_peer,
                client,
                OnionTcpPayload::Error {
                    message: Error::NoPermission.to_string(),
                },
            )
            .await;
        }
        let (tx, rx) = mpsc::channel(32);
        if let Err(error) = self.insert_exit_stream(key, expected_forward_peer, tx) {
            return send_tcp_backward(
                &scope,
                &self.session_sk,
                circuit_id,
                return_peer,
                client,
                OnionTcpPayload::Error {
                    message: error.to_string(),
                },
            )
            .await;
        }
        let lease = match self.admit_exit_stream(policy, circuit_id, return_peer, 0) {
            Ok(lease) => lease,
            Err(error) => {
                self.remove_exit_stream(key);
                return send_tcp_backward(
                    &scope,
                    &self.session_sk,
                    circuit_id,
                    return_peer,
                    client,
                    OnionTcpPayload::Error {
                        message: error.to_string(),
                    },
                )
                .await;
            }
        };
        let addr = match resolve_target(&authority).await {
            Ok(addr) => addr,
            Err(error) => {
                self.remove_exit_stream(key);
                drop(lease);
                return send_tcp_backward(
                    &scope,
                    &self.session_sk,
                    circuit_id,
                    return_peer,
                    client,
                    OnionTcpPayload::Error {
                        message: error.to_string(),
                    },
                )
                .await;
            }
        };
        let stream = match TcpStream::connect(addr).await {
            Ok(stream) => stream,
            Err(error) => {
                self.remove_exit_stream(key);
                drop(lease);
                return send_tcp_backward(
                    &scope,
                    &self.session_sk,
                    circuit_id,
                    return_peer,
                    client,
                    OnionTcpPayload::Error {
                        message: format!("connect onion TCP target {authority:?}: {error}"),
                    },
                )
                .await;
            }
        };
        spawn_exit_stream(ExitStreamTask {
            runtime: self.clone(),
            scope,
            key,
            circuit_id,
            return_peer,
            client,
            stream,
            rx,
            lease,
        });
        Ok(())
    }

    fn insert_client_stream(
        &self,
        expected_return_peer: Did,
        expected_exit: OnionExitDescriptor,
        tx: mpsc::Sender<TcpInbound>,
    ) -> Result<TcpStreamKey> {
        let mut streams = self.client_streams.lock().map_err(|_| Error::Lock)?;
        for _ in 0..16 {
            let key = TcpStreamKey {
                circuit_id: OnionCircuitId::random(),
            };
            match streams.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(ClientStream {
                        expected_return_peer,
                        expected_exit,
                        consumed_backward_nonces: HashSet::new(),
                        tx,
                    });
                    return Ok(key);
                }
                Entry::Occupied(_) => {}
            }
        }
        Err(Error::OnionRouteError(
            "failed to allocate unique onion TCP stream id".to_string(),
        ))
    }

    fn insert_exit_stream(
        &self,
        key: TcpStreamKey,
        expected_forward_peer: Did,
        tx: mpsc::Sender<TcpInbound>,
    ) -> Result<()> {
        let mut streams = self.exit_streams.lock().map_err(|_| Error::Lock)?;
        match streams.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(ExitStream {
                    expected_forward_peer,
                    tx,
                });
                Ok(())
            }
            Entry::Occupied(_) => Err(Error::OnionRouteError(
                "duplicate onion TCP open for live circuit".to_string(),
            )),
        }
    }

    async fn send_client_inbound(
        &self,
        key: TcpStreamKey,
        from: Did,
        inbound: TcpInbound,
    ) -> Result<()> {
        let tx = self.client_inbound_sender(key, from)?;
        tx.send(inbound)
            .await
            .map_err(|_| Error::OnionRouteError("onion TCP stream is closed".to_string()))
    }

    async fn send_exit_inbound(
        &self,
        key: TcpStreamKey,
        from: Did,
        inbound: TcpInbound,
    ) -> Result<()> {
        let tx = self.exit_inbound_sender(key, from)?;
        tx.send(inbound)
            .await
            .map_err(|_| Error::OnionRouteError("onion TCP stream is closed".to_string()))
    }

    fn client_inbound_sender(
        &self,
        key: TcpStreamKey,
        from: Did,
    ) -> Result<mpsc::Sender<TcpInbound>> {
        let streams = self.client_streams.lock().map_err(|_| Error::Lock)?;
        let stream = streams
            .get(&key)
            .ok_or_else(|| Error::OnionRouteError("unknown onion TCP stream".to_string()))?;
        if stream.expected_return_peer != from {
            return Err(Error::OnionRouteError(format!(
                "unexpected onion TCP return peer: expected {:?}, got {:?}",
                stream.expected_return_peer, from
            )));
        }
        Ok(stream.tx.clone())
    }

    fn verify_client_payload(
        &self,
        key: TcpStreamKey,
        from: Did,
        payload: OnionAuthenticatedPayload,
    ) -> Result<OnionCircuitPayload> {
        let expected_exit = {
            let streams = self.client_streams.lock().map_err(|_| Error::Lock)?;
            let stream = streams
                .get(&key)
                .ok_or_else(|| Error::OnionRouteError("unknown onion TCP stream".to_string()))?;
            if stream.expected_return_peer != from {
                return Err(Error::OnionRouteError(format!(
                    "unexpected onion TCP return peer: expected {:?}, got {:?}",
                    stream.expected_return_peer, from
                )));
            }
            stream.expected_exit.clone()
        };
        let verified = payload.into_verified_payload(key.circuit_id, &expected_exit)?;
        self.consume_backward_nonce(key, from, verified.nonce)?;
        Ok(verified.payload)
    }

    fn consume_backward_nonce(
        &self,
        key: TcpStreamKey,
        from: Did,
        nonce: OnionBackwardNonce,
    ) -> Result<()> {
        let mut streams = self.client_streams.lock().map_err(|_| Error::Lock)?;
        let stream = streams
            .get_mut(&key)
            .ok_or_else(|| Error::OnionRouteError("unknown onion TCP stream".to_string()))?;
        if stream.expected_return_peer != from {
            return Err(Error::OnionRouteError(format!(
                "unexpected onion TCP return peer: expected {:?}, got {:?}",
                stream.expected_return_peer, from
            )));
        }
        if !stream.consumed_backward_nonces.insert(nonce) {
            return Err(Error::OnionRouteError(
                "replayed onion TCP backward payload".to_string(),
            ));
        }
        Ok(())
    }

    fn exit_inbound_sender(
        &self,
        key: TcpStreamKey,
        from: Did,
    ) -> Result<mpsc::Sender<TcpInbound>> {
        let streams = self.exit_streams.lock().map_err(|_| Error::Lock)?;
        let stream = streams
            .get(&key)
            .ok_or_else(|| Error::OnionRouteError("unknown onion TCP stream".to_string()))?;
        if stream.expected_forward_peer != from {
            return Err(Error::OnionRouteError(format!(
                "unexpected onion TCP forward peer: expected {:?}, got {:?}",
                stream.expected_forward_peer, from
            )));
        }
        Ok(stream.tx.clone())
    }

    fn remove_client_stream(&self, key: TcpStreamKey) {
        if let Ok(mut streams) = self.client_streams.lock() {
            streams.remove(&key);
        }
    }

    fn remove_exit_stream(&self, key: TcpStreamKey) {
        if let Ok(mut streams) = self.exit_streams.lock() {
            streams.remove(&key);
        }
    }

    fn admit_exit_stream(
        &self,
        policy: &OnionExitPolicy,
        circuit_id: OnionCircuitId,
        return_peer: Did,
        bytes: u64,
    ) -> Result<OnionExitLease> {
        self.accounting
            .admit(policy, circuit_id, return_peer, bytes)
    }

    fn record_exit_bytes(&self, policy: &OnionExitPolicy, bytes: u64) -> Result<()> {
        self.accounting.record_bytes(policy, bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TcpStreamKey {
    circuit_id: OnionCircuitId,
}

struct TcpExitOpen {
    scope: Scope,
    key: TcpStreamKey,
    circuit_id: OnionCircuitId,
    return_peer: Did,
    client: OnionClientReturn,
    expected_forward_peer: Did,
    target: String,
}

// Invariant: each nonce in `consumed_backward_nonces` has already produced at most one
// `TcpInbound` event for this client stream.
// Preservation: `verify_client_payload` verifies the exit proof first, then inserts the nonce before
// decoding the TCP payload; duplicate nonce insertion fails before bytes reach the stream.
struct ClientStream {
    expected_return_peer: Did,
    expected_exit: OnionExitDescriptor,
    consumed_backward_nonces: HashSet<OnionBackwardNonce>,
    tx: mpsc::Sender<TcpInbound>,
}

struct ExitStream {
    expected_forward_peer: Did,
    tx: mpsc::Sender<TcpInbound>,
}

#[derive(Debug)]
enum TcpInbound {
    Data(Bytes),
    Shutdown,
    Close,
    Error(String),
}

/// TCP duplex close state for one onion stream.
///
/// Invariant: `remote_terminal_seen => !read_open && !write_open`.
/// Preservation: local half-closes only clear one half and still announce a terminal frame when
/// both halves close; observing a remote terminal clears both halves and suppresses echo.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TcpDuplexState {
    read_open: bool,
    write_open: bool,
    remote_terminal_seen: bool,
}

impl TcpDuplexState {
    const fn open() -> Self {
        Self {
            read_open: true,
            write_open: true,
            remote_terminal_seen: false,
        }
    }

    const fn can_read(self) -> bool {
        self.read_open
    }

    const fn can_write(self) -> bool {
        self.write_open
    }

    const fn is_closed(self) -> bool {
        !self.read_open && !self.write_open
    }

    const fn should_announce_terminal(self) -> bool {
        !self.remote_terminal_seen
    }

    fn close_read(&mut self) {
        self.read_open = false;
    }

    fn close_write(&mut self) {
        self.write_open = false;
    }

    fn observe_remote_terminal(&mut self) {
        self.read_open = false;
        self.write_open = false;
        self.remote_terminal_seen = true;
    }
}

fn spawn_client_stream(
    runtime: Arc<OnionTcpRuntime>,
    scope: Scope,
    key: TcpStreamKey,
    stream: TcpStream,
    route: OnionRoute,
    client_return: OnionClientReturn,
    mut rx: mpsc::Receiver<TcpInbound>,
) {
    tokio::spawn(async move {
        let (mut read, mut write) = stream.into_split();
        let mut read_buf = vec![0_u8; TCP_BUF];
        let mut state = TcpDuplexState::open();
        loop {
            if state.is_closed() {
                break;
            }
            tokio::select! {
                read_result = read.read(read_buf.as_mut_slice()), if state.can_read() => {
                    match read_result {
                        Ok(0) => {
                            if send_client_payload(&scope, &route, client_return, key.circuit_id, OnionTcpPayload::Shutdown).await.is_err() {
                                break;
                            }
                            state.close_read();
                        }
                        Ok(n) => {
                            let bytes = Bytes::copy_from_slice(read_buf.get(..n).unwrap_or_default());
                            if send_client_payload(&scope, &route, client_return, key.circuit_id, OnionTcpPayload::Data { bytes }).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                inbound = rx.recv() => {
                    match inbound {
                        Some(TcpInbound::Data(bytes)) => {
                            if !state.can_write() {
                                continue;
                            }
                            if write.write_all(bytes.as_ref()).await.is_err() {
                                break;
                            }
                        }
                        Some(TcpInbound::Shutdown) => {
                            if state.can_write() {
                                let _ = write.shutdown().await;
                                state.close_write();
                            }
                        }
                        Some(TcpInbound::Close) | None => {
                            state.observe_remote_terminal();
                            break;
                        }
                        Some(TcpInbound::Error(message)) => {
                            tracing::warn!("onion TCP client stream failed: {message}");
                            state.observe_remote_terminal();
                            break;
                        }
                    }
                }
            }
        }
        if state.should_announce_terminal() {
            let _ = send_client_payload(
                &scope,
                &route,
                client_return,
                key.circuit_id,
                OnionTcpPayload::Close,
            )
            .await;
        }
        runtime.remove_client_stream(key);
    });
}

struct ExitStreamTask {
    runtime: Arc<OnionTcpRuntime>,
    scope: Scope,
    key: TcpStreamKey,
    circuit_id: OnionCircuitId,
    return_peer: Did,
    client: OnionClientReturn,
    stream: TcpStream,
    rx: mpsc::Receiver<TcpInbound>,
    lease: OnionExitLease,
}

fn spawn_exit_stream(task: ExitStreamTask) {
    tokio::spawn(async move {
        let ExitStreamTask {
            runtime,
            scope,
            key,
            circuit_id,
            return_peer,
            client,
            stream,
            mut rx,
            lease,
        } = task;
        let (mut read, mut write) = stream.into_split();
        let mut read_buf = vec![0_u8; TCP_BUF];
        let mut state = TcpDuplexState::open();
        loop {
            if state.is_closed() {
                break;
            }
            tokio::select! {
                read_result = read.read(read_buf.as_mut_slice()), if state.can_read() => {
                    match read_result {
                        Ok(0) => {
                            if send_tcp_backward(&scope, &runtime.session_sk, circuit_id, return_peer, client, OnionTcpPayload::Shutdown).await.is_err() {
                                break;
                            }
                            state.close_read();
                        }
                        Ok(n) => {
                            let bytes = Bytes::copy_from_slice(read_buf.get(..n).unwrap_or_default());
                            if let Some(policy) = &runtime.exit_policy {
                                if runtime.record_exit_bytes(policy, bytes.len() as u64).is_err() {
                                    let _ = send_tcp_backward(
                                        &scope,
                                        &runtime.session_sk,
                                        circuit_id,
                                        return_peer,
                                        client,
                                        OnionTcpPayload::Error {
                                            message: Error::NoPermission.to_string(),
                                        },
                                    )
                                    .await;
                                    break;
                                }
                            }
                            if send_tcp_backward(&scope, &runtime.session_sk, circuit_id, return_peer, client, OnionTcpPayload::Data { bytes }).await.is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = send_tcp_backward(
                                &scope,
                                &runtime.session_sk,
                                circuit_id,
                                return_peer,
                                client,
                                OnionTcpPayload::Error {
                                    message: format!("read onion TCP target: {error}"),
                                },
                            )
                            .await;
                            break;
                        }
                    }
                }
                inbound = rx.recv() => {
                    match inbound {
                        Some(TcpInbound::Data(bytes)) => {
                            if !state.can_write() {
                                continue;
                            }
                            if let Some(policy) = &runtime.exit_policy {
                                if runtime.record_exit_bytes(policy, bytes.len() as u64).is_err() {
                                    let _ = send_tcp_backward(
                                        &scope,
                                        &runtime.session_sk,
                                        circuit_id,
                                        return_peer,
                                        client,
                                        OnionTcpPayload::Error {
                                            message: Error::NoPermission.to_string(),
                                        },
                                    )
                                    .await;
                                    break;
                                }
                            }
                            if write.write_all(bytes.as_ref()).await.is_err() {
                                break;
                            }
                        }
                        Some(TcpInbound::Shutdown) => {
                            if state.can_write() {
                                let _ = write.shutdown().await;
                                state.close_write();
                            }
                        }
                        Some(TcpInbound::Close) | None => {
                            state.observe_remote_terminal();
                            break;
                        }
                        Some(TcpInbound::Error(message)) => {
                            tracing::warn!("onion TCP exit stream failed: {message}");
                            state.observe_remote_terminal();
                            break;
                        }
                    }
                }
            }
        }
        if state.should_announce_terminal() {
            let _ = send_tcp_backward(
                &scope,
                &runtime.session_sk,
                circuit_id,
                return_peer,
                client,
                OnionTcpPayload::Close,
            )
            .await;
        }
        runtime.remove_exit_stream(key);
        drop(lease);
    });
}

async fn send_client_payload(
    scope: &Scope,
    route: &OnionRoute,
    client_return: OnionClientReturn,
    circuit_id: OnionCircuitId,
    payload: OnionTcpPayload,
) -> Result<()> {
    let payload = encode_tcp_payload(payload)?;
    let (to, payload) = encode_initial_forward(client_return, route, circuit_id, payload)?;
    scope.send(to, payload).await
}

async fn send_tcp_backward(
    scope: &Scope,
    signer: &SessionSk,
    circuit_id: OnionCircuitId,
    return_peer: Did,
    client: OnionClientReturn,
    payload: OnionTcpPayload,
) -> Result<()> {
    send_backward(
        scope,
        signer,
        circuit_id,
        return_peer,
        client,
        encode_tcp_payload(payload)?,
    )
    .await
}

async fn resolve_target(authority: &str) -> Result<SocketAddr> {
    lookup_host(authority)
        .await
        .map_err(|error| {
            Error::InvalidConfig(format!("resolve onion exit target {authority:?}: {error}"))
        })?
        .next()
        .ok_or_else(|| {
            Error::InvalidConfig(format!("onion exit target {authority:?} resolved empty"))
        })
}

#[cfg(test)]
mod tests;
