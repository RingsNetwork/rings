//! Native TCP adapter for route-aware onion circuits.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::message::MessageSigner;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio::time::Instant;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::circuit::route_first_hop;
use crate::onion::circuit::send_backward;
use crate::onion::circuit::OnionAuthenticatedPayload;
use crate::onion::circuit::OnionBackwardSequence;
use crate::onion::circuit::OnionCircuitExitFrame;
use crate::onion::circuit::OnionCircuitId;
use crate::onion::circuit::OnionCircuitPath;
use crate::onion::circuit::OnionCircuitPayload;
use crate::onion::circuit::OnionClientReturn;
#[cfg(test)]
use crate::onion::circuit::OnionForwardNonce;
use crate::onion::circuit::OnionForwardSequence;
use crate::onion::circuit::OnionLinkSender;
use crate::onion::circuit::OnionReturnId;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::exit_accounting::OnionExitLease;
use crate::onion::replay::OnionForwardReplayWitness;
use crate::onion::replay::OnionSequenceWindow;
use crate::onion::replay::SequenceAdmission;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitFailure;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;
use crate::sync_lock::lock;

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
use exit::admit_exit_target;
use exit::connect_exit_target;
use exit::open_response_deadline;
use exit::spawn_exit_stream;
use exit::ExitStreamTask;
use inbound::TcpInbound;

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
    rings_codec::serialize(&payload)
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
    decode_tcp_payload(payload.body.as_ref()).map(Some)
}

/// Decode the body of one TCP frame.
fn decode_tcp_payload(body: &[u8]) -> Result<OnionTcpPayload> {
    rings_codec::deserialize(body).map_err(|_| Error::DecodeError)
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
    pub fn relay<S>(self, stream: S)
    where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static {
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

/// Client and exit stream tables of the native TCP adapter.
pub(in crate::onion) struct OnionTcpRuntime {
    /// The delegatee key decrypts inbound cells; paired with the overlay it signs backward payloads.
    signer: MessageSigner<DelegateeKey>,
    client_streams: Mutex<HashMap<TcpStreamKey, ClientStream>>,
    exit_streams: Mutex<HashMap<TcpStreamKey, ExitStream>>,
    /// Replay authority shared with the HTTPS adapter installed on this node.
    forward_replays: OnionForwardReplayWitness,
    exit_config: Option<NativeOnionTcpExitConfig>,
    accounting: OnionExitAccounting,
    link_sender: OnionLinkSender,
}

impl OnionTcpRuntime {
    #[cfg(test)]
    fn new(
        delegatee_key: DelegateeKey,
        network_id: u32,
        exit_config: Option<NativeOnionTcpExitConfig>,
    ) -> Self {
        Self::with_resources(
            delegatee_key,
            network_id,
            exit_config,
            OnionExitAccounting::default(),
            OnionLinkSender::default(),
            OnionForwardReplayWitness::default(),
        )
    }

    /// Create a runtime sharing node-wide accounting, link and replay resources.
    pub(in crate::onion) fn with_resources(
        delegatee_key: DelegateeKey,
        network_id: u32,
        exit_config: Option<NativeOnionTcpExitConfig>,
        accounting: OnionExitAccounting,
        link_sender: OnionLinkSender,
        forward_replays: OnionForwardReplayWitness,
    ) -> Self {
        Self {
            signer: MessageSigner::new(delegatee_key, network_id),
            client_streams: Mutex::new(HashMap::new()),
            exit_streams: Mutex::new(HashMap::new()),
            forward_replays,
            exit_config,
            accounting,
            link_sender,
        }
    }

    /// Installed exit configuration; `None` means client-only mode.
    pub(in crate::onion) fn exit_config(&self) -> Option<&NativeOnionTcpExitConfig> {
        self.exit_config.as_ref()
    }

    /// Link outbox shared by every adapter of this node's circuit protocol.
    pub(in crate::onion) const fn link_sender(&self) -> &OnionLinkSender {
        &self.link_sender
    }

    /// The authority that signs this exit's backward payloads.
    fn message_signer(&self) -> MessageSigner<&DelegateeKey> {
        self.signer.by_ref()
    }

    /// Open one client stream over `route` and wait for the exit's open answer.
    pub(in crate::onion) async fn open_client_connection(
        self: &Arc<Self>,
        scope: Scope,
        route: OnionRoute,
        target: OnionProxyTarget,
    ) -> Result<NativeOnionOpenStream> {
        let expected_return_peer = route_first_hop(&route);
        let expected_exit = route.exit().clone();
        let service = route.service_name().clone();
        let client_return = OnionClientReturn::new(self.signer.delegatee_public_key());
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
        let (first_link, payload) = match path.encode_forward(client_return, open_payload) {
            Ok(encoded) => encoded,
            Err(error) => {
                self.remove_client_stream(key);
                return Err(error);
            }
        };
        if let Err(error) = self
            .link_sender
            .send_sealed(scope.clone(), first_link, payload)
            .await
        {
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

    /// Apply one forward frame addressed to this node as a TCP exit.
    pub(in crate::onion) async fn handle_exit_payload(
        self: &Arc<Self>,
        scope: Scope,
        frame: OnionCircuitExitFrame,
    ) -> Result<()> {
        let key = TcpStreamKey {
            circuit_id: frame.circuit_id,
        };
        let Some((service, payload, policy)) = self.decode_exit_payload(frame.payload)? else {
            return Ok(());
        };
        match payload {
            OnionTcpPayload::Open { target } => {
                if frame.forward_sequence != OnionForwardSequence::FIRST {
                    return Err(Error::OnionRouteError(OnionRouteError::ForwardReplay));
                }
                self.forward_replays.consume_forward_nonce(
                    frame.from,
                    frame.circuit_id,
                    frame.forward_nonce,
                )?;
                self.open_exit_stream(
                    TcpExitOpen {
                        scope,
                        opened_at: Instant::now(),
                        key,
                        circuit_id: frame.circuit_id,
                        return_peer: frame.return_peer,
                        return_delegatee_public_key: frame.return_delegatee_public_key,
                        client: frame.client,
                        expected_forward_peer: frame.from,
                        service,
                        target,
                    },
                    policy,
                )
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

    /// Apply one backward frame to the client stream owning its circuit.
    pub(in crate::onion) async fn handle_client_payload(
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

    /// Decode one exit frame routed here by the node's Σ-algebra.
    ///
    /// Pre: the algebra registers this runtime only for configured services (`Σ_n`), so the
    /// frame's service needs no second check here; a runtime without an exit configuration
    /// serves nothing.
    fn decode_exit_payload(
        &self,
        payload: OnionCircuitPayload,
    ) -> Result<Option<(OnionServiceName, OnionTcpPayload, OnionExitPolicy)>> {
        let Some(exit_config) = self.exit_config.as_ref() else {
            return Ok(None);
        };
        decode_tcp_payload(payload.body.as_ref()).map(|decoded| {
            Some((
                payload.service_name().clone(),
                decoded,
                exit_config.policy().clone(),
            ))
        })
    }

    async fn open_exit_stream(
        self: &Arc<Self>,
        request: TcpExitOpen,
        policy: OnionExitPolicy,
    ) -> Result<()> {
        let target = match admit_exit_target(&policy, &request.target) {
            Ok(target) => target,
            Err(failure) => return self.reject_exit_open(&request, failure).await,
        };
        let (rx, lease) = match self.reserve_exit_stream(&request, &policy) {
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
            return_delegatee_public_key,
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
            return_delegatee_public_key,
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
            link_sender: &self.link_sender,
            scope: &request.scope,
            signer: self.message_signer(),
            service: &request.service,
            circuit_id: request.circuit_id,
            return_peer: request.return_peer,
            return_delegatee_public_key: request.return_delegatee_public_key,
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
        let mut streams = lock(&self.client_streams)?;
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
        let mut streams = lock(&self.exit_streams)?;
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
        let streams = lock(&self.client_streams)?;
        let stream = authorize_client_stream(&streams, key, from)?;
        Ok(stream.service.clone())
    }

    fn client_inbound_sender(
        &self,
        key: TcpStreamKey,
        from: Did,
    ) -> Result<mpsc::Sender<TcpInbound>> {
        let streams = lock(&self.client_streams)?;
        let stream = authorize_client_stream(&streams, key, from)?;
        Ok(stream.tx.clone())
    }

    fn verify_client_payload(
        &self,
        key: TcpStreamKey,
        from: Did,
        payload: OnionAuthenticatedPayload,
    ) -> Result<OnionCircuitPayload> {
        let (service, expected_exit, return_id) = {
            let streams = lock(&self.client_streams)?;
            let stream = authorize_client_stream(&streams, key, from)?;
            (
                stream.service.clone(),
                stream.expected_exit.clone(),
                stream.return_id,
            )
        };
        let verified =
            payload.into_verified_payload(return_id, &expected_exit, self.signer.network_id())?;
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
        let mut streams = lock(&self.client_streams)?;
        let stream = authorize_client_stream_mut(&mut streams, key, from)?;
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
        let mut streams = lock(&self.client_streams)?;
        let stream = authorize_client_stream_mut(&mut streams, key, from)?;
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
        let mut streams = lock(&self.exit_streams)?;
        let stream = authorize_exit_stream(&mut streams, key, from)?;
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
        let mut streams = lock(&self.exit_streams)?;
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
    return_delegatee_public_key: PublicKey<33>,
    client: OnionClientReturn,
    expected_forward_peer: Did,
    service: OnionServiceName,
    target: String,
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

fn authorize_client_stream(
    streams: &HashMap<TcpStreamKey, ClientStream>,
    key: TcpStreamKey,
    actual: Did,
) -> Result<&ClientStream> {
    let stream = streams
        .get(&key)
        .ok_or(Error::OnionRouteError(OnionRouteError::UnknownTcpStream))?;
    if stream.expected_return_peer != actual {
        return Err(Error::OnionRouteError(
            OnionRouteError::UnexpectedTcpReturnPeer {
                expected: stream.expected_return_peer,
                actual,
            },
        ));
    }
    Ok(stream)
}

fn authorize_client_stream_mut(
    streams: &mut HashMap<TcpStreamKey, ClientStream>,
    key: TcpStreamKey,
    actual: Did,
) -> Result<&mut ClientStream> {
    let stream = streams
        .get_mut(&key)
        .ok_or(Error::OnionRouteError(OnionRouteError::UnknownTcpStream))?;
    if stream.expected_return_peer != actual {
        return Err(Error::OnionRouteError(
            OnionRouteError::UnexpectedTcpReturnPeer {
                expected: stream.expected_return_peer,
                actual,
            },
        ));
    }
    Ok(stream)
}

fn authorize_exit_stream(
    streams: &mut HashMap<TcpStreamKey, ExitStream>,
    key: TcpStreamKey,
    actual: Did,
) -> Result<&mut ExitStream> {
    let stream = streams
        .get_mut(&key)
        .ok_or(Error::OnionRouteError(OnionRouteError::UnknownTcpStream))?;
    if stream.expected_forward_peer != actual {
        return Err(Error::OnionRouteError(
            OnionRouteError::UnexpectedTcpForwardPeer {
                expected: stream.expected_forward_peer,
                actual,
            },
        ));
    }
    Ok(stream)
}

#[cfg(test)]
mod tests;
