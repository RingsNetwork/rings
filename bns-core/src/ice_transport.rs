use anyhow::anyhow;
use anyhow::Result;

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
use webrtc::peer_connection::offer_answer_options::RTCAnswerOptions;
use webrtc::peer_connection::offer_answer_options::RTCOfferOptions;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use webrtc::peer_connection::RTCPeerConnection;

#[derive(Clone)]
pub struct IceTransport {
    pub connection: Arc<Mutex<Option<Arc<RTCPeerConnection>>>>,
    pub pending_candidates: Arc<Mutex<Vec<RTCIceCandidate>>>,
}

impl IceTransport {
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
        Ok(IceTransport {
            connection: Arc::new(Mutex::new(Some(Arc::clone(&peer_connection)))),
            pending_candidates: Arc::new(Mutex::new(vec![])),
        })
    }

    pub async fn get_peer_connection(&self) -> Option<Arc<RTCPeerConnection>> {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => Some(peer_connection),
            None => panic!("Connection Failed."),
        }
    }

    pub async fn get_pending_candidates(&self) -> Arc<Mutex<Vec<RTCIceCandidate>>> {
        Arc::clone(&self.pending_candidates)
    }

    pub async fn get_answer(
        &self,
        options: Option<RTCAnswerOptions>,
    ) -> Result<RTCSessionDescription> {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => peer_connection
                .create_answer(options)
                .await
                .map_err(|e| anyhow!(e)),
            None => panic!("Connection Failed."),
        }
    }

    pub async fn get_offer(
        &self,
        options: Option<RTCOfferOptions>,
    ) -> Result<RTCSessionDescription> {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => peer_connection
                .create_offer(options)
                .await
                .map_err(|e| anyhow!(e)),
            None => panic!("Connection Failed."),
        }
    }

    pub async fn get_data_channel(
        &self,
        label: &str,
        options: Option<RTCDataChannelInit>,
    ) -> Result<Arc<Mutex<Arc<RTCDataChannel>>>> {
        match self.connection.lock().await.clone() {
            Some(peer_connection) => {
                let data_channel = peer_connection
                    .create_data_channel(label, options)
                    .await
                    .map_err(|e| anyhow!(e))?;
                Ok(Arc::new(Mutex::new(data_channel)))
            }
            None => {
                panic!("Connection Failed.")
            }
        }
    }

    pub async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<RTCSessionDescription>,
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

    pub async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<RTCSessionDescription>,
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

    pub async fn on_ice_candidate(
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

    pub async fn on_peer_connection_state_change(
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
}
