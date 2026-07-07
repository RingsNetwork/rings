use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::duplex::TcpDuplexState;
use super::inbound::TcpInbound;
use super::send_tcp_backward;
use super::OnionTcpPayload;
use super::OnionTcpRuntime;
use super::TcpStreamKey;
use super::TCP_BUF;
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
    client: OnionClientReturn,
    service: OnionServiceName,
}

impl ExitReturnPath {
    async fn send(&self, payload: OnionTcpPayload) -> crate::error::Result<()> {
        send_tcp_backward(
            &self.scope,
            &self.runtime.session_sk,
            &self.service,
            self.circuit_id,
            self.return_peer,
            self.client,
            payload,
        )
        .await
    }

    async fn record_bytes_or_reject(&self, bytes: usize) -> bool {
        let Ok(bytes) = u64::try_from(bytes) else {
            let _ = self
                .send(OnionTcpPayload::Error(OnionExitFailure::PermissionDenied))
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
                .send(OnionTcpPayload::Error(OnionExitFailure::PermissionDenied))
                .await;
        }
        !rejected
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
        client,
        service,
        stream,
        mut rx,
        lease,
    } = task;
    let return_path = ExitReturnPath {
        runtime: runtime.clone(),
        scope,
        circuit_id,
        return_peer,
        client,
        service,
    };
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
                        if return_path.send(OnionTcpPayload::Shutdown).await.is_err() {
                            break;
                        }
                        state.close_read();
                    }
                    Ok(n) => {
                        let bytes = read_chunk(&read_buf, n);
                        if !return_path.record_bytes_or_reject(bytes.len()).await {
                            break;
                        }
                        if return_path.send(OnionTcpPayload::Data { bytes }).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = return_path
                            .send(OnionTcpPayload::Error(OnionExitFailure::ReadTarget(format!(
                                "read onion TCP target: {error}"
                            ))))
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
                        if !return_path.record_bytes_or_reject(bytes.len()).await {
                            break;
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
                    Some(TcpInbound::Error(failure)) => {
                        tracing::warn!("onion TCP exit stream failed: {failure}");
                        state.observe_remote_terminal();
                        break;
                    }
                }
            }
        }
    }
    if state.should_announce_terminal() {
        let _ = return_path.send(OnionTcpPayload::Close).await;
    }
    runtime.remove_exit_stream(key);
    drop(lease);
}

fn read_chunk(read_buf: &[u8], n: usize) -> Bytes {
    // Pre: Tokio returns a byte count no larger than the buffer passed to `read`.
    // Post: the returned `Bytes` is exactly the observed prefix, or empty if an invalid
    // foreign implementation violates the `AsyncRead` contract.
    Bytes::copy_from_slice(read_buf.get(..n).unwrap_or_default())
}
