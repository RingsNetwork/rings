use std::sync::Arc;
use std::time::Duration;

use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::inbound::TcpInbound;
use super::pump::pump_tcp_duplex;
use super::pump::TcpDuplexEffects;
use super::OnionTcpPayload;
use super::OnionTcpRuntime;
use super::TcpBackwardRoute;
use super::TcpStreamKey;
use crate::extension::ext::Scope;
use crate::onion::circuit::OnionCircuitId;
use crate::onion::circuit::OnionClientReturn;
use crate::onion::exit_accounting::OnionExitLease;
use crate::onion::target::resolve_public_target;
use crate::onion::OnionExitFailure;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitTarget;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionServiceName;

const TCP_OPEN_RESPONSE_QUANTUM_MS: u128 = 250;

pub(super) fn open_response_deadline(opened_at: Instant, now: Instant) -> Instant {
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

pub(super) fn admit_exit_target(
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

pub(super) async fn connect_exit_target(
    target: &OnionProxyTarget,
) -> std::result::Result<TcpStream, OnionExitFailure> {
    let authority = target.authority();
    let addresses = resolve_public_target(target).await.map_err(|error| {
        tracing::warn!(target = authority, %error, "rejected or failed to resolve onion TCP exit target");
        if matches!(error, crate::error::Error::NoPermission) {
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

pub(super) struct ExitStreamTask {
    pub(super) runtime: Arc<OnionTcpRuntime>,
    pub(super) scope: Scope,
    pub(super) key: TcpStreamKey,
    pub(super) circuit_id: OnionCircuitId,
    pub(super) return_peer: Did,
    pub(super) return_session_public_key: PublicKey<33>,
    pub(super) client: OnionClientReturn,
    pub(super) service: OnionServiceName,
    pub(super) stream: TcpStream,
    pub(super) rx: mpsc::Receiver<TcpInbound>,
    pub(super) lease: OnionExitLease,
}

struct ExitReturnPath {
    runtime: Arc<OnionTcpRuntime>,
    scope: Scope,
    circuit_id: OnionCircuitId,
    return_peer: Did,
    return_session_public_key: PublicKey<33>,
    client: OnionClientReturn,
    service: OnionServiceName,
}

impl ExitReturnPath {
    async fn send_payload(&self, payload: OnionTcpPayload) -> crate::error::Result<()> {
        let sequence = self.runtime.next_backward_sequence(TcpStreamKey {
            circuit_id: self.circuit_id,
        })?;
        TcpBackwardRoute {
            link_sender: &self.runtime.link_sender,
            scope: &self.scope,
            signer: self.runtime.message_signer(),
            service: &self.service,
            circuit_id: self.circuit_id,
            return_peer: self.return_peer,
            return_session_public_key: self.return_session_public_key,
            client: self.client,
        }
        .send(sequence, payload)
        .await
    }

    async fn record_bytes_or_reject(&self, bytes: usize) -> bool {
        let Ok(bytes) = u64::try_from(bytes) else {
            let _ = self
                .send_payload(OnionTcpPayload::Error(OnionExitFailure::PermissionDenied))
                .await;
            return false;
        };
        let rejected = self.runtime.exit_config.as_ref().is_some_and(|config| {
            self.runtime
                .record_exit_bytes(config.policy(), bytes)
                .is_err()
        });
        if rejected {
            let _ = self
                .send_payload(OnionTcpPayload::Error(OnionExitFailure::PermissionDenied))
                .await;
        }
        !rejected
    }
}

#[async_trait::async_trait]
impl TcpDuplexEffects for ExitReturnPath {
    async fn send(&mut self, payload: OnionTcpPayload) -> crate::error::Result<()> {
        self.send_payload(payload).await
    }

    async fn admit_bytes(&mut self, bytes: usize) -> bool {
        self.record_bytes_or_reject(bytes).await
    }

    async fn read_failed(&mut self, error: &std::io::Error) {
        tracing::warn!(%error, "failed to read onion TCP exit target");
        let _ = self
            .send_payload(OnionTcpPayload::Error(OnionExitFailure::ReadTarget))
            .await;
    }

    fn remote_failed(&mut self, failure: &OnionExitFailure) {
        tracing::warn!("onion TCP exit stream failed: {failure}");
    }
}

pub(super) fn spawn_exit_stream(task: ExitStreamTask) {
    tokio::spawn(run_exit_stream(task));
}

async fn run_exit_stream(task: ExitStreamTask) {
    let ExitStreamTask {
        runtime,
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
    } = task;
    let mut return_path = ExitReturnPath {
        runtime: runtime.clone(),
        scope,
        circuit_id,
        return_peer,
        return_session_public_key,
        client,
        service,
    };
    pump_tcp_duplex(stream, rx, &mut return_path).await;
    runtime.remove_exit_stream(key);
    drop(lease);
}
