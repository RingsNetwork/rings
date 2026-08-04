use rings_core::ecc::PublicKey;

use super::*;
use crate::onion::circuit::OnionBackwardPath;

struct ClientReturnPath {
    scope: Scope,
    path: OnionCircuitPath,
    client_return: OnionClientReturn,
}

#[async_trait::async_trait]
impl pump::TcpDuplexEffects for ClientReturnPath {
    async fn send(&mut self, payload: OnionTcpPayload) -> Result<()> {
        send_client_payload(&self.scope, &self.path, self.client_return, payload).await
    }

    fn remote_failed(&mut self, failure: &OnionExitFailure) {
        tracing::warn!("onion TCP client stream failed: {failure}");
    }
}

pub(super) fn spawn_client_stream(
    runtime: Arc<OnionTcpRuntime>,
    scope: Scope,
    key: TcpStreamKey,
    stream: TcpStream,
    path: OnionCircuitPath,
    client_return: OnionClientReturn,
    rx: mpsc::Receiver<TcpInbound>,
) {
    tokio::spawn(async move {
        let mut return_path = ClientReturnPath {
            scope,
            path,
            client_return,
        };
        pump::pump_tcp_duplex(stream, rx, &mut return_path).await;
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
    pub(super) return_session_public_key: PublicKey<33>,
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
            OnionBackwardPath::new(
                self.circuit_id,
                self.return_peer,
                self.return_session_public_key,
                self.client,
            ),
            sequence,
            encode_tcp_payload(self.service, payload)?,
        )
        .await
    }
}
