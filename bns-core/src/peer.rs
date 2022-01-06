use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::Mutex;
use webrtc::api::APIBuilder;
use webrtc::api::API;
use webrtc::data_channel::OnOpenHdlrFn;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_gatherer::OnLocalCandidateHdlrFn;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::math_rand_alpha;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::OnDataChannelHdlrFn;
use webrtc::peer_connection::OnPeerConnectionStateChangeHdlrFn;
use webrtc::peer_connection::OnSignalingStateChangeHdlrFn;
use webrtc::peer_connection::RTCPeerConnection;

#[derive(Clone)]
pub struct Peer {
    conn: Arc<RTCPeerConnection>,
    connection_state: RTCPeerConnectionState,
    ice_candidates: Arc<Mutex<Vec<RTCIceCandidate>>>,
}

impl Peer {
    pub async fn new(urls: Vec<String>) -> Result<Self> {
        let mut urls = urls;
        if urls.len() <= 0 {
            urls = vec!["stun:stun.l.google.com:19302".to_owned()];
        }

        let api = APIBuilder::new().build();
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: urls,
                ..Default::default()
            }],
            ..Default::default()
        };
        let peer_connection = Arc::new(api.new_peer_connection(config).await?);
        let connection_state = peer_connection.connection_state();
        Ok(Peer {
            conn: peer_connection,
            connection_state: connection_state,
            ice_candidates: Arc::new(Mutex::new(vec![])),
        })
    }

    pub async fn on_data_channel<T>(&mut self, f: OnDataChannelHdlrFn) -> Result<()>
    where
        T: Send,
    {
        self.conn
            .on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
                Box::pin(async {})
            }))
            .await;
        Ok(())
    }

    pub async fn on_ice_candidate(&self) -> Result<()> {
        let pending_candidates = Arc::clone(&self.ice_candidates);
        let conn = Arc::clone(&self.conn);
        self.conn
            .on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                let conn2 = conn.clone();
                Box::pin(async {})
            }))
            .await;
        Ok(())
    }
}
