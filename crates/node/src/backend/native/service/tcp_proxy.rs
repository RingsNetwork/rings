#![warn(missing_docs)]
//! Module tcp_proxy provide implementation of TCP/IP based services
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_rpc::method::Method;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::backend::types::BackendMessage;
use crate::backend::types::ServiceMessage;
use crate::backend::types::TunnelDefeat;
use crate::backend::types::TunnelId;
use crate::provider::Provider;
use rings_types::AsyncProvider;
/// Abstract Tcp Tunnel
pub struct Tunnel {
    tid: TunnelId,
    remote_stream_tx: Option<mpsc::Sender<Bytes>>,
    listener_cancel_token: Option<CancellationToken>,
    listener: Option<tokio::task::JoinHandle<()>>,
}

/// Listener of Tcp Tunnel, contains a mpsc channel
pub struct TunnelListener {
    tid: TunnelId,
    local_stream: TcpStream,
    remote_stream_tx: mpsc::Sender<Bytes>,
    remote_stream_rx: mpsc::Receiver<Bytes>,
    peer_did: Did,
    cancel_token: CancellationToken,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(cancel_token) = self.listener_cancel_token.take() {
            cancel_token.cancel();
        }

        if let Some(listener) = self.listener.take() {
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                listener.abort();
            });
        }

        tracing::info!("Tunnel {} dropped", self.tid);
    }
}

impl Tunnel {
    /// Create a new tunnel with a given tunnel Id
    pub fn new(tid: TunnelId) -> Self {
        Self {
            tid,
            remote_stream_tx: None,
            listener: None,
            listener_cancel_token: None,
        }
    }

    /// Send bytes to tunnel via channel
    pub async fn send(&self, bytes: Bytes) {
        if let Some(ref tx) = self.remote_stream_tx {
            let _ = tx.send(bytes).await;
        } else {
            tracing::error!("Tunnel {} remote stream tx is none", self.tid);
        }
    }

    /// Start listen a local stream, this function will spawn a thread which
    /// listening the inbound messages
    pub async fn listen(
        &mut self,
        provider: Arc<Provider>,
        local_stream: TcpStream,
        peer_did: Did,
    ) {
        if self.listener.is_some() {
            return;
        }
        let provider = provider.clone();
        let mut listener = TunnelListener::new(self.tid, local_stream, peer_did).await;
        let listener_cancel_token = listener.cancel_token();
        let remote_stream_tx = listener.remote_stream_tx.clone();
        let listener_handler =
            tokio::spawn(Box::pin(async move { listener.listen(provider).await }));

        self.remote_stream_tx = Some(remote_stream_tx);
        self.listener = Some(listener_handler);
        self.listener_cancel_token = Some(listener_cancel_token);
    }
}

impl TunnelListener {
    /// Create a new listener instance with TcpStream, tunnel id, and did of a target peer
    async fn new(tid: TunnelId, local_stream: TcpStream, peer_did: Did) -> Self {
        let (remote_stream_tx, remote_stream_rx) = mpsc::channel(1024);
        Self {
            tid,
            local_stream,
            remote_stream_tx,
            remote_stream_rx,
            peer_did,
            cancel_token: CancellationToken::new(),
        }
    }

    fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    async fn listen(&mut self, provider: Arc<Provider>) {
        let (mut local_read, mut local_write) = self.local_stream.split();

        let listen_local = async {
            loop {
                if self.cancel_token.is_cancelled() {
                    break TunnelDefeat::ConnectionClosed;
                }

                let mut buf = [0u8; 30000];
                match local_read.read(&mut buf).await {
                    Err(e) => {
                        break e.kind().into();
                    }
                    Ok(0) => {
                        break TunnelDefeat::ConnectionClosed;
                    }
                    Ok(n) => {
                        let body = Bytes::copy_from_slice(&buf[..n]);
                        let msg = ServiceMessage::TcpPackage {
                            tid: self.tid,
                            body,
                        };

                        let backend_message: BackendMessage = msg.into();
                        let params = backend_message
                            .into_send_backend_message_request(self.peer_did)
                            .unwrap();
                        if let Err(e) = provider.request(Method::SendBackendMessage, params).await {
                            tracing::error!("Send TcpPackage message failed: {e:?}");
                            break TunnelDefeat::WebrtcDatachannelSendFailed;
                        }
                    }
                }
            }
        };

        let listen_remote = async {
            loop {
                if self.cancel_token.is_cancelled() {
                    break TunnelDefeat::ConnectionClosed;
                }

                if let Some(body) = self.remote_stream_rx.recv().await {
                    if let Err(e) = local_write.write_all(&body).await {
                        tracing::error!("Write to local stream failed: {e:?}");
                        break e.kind().into();
                    }
                }
            }
        };

        tokio::select! {
            defeat = listen_local => {
                tracing::info!("Local stream closed: {defeat:?}");
                let msg = ServiceMessage::TcpClose {
                    tid: self.tid,
                    reason: defeat,
                };

                let backend_message: BackendMessage = msg.into();
                let params = backend_message.into_send_backend_message_request(self.peer_did).unwrap();
                if let Err(e) = provider.request(Method::SendBackendMessage, params).await {
                    tracing::error!("Send TcpClose message failed: {e:?}");
                }
            },
            defeat = listen_remote => {
                tracing::info!("Remote stream closed: {defeat:?}");
                let msg = ServiceMessage::TcpClose {
                    tid: self.tid,
                    reason: defeat,
                };

                let backend_message: BackendMessage = msg.into();
                let params = backend_message.into_send_backend_message_request(self.peer_did).unwrap();
                let _ = provider.request(Method::SendBackendMessage, params).await;
            }
        }
    }
}

/// This function handle a tcp request with timeout
pub async fn tcp_connect_with_timeout(
    addr: SocketAddr,
    request_timeout_s: u64,
) -> Result<TcpStream, TunnelDefeat> {
    let fut = tcp_connect(addr);
    match timeout(Duration::from_secs(request_timeout_s), fut).await {
        Ok(result) => result,
        Err(_) => Err(TunnelDefeat::ConnectionTimeout),
    }
}

async fn tcp_connect(addr: SocketAddr) -> Result<TcpStream, TunnelDefeat> {
    match TcpStream::connect(addr).await {
        Ok(o) => Ok(o),
        Err(e) => Err(e.kind().into()),
    }
}
