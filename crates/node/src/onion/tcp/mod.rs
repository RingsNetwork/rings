//! Native TCP adapter for route-aware onion circuits.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::session::SessionSk;
use serde::Deserialize;
use serde::Serialize;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio::time::Instant;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Extensions;
use crate::extension::ext::Scope;
use crate::onion::circuit::route_first_hop;
use crate::onion::circuit::send_backward;
use crate::onion::circuit::OnionAuthenticatedPayload;
use crate::onion::circuit::OnionBackwardSequence;
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
use crate::onion::circuit::OnionForwardSequence;
use crate::onion::circuit::OnionReturnId;
use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::exit_accounting::OnionExitLease;
use crate::onion::https::try_handle_https_exit_payload;
use crate::onion::https::OnionHttpsRuntime;
use crate::onion::replay::OnionForwardReplayKey;
use crate::onion::replay::OnionForwardReplayPartitions;
use crate::onion::replay::OnionSequenceWindow;
use crate::onion::replay::ReplayAdmission;
use crate::onion::replay::SequenceAdmission;
use crate::onion::target::resolve_public_target;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitFailure;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitTarget;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;

mod client;
mod config;
mod duplex;
mod exit;
mod inbound;
mod pump;

use client::spawn_client_stream;
use client::TcpBackwardRoute;
pub use config::NativeOnionTcpExitConfig;
#[cfg(test)]
use duplex::TcpDuplexState;
use exit::spawn_exit_stream;
use exit::ExitStreamTask;
use inbound::TcpInbound;

const TCP_BUF: usize = 30_000;
const TCP_OPEN_TIMEOUT_SECS: u64 = 30;
const TCP_OPEN_RESPONSE_QUANTUM_MS: u128 = 250;

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
        let https = Arc::new(OnionHttpsRuntime::new());
        if let Some(config) = runtime.exit_config.as_ref() {
            if config.allows_service(&OnionServiceName::https()) {
                https.set_exit_policy(Some(config.policy().clone()));
            }
        }
        let capabilities = OnionCircuitCapabilities::from_registration(allow_relay, allow_exit);
        let handler_session_sk = session_sk.clone();
        extensions.register(
            OnionCircuitProtocol::new(capabilities),
            OnionCircuitShell::new(session_sk, NativeOnionCircuitHandler {
                runtime: runtime.clone(),
                https,
                session_sk: handler_session_sk,
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
    https: Arc<OnionHttpsRuntime>,
    session_sk: SessionSk,
}

#[async_trait::async_trait]
impl OnionCircuitHandler for NativeOnionCircuitHandler {
    async fn handle_exit(&self, scope: &Scope, frame: OnionCircuitExitFrame) -> Result<()> {
        if frame
            .payload
            .matches_service(crate::onion::proxy::ONION_PROXY_HTTPS_SERVICE)
            && try_handle_https_exit_payload(&self.https, &self.session_sk, scope, frame.clone())
                .await?
        {
            return Ok(());
        }
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
    forward_replays: Mutex<OnionForwardReplayPartitions>,
    exit_config: Option<NativeOnionTcpExitConfig>,
    accounting: OnionExitAccounting,
}

impl OnionTcpRuntime {
    fn new(session_sk: SessionSk, exit_config: Option<NativeOnionTcpExitConfig>) -> Self {
        Self {
            session_sk,
            client_streams: Mutex::new(HashMap::new()),
            exit_streams: Mutex::new(HashMap::new()),
            forward_replays: Mutex::new(OnionForwardReplayPartitions::default()),
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
        match payload {
            OnionTcpPayload::Open { target } => {
                if frame.forward_sequence != OnionForwardSequence::FIRST {
                    return Err(Error::OnionRouteError(OnionRouteError::ForwardReplay));
                }
                self.consume_forward_nonce(frame.from, frame.circuit_id, frame.forward_nonce)?;
                self.open_exit_stream(TcpExitOpen {
                    scope,
                    opened_at: Instant::now(),
                    key,
                    circuit_id: frame.circuit_id,
                    return_peer: frame.return_peer,
                    return_session_public_key: frame.return_session_public_key,
                    client: frame.client,
                    expected_forward_peer: frame.from,
                    service,
                    target,
                })
                .await
            }
            OnionTcpPayload::Data { bytes } => self.send_exit_inbound(
                key,
                frame.from,
                &service,
                frame.forward_sequence,
                TcpInbound::Data(bytes),
            ),
            OnionTcpPayload::Shutdown => self.send_exit_inbound(
                key,
                frame.from,
                &service,
                frame.forward_sequence,
                TcpInbound::Shutdown,
            ),
            OnionTcpPayload::Close => self.send_exit_inbound(
                key,
                frame.from,
                &service,
                frame.forward_sequence,
                TcpInbound::Close,
            ),
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
            }
            OnionTcpPayload::Shutdown => self.send_client_inbound(key, from, TcpInbound::Shutdown),
            OnionTcpPayload::Close => self.send_client_inbound(key, from, TcpInbound::Close),
            OnionTcpPayload::Error(failure) => {
                if self.complete_client_open(key, from, Err(failure.clone()))? {
                    return Ok(());
                }
                self.send_client_inbound(key, from, TcpInbound::Error(failure))
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
        from: Did,
        circuit_id: OnionCircuitId,
        nonce: OnionForwardNonce,
    ) -> Result<()> {
        let mut replays = self.forward_replays.lock().map_err(|_| Error::Lock)?;
        match replays.consume(
            from,
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

        let target = match admit_exit_target(policy, &request.target) {
            Ok(target) => target,
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

        let stream = match timeout(
            Duration::from_secs(TCP_OPEN_TIMEOUT_SECS),
            connect_exit_target(&target),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(failure)) => {
                self.remove_exit_stream(request.key);
                drop(lease);
                return self.reject_exit_open(&request, failure).await;
            }
            Err(_) => {
                self.remove_exit_stream(request.key);
                drop(lease);
                return self
                    .reject_exit_open(&request, OnionExitFailure::ConnectTarget)
                    .await;
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
            return_session_public_key,
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
            return_session_public_key,
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
        self.send_exit_backward(
            request,
            OnionBackwardSequence::FIRST,
            OnionTcpPayload::Error(failure),
        )
        .await
    }

    async fn accept_exit_open(&self, request: &TcpExitOpen) -> Result<()> {
        let sequence = self.next_backward_sequence(request.key)?;
        self.send_exit_backward(request, sequence, OnionTcpPayload::Opened)
            .await
    }

    async fn send_exit_backward(
        &self,
        request: &TcpExitOpen,
        sequence: OnionBackwardSequence,
        payload: OnionTcpPayload,
    ) -> Result<()> {
        // Resolve/connect results in the same quantum share one response deadline. The state and
        // result algebra remain unchanged while remote clients lose byte-accurate resolver and
        // target-connect timing.
        tokio::time::sleep_until(open_response_deadline(request.opened_at, Instant::now())).await;
        TcpBackwardRoute {
            scope: &request.scope,
            signer: &self.session_sk,
            service: &request.service,
            circuit_id: request.circuit_id,
            return_peer: request.return_peer,
            return_session_public_key: request.return_session_public_key,
            client: request.client,
        }
        .send(sequence, payload)
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
                        backward_sequences: OnionSequenceWindow::default(),
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
                    forward_sequences: OnionSequenceWindow::with_initial(
                        OnionForwardSequence::FIRST.value(),
                    ),
                    next_backward_sequence: 0,
                    tx,
                });
                Ok(())
            }
            Entry::Occupied(_) => Err(Error::OnionRouteError(OnionRouteError::DuplicateTcpOpen)),
        }
    }

    fn send_client_inbound(&self, key: TcpStreamKey, from: Did, inbound: TcpInbound) -> Result<()> {
        let tx = self.client_inbound_sender(key, from)?;
        tx.try_send(inbound).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                self.remove_client_stream(key);
                Error::OnionRouteError(OnionRouteError::TcpStreamBackpressure)
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                Error::OnionRouteError(OnionRouteError::TcpStreamClosed)
            }
        })
    }

    fn send_exit_inbound(
        &self,
        key: TcpStreamKey,
        from: Did,
        service: &OnionServiceName,
        sequence: OnionForwardSequence,
        inbound: TcpInbound,
    ) -> Result<()> {
        let tx = self.exit_inbound_sender(key, from, service, sequence)?;
        tx.try_send(inbound).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                self.remove_exit_stream(key);
                Error::OnionRouteError(OnionRouteError::TcpStreamBackpressure)
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                Error::OnionRouteError(OnionRouteError::TcpStreamClosed)
            }
        })
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
        self.consume_backward_sequence(key, from, verified.sequence)?;
        Ok(verified.payload)
    }

    fn consume_backward_sequence(
        &self,
        key: TcpStreamKey,
        from: Did,
        sequence: OnionBackwardSequence,
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
        match stream.backward_sequences.consume(sequence.value()) {
            SequenceAdmission::Consumed => Ok(()),
            SequenceAdmission::Duplicate => {
                tracing::debug!(
                    ?key,
                    sequence = sequence.value(),
                    "duplicate onion TCP backward sequence"
                );
                Err(Error::OnionRouteError(OnionRouteError::BackwardReplay))
            }
            SequenceAdmission::Stale => {
                tracing::debug!(
                    ?key,
                    sequence = sequence.value(),
                    "stale onion TCP backward sequence"
                );
                Err(Error::OnionRouteError(OnionRouteError::BackwardReplay))
            }
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
        sequence: OnionForwardSequence,
    ) -> Result<mpsc::Sender<TcpInbound>> {
        let mut streams = self.exit_streams.lock().map_err(|_| Error::Lock)?;
        let stream = streams
            .get_mut(&key)
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
        match stream.forward_sequences.consume(sequence.value()) {
            SequenceAdmission::Consumed => {}
            SequenceAdmission::Duplicate => {
                tracing::debug!(
                    ?key,
                    sequence = sequence.value(),
                    "duplicate onion TCP forward sequence"
                );
                return Err(Error::OnionRouteError(OnionRouteError::ForwardReplay));
            }
            SequenceAdmission::Stale => {
                tracing::debug!(
                    ?key,
                    sequence = sequence.value(),
                    "stale onion TCP forward sequence"
                );
                return Err(Error::OnionRouteError(OnionRouteError::ForwardReplay));
            }
        }
        Ok(stream.tx.clone())
    }

    fn next_backward_sequence(&self, key: TcpStreamKey) -> Result<OnionBackwardSequence> {
        let mut streams = self.exit_streams.lock().map_err(|_| Error::Lock)?;
        let stream = streams
            .get_mut(&key)
            .ok_or(Error::OnionRouteError(OnionRouteError::UnknownTcpStream))?;
        let sequence = stream.next_backward_sequence;
        stream.next_backward_sequence = sequence
            .checked_add(1)
            .ok_or(Error::OnionRouteError(OnionRouteError::SequenceExhausted))?;
        Ok(OnionBackwardSequence::new(sequence))
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
    opened_at: Instant,
    key: TcpStreamKey,
    circuit_id: OnionCircuitId,
    return_peer: Did,
    return_session_public_key: PublicKey<33>,
    client: OnionClientReturn,
    expected_forward_peer: Did,
    service: OnionServiceName,
    target: String,
}

fn open_response_deadline(opened_at: Instant, now: Instant) -> Instant {
    let elapsed_ms = now.saturating_duration_since(opened_at).as_millis();
    let quanta = elapsed_ms
        .saturating_add(TCP_OPEN_RESPONSE_QUANTUM_MS - 1)
        .checked_div(TCP_OPEN_RESPONSE_QUANTUM_MS)
        .unwrap_or(1)
        .max(1);
    let deadline_ms =
        u64::try_from(quanta.saturating_mul(TCP_OPEN_RESPONSE_QUANTUM_MS)).unwrap_or(u64::MAX);
    opened_at
        .checked_add(Duration::from_millis(deadline_ms))
        .unwrap_or(now)
}

fn admit_exit_target(
    policy: &OnionExitPolicy,
    target: &str,
) -> std::result::Result<OnionProxyTarget, OnionExitFailure> {
    let target = OnionProxyTarget::parse_authority(target)
        .map_err(|error| OnionExitFailure::InvalidTarget(error.to_string()))?;
    let exit_target = OnionExitTarget::from_proxy_target(&target);
    if !policy.allows_target(&exit_target) {
        return Err(OnionExitFailure::PermissionDenied);
    }
    Ok(target)
}

async fn connect_exit_target(
    target: &OnionProxyTarget,
) -> std::result::Result<TcpStream, OnionExitFailure> {
    let authority = target.authority();
    let addresses = resolve_public_target(target).await.map_err(|error| {
        tracing::warn!(target = authority, %error, "rejected or failed to resolve onion TCP exit target");
        if matches!(error, Error::NoPermission) {
            OnionExitFailure::PermissionDenied
        } else {
            OnionExitFailure::ResolveTarget
        }
    })?;
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    if let Some(error) = last_error {
        tracing::warn!(target = authority, %error, "failed to connect onion TCP exit target");
    }
    Err(OnionExitFailure::ConnectTarget)
}

// Invariant: each sequence in `backward_sequences` has already produced at most one
// `TcpInbound` event for this client stream.
// Preservation: `verify_client_payload` verifies the exit proof and consumes the monotonic
// sequence before decoding the TCP payload; duplicate/stale sequences fail before bytes reach the
// stream.
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
    backward_sequences: OnionSequenceWindow,
    tx: mpsc::Sender<TcpInbound>,
}

// Invariant: `service` is the canonical service accepted by the Open payload that created this exit
// stream.
// Preservation: `exit_inbound_sender` rejects later payloads on the same circuit when their service
// differs from this stream service.
struct ExitStream {
    service: OnionServiceName,
    expected_forward_peer: Did,
    forward_sequences: OnionSequenceWindow,
    next_backward_sequence: u64,
    tx: mpsc::Sender<TcpInbound>,
}

#[cfg(test)]
mod tests;
