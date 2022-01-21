use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Mutex;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;

use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use crate::types::ice_transport::IceTransport;
use webrtc::peer_connection::RTCPeerConnection;

#[derive(Clone)]
pub struct DefaultTransport {
    pub connection: Arc<Mutex<Option<Arc<RTCPeerConnection>>>>,
    pub pending_candidates: Arc<Mutex<Vec<RTCIceCandidate>>>,
}

#[async_trait]
impl IceTransport for DefaultTransport {
    type Connection = RTCPeerConnection;
    type Candidate = RTCIceCandidate;
    type Sdp = RTCSessionDescription;
    type Channel = RTCDataChannel;
    type ConnectionState = RTCPeerConnectionState;

    async fn get_peer_connection(&self) -> Option<Arc<RTCPeerConnection>> {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => Some(peer_connection),
            None => panic!("Connection Failed."),
        }
    }

    async fn get_pending_candidates(&self) -> Arc<Mutex<Vec<RTCIceCandidate>>> {
        Arc::clone(&self.pending_candidates)
    }

    async fn get_answer(&self) -> Result<RTCSessionDescription> {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => peer_connection
                .create_answer(None)
                .await
                .map_err(|e| anyhow!(e)),
            None => panic!("Connection Failed."),
        }
    }

    async fn get_offer(&self) -> Result<RTCSessionDescription> {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => peer_connection
                .create_offer(None)
                .await
                .map_err(|e| anyhow!(e)),
            None => panic!("Connection Failed."),
        }
    }

    async fn get_data_channel(
        &self,
        label: &str,
    ) -> Result<Arc<Mutex<Arc<RTCDataChannel>>>> {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => {
                let data_channel = peer_connection
                    .create_data_channel(label, None)
                    .await
                    .map_err(|e| anyhow!(e))?;
                Ok(Arc::new(Mutex::new(data_channel)))
            }
            None => {
                panic!("Connection Failed.")
            }
        }
    }

    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<RTCSessionDescription> + std::marker::Send,
    {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => peer_connection
                .set_local_description(desc.into())
                .await
                .map_err(|e| anyhow!(e)),
            None => {
                panic!("Connection Failed.")
            }
        }
    }

    async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<RTCSessionDescription> + std::marker::Send,
    {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => peer_connection
                .set_remote_description(desc.into())
                .await
                .map_err(|e| anyhow!(e)),
            None => {
                panic!("Connection Failed.")
            }
        }
    }

    async fn on_ice_candidate(
        &self,
        f: Box<
            dyn FnMut(Option<RTCIceCandidate>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => {
                peer_connection.on_ice_candidate(f).await;
            }
            None => {
                panic!("Connection Failed.");
            }
        }
        Ok(())
    }

    async fn on_peer_connection_state_change(
        &self,
        f: Box<
            dyn FnMut(RTCPeerConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => {
                peer_connection.on_peer_connection_state_change(f).await;
            }
            None => panic!("Connection Failed."),
        }
        Ok(())
    }

    async fn on_data_channel(
        &self,
        f: Box<
            dyn FnMut(Arc<RTCDataChannel>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => {
                peer_connection.on_data_channel(f).await;
            }
            None => panic!("Connection Failed."),
        }
        Ok(())
    }
}

impl DefaultTransport {
    pub async fn new() -> Result<Self> {
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let api = APIBuilder::new().build();
        let peer_connection = Arc::new(api.new_peer_connection(config).await?);
        Ok(Self {
            connection: Arc::new(Mutex::new(Some(Arc::clone(&peer_connection)))),
            pending_candidates: Arc::new(Mutex::new(vec![])),
        })
    }
}
