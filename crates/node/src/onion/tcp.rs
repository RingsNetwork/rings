//! Native TCP adapter for route-aware onion circuits.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::session::SessionSk;
use rings_core::utils::get_epoch_ms;
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
use crate::onion::circuit::OnionCircuitCapabilities;
use crate::onion::circuit::OnionCircuitHandler;
use crate::onion::circuit::OnionCircuitId;
use crate::onion::circuit::OnionCircuitPayload;
use crate::onion::circuit::OnionCircuitProtocol;
use crate::onion::circuit::OnionCircuitShell;
use crate::onion::circuit::OnionClientReturn;
use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;

const TCP_BUF: usize = 30_000;
const EXIT_LIMIT_WINDOW_MS: u128 = 60_000;

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
        let runtime = Arc::new(OnionTcpRuntime::new(
            OnionClientReturn::new(session_sk.session_public_key()),
            exit_policy,
        ));
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
        payload: OnionCircuitPayload,
    ) -> Result<()> {
        self.runtime
            .handle_client_payload(from, circuit_id, payload)
            .await
    }
}

struct OnionTcpRuntime {
    client_return: OnionClientReturn,
    client_streams: Mutex<HashMap<TcpStreamKey, ClientStream>>,
    exit_streams: Mutex<HashMap<TcpStreamKey, ExitStream>>,
    exit_policy: Option<OnionExitPolicy>,
    limiter: Arc<Mutex<TcpExitLimiter>>,
}

impl OnionTcpRuntime {
    fn new(client_return: OnionClientReturn, exit_policy: Option<OnionExitPolicy>) -> Self {
        Self {
            client_return,
            client_streams: Mutex::new(HashMap::new()),
            exit_streams: Mutex::new(HashMap::new()),
            exit_policy,
            limiter: Arc::new(Mutex::new(TcpExitLimiter::default())),
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
        let (tx, rx) = mpsc::channel(32);
        let key = self.insert_client_stream(expected_return_peer, tx)?;
        let (to, payload) = encode_initial_forward(
            self.client_return,
            &route,
            key.circuit_id,
            OnionCircuitPayload::TcpOpen {
                target: target.authority(),
            },
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
        match payload {
            OnionCircuitPayload::TcpOpen { target } => {
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
            OnionCircuitPayload::TcpData { bytes } => {
                self.send_exit_inbound(key, from, TcpInbound::Data(bytes))
                    .await
            }
            OnionCircuitPayload::TcpShutdown => {
                self.send_exit_inbound(key, from, TcpInbound::Shutdown)
                    .await
            }
            OnionCircuitPayload::TcpClose => {
                self.send_exit_inbound(key, from, TcpInbound::Close).await
            }
            OnionCircuitPayload::HttpsRequest(_) => {
                send_backward(
                    &scope,
                    circuit_id,
                    return_peer,
                    client,
                    OnionCircuitPayload::HttpsError(
                        "native onion exits do not support HTTPS".to_string(),
                    ),
                )
                .await
            }
            _ => Ok(()),
        }
    }

    async fn handle_client_payload(
        self: &Arc<Self>,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: OnionCircuitPayload,
    ) -> Result<()> {
        let key = TcpStreamKey { circuit_id };
        match payload {
            OnionCircuitPayload::TcpData { bytes } => {
                self.send_client_inbound(key, from, TcpInbound::Data(bytes))
                    .await
            }
            OnionCircuitPayload::TcpShutdown => {
                self.send_client_inbound(key, from, TcpInbound::Shutdown)
                    .await
            }
            OnionCircuitPayload::TcpClose => {
                self.send_client_inbound(key, from, TcpInbound::Close).await
            }
            OnionCircuitPayload::TcpError { message } => {
                self.send_client_inbound(key, from, TcpInbound::Error(message))
                    .await
            }
            _ => Ok(()),
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
            return send_backward(
                &scope,
                circuit_id,
                return_peer,
                client,
                OnionCircuitPayload::TcpError {
                    message: "native onion TCP exit is not enabled locally".to_string(),
                },
            )
            .await;
        };
        let target = OnionProxyTarget::parse_authority(&target)?;
        let authority = target.authority();
        if !policy.allows_target(&authority) {
            return send_backward(
                &scope,
                circuit_id,
                return_peer,
                client,
                OnionCircuitPayload::TcpError {
                    message: Error::NoPermission.to_string(),
                },
            )
            .await;
        }
        let (tx, rx) = mpsc::channel(32);
        if let Err(error) = self.insert_exit_stream(key, expected_forward_peer, tx) {
            return send_backward(
                &scope,
                circuit_id,
                return_peer,
                client,
                OnionCircuitPayload::TcpError {
                    message: error.to_string(),
                },
            )
            .await;
        }
        let lease = match self.admit_exit_stream(policy, circuit_id, return_peer, 0) {
            Ok(lease) => lease,
            Err(error) => {
                self.remove_exit_stream(key);
                return send_backward(
                    &scope,
                    circuit_id,
                    return_peer,
                    client,
                    OnionCircuitPayload::TcpError {
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
                return send_backward(
                    &scope,
                    circuit_id,
                    return_peer,
                    client,
                    OnionCircuitPayload::TcpError {
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
                return send_backward(
                    &scope,
                    circuit_id,
                    return_peer,
                    client,
                    OnionCircuitPayload::TcpError {
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
    ) -> Result<TcpExitLease> {
        let circuit = ExitCircuitKey::new(circuit_id, return_peer);
        let mut limiter = self.limiter.lock().map_err(|_| Error::Lock)?;
        let active_streams = limiter
            .active_streams_by_circuit
            .get(&circuit)
            .copied()
            .unwrap_or_default();
        if policy.max_streams_per_circuit > 0 && active_streams >= policy.max_streams_per_circuit {
            return Err(Error::NoPermission);
        }
        if active_streams == 0
            && policy.max_circuits > 0
            && limiter.active_circuits >= policy.max_circuits
        {
            return Err(Error::NoPermission);
        }
        if active_streams == 0 {
            limiter.active_circuits = limiter.active_circuits.saturating_add(1);
        }
        limiter
            .active_streams_by_circuit
            .insert(circuit.clone(), active_streams.saturating_add(1));
        drop(limiter);
        let lease = TcpExitLease {
            limiter: self.limiter.clone(),
            circuit,
        };
        if let Err(error) = self.record_exit_bytes(policy, bytes) {
            drop(lease);
            return Err(error);
        }
        Ok(lease)
    }

    fn record_exit_bytes(&self, policy: &OnionExitPolicy, bytes: u64) -> Result<()> {
        if policy.max_bytes_per_minute == 0 || bytes == 0 {
            return Ok(());
        }
        let mut limiter = self.limiter.lock().map_err(|_| Error::Lock)?;
        let now_ms = get_epoch_ms();
        if now_ms.saturating_sub(limiter.window_start_ms) >= EXIT_LIMIT_WINDOW_MS {
            limiter.window_start_ms = now_ms;
            limiter.bytes_this_window = 0;
        }
        let next = limiter.bytes_this_window.saturating_add(bytes);
        if next > policy.max_bytes_per_minute {
            return Err(Error::NoPermission);
        }
        limiter.bytes_this_window = next;
        Ok(())
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

struct ClientStream {
    expected_return_peer: Did,
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

#[derive(Default)]
struct TcpExitLimiter {
    active_circuits: u32,
    active_streams_by_circuit: HashMap<ExitCircuitKey, u32>,
    window_start_ms: u128,
    bytes_this_window: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ExitCircuitKey {
    circuit_id: OnionCircuitId,
    return_peer: Did,
}

impl ExitCircuitKey {
    const fn new(circuit_id: OnionCircuitId, return_peer: Did) -> Self {
        Self {
            circuit_id,
            return_peer,
        }
    }
}

struct TcpExitLease {
    limiter: Arc<Mutex<TcpExitLimiter>>,
    circuit: ExitCircuitKey,
}

impl Drop for TcpExitLease {
    fn drop(&mut self) {
        if let Ok(mut limiter) = self.limiter.lock() {
            if let Some(active_streams) = limiter.active_streams_by_circuit.get_mut(&self.circuit) {
                if *active_streams > 1 {
                    *active_streams -= 1;
                } else {
                    limiter.active_streams_by_circuit.remove(&self.circuit);
                    limiter.active_circuits = limiter.active_circuits.saturating_sub(1);
                }
            }
        }
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
                            if send_client_payload(&scope, &route, client_return, key.circuit_id, OnionCircuitPayload::TcpShutdown).await.is_err() {
                                break;
                            }
                            state.close_read();
                        }
                        Ok(n) => {
                            let bytes = Bytes::copy_from_slice(read_buf.get(..n).unwrap_or_default());
                            if send_client_payload(&scope, &route, client_return, key.circuit_id, OnionCircuitPayload::TcpData { bytes }).await.is_err() {
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
                OnionCircuitPayload::TcpClose,
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
    lease: TcpExitLease,
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
                            if send_backward(&scope, circuit_id, return_peer, client, OnionCircuitPayload::TcpShutdown).await.is_err() {
                                break;
                            }
                            state.close_read();
                        }
                        Ok(n) => {
                            let bytes = Bytes::copy_from_slice(read_buf.get(..n).unwrap_or_default());
                            if let Some(policy) = &runtime.exit_policy {
                                if runtime.record_exit_bytes(policy, bytes.len() as u64).is_err() {
                                    let _ = send_backward(
                                        &scope,
                                        circuit_id,
                                        return_peer,
                                        client,
                                        OnionCircuitPayload::TcpError {
                                            message: Error::NoPermission.to_string(),
                                        },
                                    )
                                    .await;
                                    break;
                                }
                            }
                            if send_backward(&scope, circuit_id, return_peer, client, OnionCircuitPayload::TcpData { bytes }).await.is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = send_backward(
                                &scope,
                                circuit_id,
                                return_peer,
                                client,
                                OnionCircuitPayload::TcpError {
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
                                    let _ = send_backward(
                                        &scope,
                                        circuit_id,
                                        return_peer,
                                        client,
                                        OnionCircuitPayload::TcpError {
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
            let _ = send_backward(
                &scope,
                circuit_id,
                return_peer,
                client,
                OnionCircuitPayload::TcpClose,
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
    payload: OnionCircuitPayload,
) -> Result<()> {
    let (to, payload) = encode_initial_forward(client_return, route, circuit_id, payload)?;
    scope.send(to, payload).await
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
