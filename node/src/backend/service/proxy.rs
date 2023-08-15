use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageType;
use crate::error::TunnelDefeat;
use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::prelude::uuid::Uuid;
use crate::prelude::Message;
use crate::prelude::PayloadSender;
use crate::prelude::Swarm;

pub type TunnelId = Uuid;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum TunnelMessage {
    TcpDial { tid: TunnelId, service: String },
    TcpClose { tid: TunnelId, reason: TunnelDefeat },
    TcpPackage { tid: TunnelId, body: Bytes },
}

pub struct Tunnel {
    tid: TunnelId,
    remote_stream_tx: Option<mpsc::Sender<Bytes>>,
    listener: Option<tokio::task::JoinHandle<()>>,
}

pub struct TunnelListener {
    tid: TunnelId,
    local_stream: TcpStream,
    remote_stream_tx: mpsc::Sender<Bytes>,
    remote_stream_rx: mpsc::Receiver<Bytes>,
    swarm: &'static Swarm,
    peer_did: Did,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        tracing::info!("Tunnel {} dropped", self.tid);
        if let Some(listener) = self.listener.take() {
            listener.abort();
        }
    }
}

impl Tunnel {
    pub fn new(tid: TunnelId) -> Self {
        Self {
            tid,
            remote_stream_tx: None,
            listener: None,
        }
    }

    pub async fn send(&self, bytes: Bytes) {
        if let Some(ref tx) = self.remote_stream_tx {
            let _ = tx.send(bytes).await;
        } else {
            tracing::error!("Tunnel {} remote stream tx is none", self.tid);
        }
    }

    pub async fn listen(&mut self, local_stream: TcpStream, swarm: &Swarm, peer_did: Did) {
        if self.listener.is_some() {
            return;
        }

        let mut listener = TunnelListener::new(self.tid, local_stream, swarm, peer_did).await;
        let remote_stream_tx = listener.remote_stream_tx.clone();
        let listener_handler = tokio::spawn(Box::pin(async move { listener.listen().await }));

        self.remote_stream_tx = Some(remote_stream_tx);
        self.listener = Some(listener_handler);
    }
}

impl TunnelListener {
    async fn new(
        tid: TunnelId,
        local_stream: TcpStream,
        swarm: &'static Swarm,
        peer_did: Did,
    ) -> Self {
        let (remote_stream_tx, remote_stream_rx) = mpsc::channel(1024);
        Self {
            tid,
            local_stream,
            remote_stream_tx,
            remote_stream_rx,
            swarm,
            peer_did,
        }
    }

    async fn listen(&mut self) {
        let (mut local_read, mut local_write) = self.local_stream.split();

        let listen_local = async {
            let mut defeat = TunnelDefeat::None;

            loop {
                let mut buf = [0u8; 30000];

                match local_read.read(&mut buf).await {
                    Err(e) => {
                        defeat = e.kind().into();
                        break;
                    }
                    Ok(n) if n == 0 => {
                        defeat = TunnelDefeat::ConnectionClosed;
                        break;
                    }
                    Ok(n) => {
                        let body = Bytes::copy_from_slice(&buf[..n]);
                        let message = TunnelMessage::TcpPackage {
                            tid: self.tid,
                            body,
                        };
                        let custom_msg = wrap_custom_message(&message);
                        if let Err(e) = self.swarm.send_message(custom_msg, self.peer_did).await {
                            tracing::error!("Send TcpPackage message failed: {e:?}");
                            defeat = TunnelDefeat::WebrtcDatachannelSendFailed;
                            break;
                        }
                    }
                }
            }

            defeat
        };

        let listen_remote = async {
            let mut defeat = TunnelDefeat::None;

            loop {
                if let Some(body) = self.remote_stream_rx.recv().await {
                    if let Err(e) = local_write.write_all(&body).await {
                        tracing::error!("Write to local stream failed: {e:?}");
                        defeat = e.kind().into();
                        break;
                    }
                }
            }

            defeat
        };

        tokio::select! {
            defeat = listen_local => {
                tracing::info!("Local stream closed: {defeat:?}");
                let message = TunnelMessage::TcpClose {
                    tid: self.tid,
                    reason: defeat,
                };
                let custom_msg = wrap_custom_message(&message);
                if let Err(e) =  self.swarm.send_message(custom_msg, self.peer_did).await {
                    tracing::error!("Send TcpClose message failed: {e:?}");
                }
            },
            defeat = listen_remote => {
                tracing::info!("Remote stream closed: {defeat:?}");
                let message = TunnelMessage::TcpClose {
                    tid: self.tid,
                    reason: defeat,
                };
                let custom_msg = wrap_custom_message(&message);
                let _ = self.swarm.send_message(message, self.peer_did).await;
            }
        }
    }
}

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

pub fn wrap_custom_message(message: &TunnelMessage) -> Message {
    let message_bytes = bincode::serialize(message).unwrap();

    let backend_msg =
        BackendMessage::from((MessageType::TunnelMessage.into(), message_bytes.as_slice()));

    let data = bincode::serialize(&backend_msg).unwrap();

    let mut new_bytes: Vec<u8> = Vec::with_capacity(data.len() + 1);
    new_bytes.push(0);
    new_bytes.extend_from_slice(&[0u8; 3]);
    new_bytes.extend_from_slice(&data);

    Message::custom(&new_bytes)
}
