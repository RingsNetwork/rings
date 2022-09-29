use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::prelude::rings_core::message::Message;
use crate::prelude::*;

#[derive(Deserialize, Serialize, Debug)]
pub struct BackendConfig {
    pub tcp_proxy: Option<TcpProxyConfig>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct TcpProxyConfig {
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum BackendMessage {
    TcpProxy(TcpProxyMessage),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum TcpProxyMessage {
    Write(Vec<u8>),
    Read(Vec<u8>),
}

pub struct Backend {
    tcp_proxy_port: Option<u16>,
}

impl Backend {
    pub async fn new(config: BackendConfig) -> Self {
        Self {
            tcp_proxy_port: config.tcp_proxy.map(|c| c.port),
        }
    }
}

#[async_trait]
impl MessageCallback for Backend {
    async fn custom_message(
        &self,
        handler: &MessageHandler,
        ctx: &MessagePayload<Message>,
        msg: &MaybeEncrypted<CustomMessage>,
    ) {
        let mut relay = ctx.relay.clone();
        relay.relay(relay.destination, None).unwrap();

        if let Ok(CustomMessage(raw_msg)) = handler.decrypt_msg(msg) {
            if let Ok(msg) = serde_json::from_slice(&raw_msg) {
                match msg {
                    BackendMessage::TcpProxy(msg) => match msg {
                        TcpProxyMessage::Write(msg) => {
                            if let Some(port) = self.tcp_proxy_port {
                                let mut conn = TcpStream::connect(format!("127.0.0.1:{}", port))
                                    .await
                                    .unwrap();

                                conn.write_all(&msg).await.unwrap();

                                // 100k
                                let mut buff: Vec<u8> = Vec::new();
                                conn.read_to_end(&mut buff).await.unwrap();

                                let resp = BackendMessage::TcpProxy(TcpProxyMessage::Read(buff));
                                let resp_bytes = serde_json::to_vec(&resp).unwrap();
                                let pubkey = ctx.origin_session_pubkey().unwrap();

                                handler
                                    .send_report_message(
                                        Message::custom(&resp_bytes, Some(pubkey)).unwrap(),
                                        ctx.tx_id,
                                        relay,
                                    )
                                    .await
                                    .unwrap();
                            }
                        }
                        TcpProxyMessage::Read(msg) => {
                            println!("TcpProxyMessage::Read: {:?}", msg);
                        }
                    },
                }
            }
        }
    }

    async fn builtin_message(&self, _handler: &MessageHandler, _ctx: &MessagePayload<Message>) {}
}
