//! Native TCP adapter for route-aware onion circuits.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::session::SessionSk;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Extensions;
use crate::extension::ext::Scope;
use crate::onion::circuit::route_first_hop;
use crate::onion::circuit::send_backward;
use crate::onion::circuit::OnionAuthenticatedPayload;
use crate::onion::circuit::OnionBackwardNonce;
use crate::onion::circuit::OnionCircuitCapabilities;
use crate::onion::circuit::OnionCircuitExitFrame;
use crate::onion::circuit::OnionCircuitHandler;
use crate::onion::circuit::OnionCircuitId;
use crate::onion::circuit::OnionCircuitPath;
use crate::onion::circuit::OnionCircuitPayload;
use crate::onion::circuit::OnionCircuitProtocol;
use crate::onion::circuit::OnionCircuitShell;
use crate::onion::circuit::OnionClientReturn;
use crate::onion::circuit::OnionForwardNonce;
use crate::onion::circuit::OnionReturnId;
use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::exit_accounting::OnionExitLease;
use crate::onion::replay::OnionBackwardReplayCache;
use crate::onion::replay::OnionBackwardReplayKey;
use crate::onion::replay::OnionForwardReplayCache;
use crate::onion::replay::OnionForwardReplayKey;
use crate::onion::replay::ReplayAdmission;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitFailure;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitTarget;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;

mod config;
mod duplex;
mod exit;
mod inbound;
mod target;

pub use config::NativeOnionTcpExitConfig;
use duplex::TcpDuplexState;
use exit::spawn_exit_stream;
use exit::ExitStreamTask;
use inbound::TcpInbound;
use target::resolve_target;

const TCP_BUF: usize = 30_000;
const TCP_OPEN_TIMEOUT_SECS: u64 = 30;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum OnionTcpPayload {
    Open { target: String },
    Opened,
    Data { bytes: Bytes },
    Shutdown,
    Close,
    Error(OnionExitFailure),
}

fn encode_tcp_payload(
    service: &OnionServiceName,
    payload: OnionTcpPayload,
) -> Result<OnionCircuitPayload> {
    bincode::serialize(&payload)
        .map(|body| OnionCircuitPayload::new(service.clone(), Bytes::from(body)))
        .map_err(|_| Error::EncodeError)
}

fn decode_tcp_payload_for_service(
    payload: OnionCircuitPayload,
    service: &OnionServiceName,
) -> Result<Option<OnionTcpPayload>> {
    if !payload.is_service(service) {
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
        exit_config: Option<NativeOnionTcpExitConfig>,
    ) -> Result<Self> {
        let allow_exit = exit_config.is_some();
        let runtime = Arc::new(OnionTcpRuntime::new(session_sk.clone(), exit_config));
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
        let opened = self.open_tcp_stream(route, target).await?;
        opened.relay(stream);
        Ok(())
    }

    /// Open a TCP stream over `route` and wait until the exit has connected the target.
    pub async fn open_tcp_stream(
        &self,
        route: OnionRoute,
        target: OnionProxyTarget,
    ) -> Result<NativeOnionOpenStream> {
        self.runtime
            .open_client_connection(self.scope.clone(), route, target)
            .await
    }
}

/// Client-side onion TCP stream after the exit has accepted and connected the target.
pub struct NativeOnionOpenStream {
    runtime: Arc<OnionTcpRuntime>,
    scope: Scope,
    key: TcpStreamKey,
    path: OnionCircuitPath,
    client_return: OnionClientReturn,
    rx: mpsc::Receiver<TcpInbound>,
}

impl NativeOnionOpenStream {
    /// Relay `stream` through this already-open onion TCP stream.
    pub fn relay(self, stream: TcpStream) {
        spawn_client_stream(
            self.runtime,
            self.scope,
            self.key,
            stream,
            self.path,
            self.client_return,
            self.rx,
        );
    }
}

#[derive(Clone)]
struct NativeOnionCircuitHandler {
    runtime: Arc<OnionTcpRuntime>,
}

#[async_trait::async_trait]
impl OnionCircuitHandler for NativeOnionCircuitHandler {
    async fn handle_exit(&self, scope: &Scope, frame: OnionCircuitExitFrame) -> Result<()> {
        self.runtime.handle_exit_payload(scope.clone(), frame).await
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
    client_streams: Mutex<HashMap<TcpStreamKey, ClientStream>>,
    exit_streams: Mutex<HashMap<TcpStreamKey, ExitStream>>,
    forward_replays: Mutex<OnionForwardReplayCache>,
    exit_config: Option<NativeOnionTcpExitConfig>,
    accounting: OnionExitAccounting,
}

impl OnionTcpRuntime {
    fn new(session_sk: SessionSk, exit_config: Option<NativeOnionTcpExitConfig>) -> Self {
        Self {
            session_sk,
            client_streams: Mutex::new(HashMap::new()),
            exit_streams: Mutex::new(HashMap::new()),
            forward_replays: Mutex::new(OnionForwardReplayCache::default()),
            exit_config,
            accounting: OnionExitAccounting::default(),
        }
    }

    async fn open_client_connection(
        self: &Arc<Self>,
        scope: Scope,
        route: OnionRoute,
        target: OnionProxyTarget,
    ) -> Result<NativeOnionOpenStream> {
        let expected_return_peer = route_first_hop(&route)?;
        let expected_exit = route.exit().clone();
        let service = route.service_name().clone();
        let client_return = OnionClientReturn::new(self.session_sk.session_public_key());
        let (tx, rx) = mpsc::channel(32);
        let (open_tx, open_rx) = oneshot::channel();
        let key = self.insert_client_stream(
            service.clone(),
            expected_return_peer,
            expected_exit,
            client_return.return_id,
            open_tx,
            tx,
        )?;
        let path = match OnionCircuitPath::new(route, key.circuit_id) {
            Ok(path) => path,
            Err(error) => {
                self.remove_client_stream(key);
                return Err(error);
            }
        };
        let open_payload = match encode_tcp_payload(&service, OnionTcpPayload::Open {
            target: target.authority(),
        }) {
            Ok(payload) => payload,
            Err(error) => {
                self.remove_client_stream(key);
                return Err(error);
            }
        };
        let (to, payload) = match path.encode_forward(client_return, open_payload) {
            Ok(encoded) => encoded,
            Err(error) => {
                self.remove_client_stream(key);
                return Err(error);
            }
        };
        if let Err(error) = scope.send(to, payload).await {
            self.remove_client_stream(key);
            return Err(error);
        }
        match timeout(Duration::from_secs(TCP_OPEN_TIMEOUT_SECS), open_rx).await {
            Ok(Ok(Ok(()))) => Ok(NativeOnionOpenStream {
                runtime: self.clone(),
                scope,
                key,
                path,
                client_return,
                rx,
            }),
            Ok(Ok(Err(failure))) => {
                self.remove_client_stream(key);
                Err(Error::OnionRouteError(OnionRouteError::ExitFailure(
                    failure,
                )))
            }
            Ok(Err(_)) => {
                self.remove_client_stream(key);
                Err(Error::OnionRouteError(
                    OnionRouteError::TcpOpenResponseClosed,
                ))
            }
            Err(_) => {
                self.remove_client_stream(key);
                Err(Error::OnionRouteError(OnionRouteError::TcpOpenTimedOut))
            }
        }
    }

    async fn handle_exit_payload(
        self: &Arc<Self>,
        scope: Scope,
        frame: OnionCircuitExitFrame,
    ) -> Result<()> {
        let key = TcpStreamKey {
            circuit_id: frame.circuit_id,
        };
        let Some((service, payload)) = self.decode_exit_payload(frame.payload)? else {
            return Ok(());
        };
        self.consume_forward_nonce(frame.circuit_id, frame.forward_nonce)?;
        match payload {
            OnionTcpPayload::Open { target } => {
                self.open_exit_stream(TcpExitOpen {
                    scope,
                    key,
                    circuit_id: frame.circuit_id,
                    return_peer: frame.return_peer,
                    client: frame.client,
                    expected_forward_peer: frame.from,
                    service,
                    target,
                })
                .await
            }
            OnionTcpPayload::Data { bytes } => {
                self.send_exit_inbound(key, frame.from, &service, TcpInbound::Data(bytes))
                    .await
            }
            OnionTcpPayload::Shutdown => {
                self.send_exit_inbound(key, frame.from, &service, TcpInbound::Shutdown)
                    .await
            }
            OnionTcpPayload::Close => {
                self.send_exit_inbound(key, frame.from, &service, TcpInbound::Close)
                    .await
            }
            OnionTcpPayload::Opened | OnionTcpPayload::Error(_) => Ok(()),
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
        let service = self.client_stream_service(key, from)?;
        let Some(payload) = decode_tcp_payload_for_service(payload, &service)? else {
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
            OnionTcpPayload::Error(failure) => {
                if self.complete_client_open(key, from, Err(failure.clone()))? {
                    return Ok(());
                }
                self.send_client_inbound(key, from, TcpInbound::Error(failure))
                    .await
            }
            OnionTcpPayload::Opened => {
                self.complete_client_open(key, from, Ok(()))?;
                Ok(())
            }
            OnionTcpPayload::Open { .. } => Ok(()),
        }
    }

    fn consume_forward_nonce(
        &self,
        circuit_id: OnionCircuitId,
        nonce: OnionForwardNonce,
    ) -> Result<()> {
        let mut replays = self.forward_replays.lock().map_err(|_| Error::Lock)?;
        match replays.consume(
            OnionForwardReplayKey::new(circuit_id, nonce),
            rings_core::utils::get_epoch_ms(),
        ) {
            ReplayAdmission::Consumed => Ok(()),
            ReplayAdmission::Duplicate => {
                Err(Error::OnionRouteError(OnionRouteError::ForwardReplay))
            }
            ReplayAdmission::Full => Err(Error::NoPermission),
        }
    }

    fn decode_exit_payload(
        &self,
        payload: OnionCircuitPayload,
    ) -> Result<Option<(OnionServiceName, OnionTcpPayload)>> {
        let service = payload.service_name().clone();
        if !self.accepts_exit_service(&service) {
            return Ok(None);
        }
        decode_tcp_payload_for_service(payload, &service)
            .map(|payload| payload.map(|payload| (service, payload)))
    }

    fn accepts_exit_service(&self, service: &OnionServiceName) -> bool {
        self.exit_config
            .as_ref()
            .is_some_and(|config| config.allows_service(service))
    }

    async fn open_exit_stream(self: &Arc<Self>, request: TcpExitOpen) -> Result<()> {
        let Some(exit_config) = &self.exit_config else {
            return self
                .reject_exit_open(&request, OnionExitFailure::ExitUnavailable)
                .await;
        };
        if !exit_config.allows_service(&request.service) {
            return self
                .reject_exit_open(&request, OnionExitFailure::ExitUnavailable)
                .await;
        }
        let policy = exit_config.policy();

        let authority = match admit_exit_target(policy, &request.target) {
            Ok(authority) => authority,
            Err(failure) => return self.reject_exit_open(&request, failure).await,
        };
        let (rx, lease) = match self.reserve_exit_stream(&request, policy) {
            Ok(reserved) => reserved,
            Err(error) => {
                return self
                    .reject_exit_open(&request, OnionExitFailure::from_error(&error))
                    .await;
            }
        };

        let stream = match connect_exit_target(&authority).await {
            Ok(stream) => stream,
            Err(failure) => {
                self.remove_exit_stream(request.key);
                drop(lease);
                return self.reject_exit_open(&request, failure).await;
            }
        };
        if let Err(error) = self.accept_exit_open(&request).await {
            self.remove_exit_stream(request.key);
            drop(lease);
            return Err(error);
        }
        let TcpExitOpen {
            scope,
            key,
            circuit_id,
            return_peer,
            client,
            service,
            ..
        } = request;
        spawn_exit_stream(ExitStreamTask {
            runtime: self.clone(),
            scope,
            key,
            circuit_id,
            return_peer,
            client,
            service,
            stream,
            rx,
            lease,
        });
        Ok(())
    }

    async fn reject_exit_open(
        &self,
        request: &TcpExitOpen,
        failure: OnionExitFailure,
    ) -> Result<()> {
        send_tcp_backward(
            &request.scope,
            &self.session_sk,
            &request.service,
            request.circuit_id,
            request.return_peer,
            request.client,
            OnionTcpPayload::Error(failure),
        )
        .await
    }

    async fn accept_exit_open(&self, request: &TcpExitOpen) -> Result<()> {
        send_tcp_backward(
            &request.scope,
            &self.session_sk,
            &request.service,
            request.circuit_id,
            request.return_peer,
            request.client,
            OnionTcpPayload::Opened,
        )
        .await
    }

    fn reserve_exit_stream(
        &self,
        request: &TcpExitOpen,
        policy: &OnionExitPolicy,
    ) -> Result<(mpsc::Receiver<TcpInbound>, OnionExitLease)> {
        let (tx, rx) = mpsc::channel(32);
        self.insert_exit_stream(
            request.key,
            request.service.clone(),
            request.expected_forward_peer,
            tx,
        )?;
        match self.admit_exit_stream(policy, request.circuit_id, request.return_peer, 0) {
            Ok(lease) => Ok((rx, lease)),
            Err(error) => {
                self.remove_exit_stream(request.key);
                Err(error)
            }
        }
    }

    fn insert_client_stream(
        &self,
        service: OnionServiceName,
        expected_return_peer: Did,
        expected_exit: OnionExitDescriptor,
        return_id: OnionReturnId,
        open_ack: oneshot::Sender<std::result::Result<(), OnionExitFailure>>,
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
                        service,
                        expected_return_peer,
                        expected_exit,
                        return_id,
                        open_ack: Some(open_ack),
                        backward_replays: OnionBackwardReplayCache::default(),
                        tx,
                    });
                    return Ok(key);
                }
                Entry::Occupied(_) => {}
            }
        }
        Err(Error::OnionRouteError(
            OnionRouteError::CircuitIdAllocationFailed,
        ))
    }

    fn insert_exit_stream(
        &self,
        key: TcpStreamKey,
        service: OnionServiceName,
        expected_forward_peer: Did,
        tx: mpsc::Sender<TcpInbound>,
    ) -> Result<()> {
        let mut streams = self.exit_streams.lock().map_err(|_| Error::Lock)?;
        match streams.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(ExitStream {
                    service,
                    expected_forward_peer,
                    tx,
                });
                Ok(())
            }
            Entry::Occupied(_) => Err(Error::OnionRouteError(OnionRouteError::DuplicateTcpOpen)),
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
            .map_err(|_| Error::OnionRouteError(OnionRouteError::TcpStreamClosed))
    }

    async fn send_exit_inbound(
        &self,
        key: TcpStreamKey,
        from: Did,
        service: &OnionServiceName,
        inbound: TcpInbound,
    ) -> Result<()> {
        let tx = self.exit_inbound_sender(key, from, service)?;
        tx.send(inbound)
            .await
            .map_err(|_| Error::OnionRouteError(OnionRouteError::TcpStreamClosed))
    }

    fn client_stream_service(&self, key: TcpStreamKey, from: Did) -> Result<OnionServiceName> {
        let streams = self.client_streams.lock().map_err(|_| Error::Lock)?;
        let stream = streams
            .get(&key)
            .ok_or(Error::OnionRouteError(OnionRouteError::UnknownTcpStream))?;
        if stream.expected_return_peer != from {
            return Err(Error::OnionRouteError(
                OnionRouteError::UnexpectedTcpReturnPeer {
                    expected: stream.expected_return_peer,
                    actual: from,
                },
            ));
        }
        Ok(stream.service.clone())
    }

    fn client_inbound_sender(
        &self,
        key: TcpStreamKey,
        from: Did,
    ) -> Result<mpsc::Sender<TcpInbound>> {
        let streams = self.client_streams.lock().map_err(|_| Error::Lock)?;
        let stream = streams
            .get(&key)
            .ok_or(Error::OnionRouteError(OnionRouteError::UnknownTcpStream))?;
        if stream.expected_return_peer != from {
            return Err(Error::OnionRouteError(
                OnionRouteError::UnexpectedTcpReturnPeer {
                    expected: stream.expected_return_peer,
                    actual: from,
                },
            ));
        }
        Ok(stream.tx.clone())
    }

    fn verify_client_payload(
        &self,
        key: TcpStreamKey,
        from: Did,
        payload: OnionAuthenticatedPayload,
    ) -> Result<OnionCircuitPayload> {
        let (service, expected_exit, return_id) = {
            let streams = self.client_streams.lock().map_err(|_| Error::Lock)?;
            let stream = streams
                .get(&key)
                .ok_or(Error::OnionRouteError(OnionRouteError::UnknownTcpStream))?;
            if stream.expected_return_peer != from {
                return Err(Error::OnionRouteError(
                    OnionRouteError::UnexpectedTcpReturnPeer {
                        expected: stream.expected_return_peer,
                        actual: from,
                    },
                ));
            }
            (
                stream.service.clone(),
                stream.expected_exit.clone(),
                stream.return_id,
            )
        };
        let verified = payload.into_verified_payload(return_id, &expected_exit)?;
        if !verified.payload.is_service(&service) {
            return Err(Error::OnionRouteError(
                OnionRouteError::PayloadServiceMismatch {
                    payload_service: verified.payload.service().to_string(),
                    route_service: service.as_str().to_string(),
                },
            ));
        }
        self.consume_backward_nonce(key, from, verified.return_id, verified.nonce)?;
        Ok(verified.payload)
    }

    fn consume_backward_nonce(
        &self,
        key: TcpStreamKey,
        from: Did,
        return_id: OnionReturnId,
        nonce: OnionBackwardNonce,
    ) -> Result<()> {
        let mut streams = self.client_streams.lock().map_err(|_| Error::Lock)?;
        let stream = streams
            .get_mut(&key)
            .ok_or(Error::OnionRouteError(OnionRouteError::UnknownTcpStream))?;
        if stream.expected_return_peer != from {
            return Err(Error::OnionRouteError(
                OnionRouteError::UnexpectedTcpReturnPeer {
                    expected: stream.expected_return_peer,
                    actual: from,
                },
            ));
        }
        match stream.backward_replays.consume(
            OnionBackwardReplayKey::new(return_id, nonce),
            rings_core::utils::get_epoch_ms(),
        ) {
            ReplayAdmission::Consumed => Ok(()),
            ReplayAdmission::Duplicate => {
                Err(Error::OnionRouteError(OnionRouteError::BackwardReplay))
            }
            ReplayAdmission::Full => Err(Error::NoPermission),
        }
    }

    fn complete_client_open(
        &self,
        key: TcpStreamKey,
        from: Did,
        result: std::result::Result<(), OnionExitFailure>,
    ) -> Result<bool> {
        let mut streams = self.client_streams.lock().map_err(|_| Error::Lock)?;
        let stream = streams
            .get_mut(&key)
            .ok_or(Error::OnionRouteError(OnionRouteError::UnknownTcpStream))?;
        if stream.expected_return_peer != from {
            return Err(Error::OnionRouteError(
                OnionRouteError::UnexpectedTcpReturnPeer {
                    expected: stream.expected_return_peer,
                    actual: from,
                },
            ));
        }
        let Some(open_ack) = stream.open_ack.take() else {
            return Ok(false);
        };
        let _ = open_ack.send(result);
        Ok(true)
    }

    fn exit_inbound_sender(
        &self,
        key: TcpStreamKey,
        from: Did,
        service: &OnionServiceName,
    ) -> Result<mpsc::Sender<TcpInbound>> {
        let streams = self.exit_streams.lock().map_err(|_| Error::Lock)?;
        let stream = streams
            .get(&key)
            .ok_or(Error::OnionRouteError(OnionRouteError::UnknownTcpStream))?;
        if stream.expected_forward_peer != from {
            return Err(Error::OnionRouteError(
                OnionRouteError::UnexpectedTcpForwardPeer {
                    expected: stream.expected_forward_peer,
                    actual: from,
                },
            ));
        }
        if &stream.service != service {
            return Err(Error::OnionRouteError(
                OnionRouteError::PayloadServiceMismatch {
                    payload_service: service.as_str().to_string(),
                    route_service: stream.service.as_str().to_string(),
                },
            ));
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
    service: OnionServiceName,
    target: String,
}

fn admit_exit_target(
    policy: &OnionExitPolicy,
    target: &str,
) -> std::result::Result<String, OnionExitFailure> {
    let target = OnionProxyTarget::parse_authority(target)
        .map_err(|error| OnionExitFailure::InvalidTarget(error.to_string()))?;
    let authority = target.authority();
    let exit_target = OnionExitTarget::from_proxy_target(&target);
    if !policy.allows_target(&exit_target) {
        return Err(OnionExitFailure::PermissionDenied);
    }
    Ok(authority)
}

async fn connect_exit_target(authority: &str) -> std::result::Result<TcpStream, OnionExitFailure> {
    let addr = resolve_target(authority)
        .await
        .map_err(|error| OnionExitFailure::ResolveTarget(error.to_string()))?;
    TcpStream::connect(addr).await.map_err(|error| {
        OnionExitFailure::ConnectTarget(format!("connect onion TCP target {authority:?}: {error}"))
    })
}

// Invariant: each nonce in `backward_replays` has already produced at most one
// `TcpInbound` event for this client stream.
// Preservation: `verify_client_payload` verifies the exit proof first, then inserts the nonce before
// decoding the TCP payload; duplicate nonce insertion fails before bytes reach the stream.
// Invariant: `service` is the canonical route service used for every client-to-exit payload on this
// stream.
// Preservation: `verify_client_payload` rejects signed backward payloads whose service differs
// from this stream service before bytes reach the stream.
struct ClientStream {
    service: OnionServiceName,
    expected_return_peer: Did,
    expected_exit: OnionExitDescriptor,
    return_id: OnionReturnId,
    open_ack: Option<oneshot::Sender<std::result::Result<(), OnionExitFailure>>>,
    backward_replays: OnionBackwardReplayCache,
    tx: mpsc::Sender<TcpInbound>,
}

// Invariant: `service` is the canonical service accepted by the Open payload that created this exit
// stream.
// Preservation: `exit_inbound_sender` rejects later payloads on the same circuit when their service
// differs from this stream service.
struct ExitStream {
    service: OnionServiceName,
    expected_forward_peer: Did,
    tx: mpsc::Sender<TcpInbound>,
}

fn spawn_client_stream(
    runtime: Arc<OnionTcpRuntime>,
    scope: Scope,
    key: TcpStreamKey,
    stream: TcpStream,
    path: OnionCircuitPath,
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
                            if send_client_payload(&scope, &path, client_return, OnionTcpPayload::Shutdown).await.is_err() {
                                break;
                            }
                            state.close_read();
                        }
                        Ok(n) => {
                            let bytes = Bytes::copy_from_slice(read_buf.get(..n).unwrap_or_default());
                            if send_client_payload(&scope, &path, client_return, OnionTcpPayload::Data { bytes }).await.is_err() {
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
            let _ = send_client_payload(&scope, &path, client_return, OnionTcpPayload::Close).await;
        }
        runtime.remove_client_stream(key);
    });
}

async fn send_client_payload(
    scope: &Scope,
    path: &OnionCircuitPath,
    client_return: OnionClientReturn,
    payload: OnionTcpPayload,
) -> Result<()> {
    let payload = encode_tcp_payload(path.service_name(), payload)?;
    let (to, payload) = path.encode_forward(client_return, payload)?;
    scope.send(to, payload).await
}

async fn send_tcp_backward(
    scope: &Scope,
    signer: &SessionSk,
    service: &OnionServiceName,
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
        encode_tcp_payload(service, payload)?,
    )
    .await
}

#[cfg(test)]
mod tests;
