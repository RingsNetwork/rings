use anyhow::Result;
use core::future::Future;
use core::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tokio::sync::Mutex;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::OnDataChannelHdlrFn;
use webrtc::peer_connection::RTCPeerConnection;

pub struct IceTransport {
    pub conn: Arc<Mutex<Option<Arc<RTCPeerConnection>>>>,
    pub remote_session_description: Option<RTCSessionDescription>,
    pub local_session_description: Option<RTCSessionDescription>,
    pub connection_state: Arc<Mutex<RTCPeerConnectionState>>,
    pub ice_candidates: Arc<Mutex<Vec<RTCIceCandidate>>>,

    pub candidate_tx: Sender<String>,
    pub candidate_rx: Receiver<String>,
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
        let (tx, rx) = channel(buf_size);

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
        if let Err(_) = self
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
        if let Err(_) = self
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
        callback: Arc<
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
        let conn = self.conn.lock().await.clone().unwrap();
        let candidates = Arc::clone(&self.ice_candidates);
        let candidate_tx = self.candidate_tx.clone();
        let cb = Arc::clone(&callback);
        self.conn
            .lock()
            .await
            .to_owned()
            .unwrap()
            .on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                println!("on ice candidate {:?}", c);
                let conn = conn.to_owned();
                let candidates = Arc::clone(&candidates);
                let addr = addr.to_owned();
                let candidate_tx2 = candidate_tx.clone();
                let cb = cb.to_owned();
                Box::pin(async move {
                    if let Some(c) = c {
                        let desc = conn.to_owned().remote_description().await;
                        if desc.is_none() {
                            let mut cs = candidates.lock().await;
                            cs.push(c.to_owned());
                            if let Err(_) = candidate_tx2.send(c.address).await {
                                panic!("Failed on send");
                            };
                        } else if let Err(_) = cb(addr.to_string(), c).await {
                            panic!("On Ice Candidate F Failed");
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
