use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use super::*;

pub(super) fn spawn_client_stream(
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

pub(super) struct TcpBackwardRoute<'route> {
    pub(super) scope: &'route Scope,
    pub(super) signer: &'route SessionSk,
    pub(super) service: &'route OnionServiceName,
    pub(super) circuit_id: OnionCircuitId,
    pub(super) return_peer: Did,
    pub(super) client: OnionClientReturn,
}

impl TcpBackwardRoute<'_> {
    pub(super) async fn send(
        self,
        sequence: OnionBackwardSequence,
        payload: OnionTcpPayload,
    ) -> Result<()> {
        send_backward(
            self.scope,
            self.signer,
            self.circuit_id,
            self.return_peer,
            self.client,
            sequence,
            encode_tcp_payload(self.service, payload)?,
        )
        .await
    }
}
