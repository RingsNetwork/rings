use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::Receiver;
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

pub struct IceTransport {
    conn: Arc<RTCPeerConnection>,
    remote_session_description: Option<RTCSessionDescription>,
    local_session_description: Option<RTCSessionDescription>,
    connection_state: Arc<Mutex<RTCPeerConnectionState>>,
    ice_candidates: Arc<Mutex<Vec<RTCIceCandidate>>>,

    candidate_tx: Sender<String>,
    candidate_rx: Receiver<String>,
}

impl IceTransport {
    pub async fn new(buf_size: usize, urls: Vec<String>) -> Result<Self> {
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
        let (tx, mut rx) = channel(buf_size);

        Ok(IceTransport {
            conn: peer_connection,
            remote_session_description: None,
            local_session_description: None,
            connection_state: Arc::new(Mutex::new(connection_state)),
            ice_candidates: Arc::new(Mutex::new(vec![])),
            candidate_tx: tx,
            candidate_rx: rx,
        })
    }

    pub async fn on_data_channel(&mut self, f: OnDataChannelHdlrFn) -> Result<()> {
        self.conn
            .on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
                Box::pin(async {})
            }))
            .await;
        let mut locked_connection_state = self.connection_state.lock().await;
        (*locked_connection_state).clone_from(&self.conn.connection_state());
        self.conn.on_data_channel(f).await;
        Ok(())
    }

    pub async fn on_peer_state_change(
        &mut self,
        f: OnPeerConnectionStateChangeHdlrFn,
    ) -> Result<(), ()> {
        self.conn.on_peer_connection_state_change(f).await;
        Ok(())
    }

    pub async fn on_ice_candidate(&mut self) -> Result<()> {
        let pending_candidates = Arc::clone(&self.ice_candidates);
        let conn2 = Arc::downgrade(&self.conn);
        let send_tx = self.candidate_tx.clone();
        // add candidate to ice_canditdate
        self.conn
            .on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                let conn3 = conn2.clone();
                let send_tx2 = send_tx.clone();
                let pending_candidates2 = Arc::clone(&pending_candidates);
                Box::pin(async move {
                    if let Some(c) = c {
                        if let Some(pc) = conn3.upgrade() {
                            let desc = pc.remote_description().await;
                            let address = c.address.clone();
                            if desc.is_none() {
                                let mut cs = pending_candidates2.lock().await;
                                cs.push(c);
                                send_tx2.send(address);
                            }
                        }
                    }
                })
            }))
            .await;
        Ok(())
    }

    pub async fn set_remote_description(&mut self, desc: impl Into<RTCSessionDescription>) {
        let desc: RTCSessionDescription = desc.into();
        self.conn.set_remote_description(desc.clone()).await;
        self.remote_session_description = Some(desc.clone());
    }

    pub async fn set_local_description(&mut self, desc: impl Into<RTCSessionDescription>) {
        let desc: RTCSessionDescription = desc.into();
        self.conn.set_local_description(desc.clone()).await;
        self.local_session_description = Some(desc.clone());
    }

    pub async fn candidate(&mut self) -> Option<String> {
        self.candidate_rx.recv().await
    }
}

#[cfg(test)]
mod test {}
