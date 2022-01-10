use anyhow::Result;
use core::future::Future;
use core::pin::Pin;
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
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
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
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
    conn: Arc<Mutex<Option<Arc<RTCPeerConnection>>>>,
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
            conn: Arc::new(Mutex::new(Some(Arc::clone(&peer_connection)))),
            remote_session_description: None,
            local_session_description: None,
            connection_state: Arc::new(Mutex::new(connection_state)),
            ice_candidates: Arc::new(Mutex::new(vec![])),
            candidate_tx: tx,
            candidate_rx: rx,
        })
    }

    pub async fn add_ice_candidate(&self, candidate: String) {
        if let Err(e) = self
            .conn
            .lock()
            .await
            .clone()
            .unwrap()
            .add_ice_candidate(RTCIceCandidateInit {
                candidate,
                ..Default::default()
            })
            .await
        {
            panic!("{}", e);
        }
    }

    pub async fn set_remote_description(&self, sdp: impl Into<RTCSessionDescription>) {
        let desc = sdp.into();
        if let Err(e) = self
            .conn
            .lock()
            .await
            .clone()
            .unwrap()
            .set_remote_description(desc)
            .await
        {
            panic!("LocalDescription Failed");
        }
    }

    pub async fn set_local_description(&self, sdp: impl Into<RTCSessionDescription>) {
        let desc = sdp.into();
        if let Err(e) = self
            .conn
            .lock()
            .await
            .clone()
            .unwrap()
            .set_local_description(desc)
            .await
        {
            panic!("LocalDescription Failed");
        }
    }

    pub async fn on_ice_candidate(
        &mut self,
        addr: String,
        f: Arc<
            Box<
                dyn Fn(
                        String,
                        RTCIceCandidate,
                    )
                        -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>
                    + Send
                    + Sync,
            >,
        >,
    ) {
        let addr = Arc::new(addr);
        let pc = Arc::downgrade(&self.conn.lock().await.clone().unwrap());
        let pending_candidates2 = Arc::clone(&self.ice_candidates);
        let candidate_tx = self.candidate_tx.clone();
        let f2 = Arc::clone(&f);
        self.conn
            .lock()
            .await
            .clone()
            .unwrap()
            .on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                println!("on ice candidate {:?}", c);
                let pc2 = pc.clone();
                let pending_candidates3 = Arc::clone(&pending_candidates2);
                let addr2 = addr.clone();
                let candidate_tx2 = candidate_tx.clone();
                let f3 = Arc::clone(&f2);
                Box::pin(async move {
                    if let Some(c) = c {
                        if let Some(pc) = pc2.upgrade() {
                            let desc = pc.remote_description().await;
                            if desc.is_none() {
                                let mut cs = pending_candidates3.lock().await;
                                cs.push(c.clone());
                                candidate_tx2.send(c.address);
                            } else if let Err(e) = f3(addr2.to_string(), c).await {
                                panic!("On Ice Candidate F Failed");
                            }
                        }
                    }
                })
            }))
            .await;
    }

    pub async fn on_data_channel(&self, f: OnDataChannelHdlrFn) -> Result<()> {
        self.conn
            .lock()
            .await
            .clone()
            .unwrap()
            .on_data_channel(f)
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod test {}
