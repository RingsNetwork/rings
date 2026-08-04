use std::sync::Arc;

use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

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
use crate::onion::OnionExitFailure;
use crate::onion::OnionServiceName;

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
            scope: &self.scope,
            signer: &self.runtime.session_sk,
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
