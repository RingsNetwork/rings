use anyhow::Result;




use std::sync::Arc;


use tokio::sync::mpsc::Sender;
use tokio::sync::Mutex;
use tokio::time::Duration;
use webrtc::api::APIBuilder;

use webrtc::data_channel::data_channel_message::DataChannelMessage;

use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;


use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::math_rand_alpha;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;




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

    pub async fn start_answer(&self, candidate_tx: Sender<RTCIceCandidate>) {
        let pc = Arc::downgrade(&self.connection.lock().await.clone().unwrap());
        let pending_candidates2 = Arc::clone(&self.pending_candidates);
        let candidate_tx2 = candidate_tx.clone();
        self.connection
            .lock()
            .await
            .clone()
            .unwrap()
            .on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                println!("start answer candidate option: {:?}", c);
                let pc2 = pc.clone();
                let pending_candidates3 = Arc::clone(&pending_candidates2);
                let candidate_tx3 = candidate_tx2.clone();
                Box::pin(async move {
                    if let Some(candidate) = c {
                        if let Some(pc) = pc2.upgrade() {
                            let desc = pc.remote_description().await;
                            if desc.is_none() {
                                let mut candidates = pending_candidates3.lock().await;
                                println!("start answer candidate: {:?}", candidate);
                                candidates.push(candidate.clone());
                                candidate_tx3.send(candidate).await;
                            }
                        }
                    }
                })
            }))
            .await;

        let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
        self.connection
            .lock()
            .await
            .clone()
            .unwrap()
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                // Failed to exit dial server
                if s == RTCPeerConnectionState::Failed {
                    let _ = done_tx.try_send(());
                }

                Box::pin(async {})
            }))
            .await;
        // server on data_channel open
        // or return data-channel object
        // demo should just ping pong
        self.connection
            .lock()
            .await.clone().unwrap()
            .on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
                let d_label = d.label().to_owned();
                let d_id = d.id();
                println!("New DataChannel {} {}", d_label, d_id);

                Box::pin(async move{
                    // Register channel opening handling
                    let d2 =  Arc::clone(&d);
                    let d_label2 = d_label.clone();
                    let d_id2 = d_id;
                    d.on_open(Box::new(move || {
                        println!("Data channel '{}'-'{}' open. Random messages will now be sent to any connected DataChannels every 5 seconds", d_label2, d_id2);
                        Box::pin(async move {
                            let mut result = Result::<usize>::Ok(0);
                            while result.is_ok() {
                                let timeout = tokio::time::sleep(Duration::from_secs(5));
                                tokio::pin!(timeout);

                                tokio::select! {
                                    _ = timeout.as_mut() =>{
                                        let message = math_rand_alpha(15);
                                        println!("Sending '{}'", message);
                                        result = d2.send_text(message).await.map_err(Into::into);
                                    }
                                };
                            }
                        })
                    })).await;

                    // Register text message handling
                    d.on_message(Box::new(move |msg: DataChannelMessage| {
                        let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
                        println!("Message from DataChannel '{}': '{}'", d_label, msg_str);
                        Box::pin(async{})
                    })).await;
                })
        })).await;

        tokio::select! {
            _ = done_rx.recv() => {
                println!("received done signal!");
            }
            _ = tokio::signal::ctrl_c() => {
                println!("");
            }
        };
    }
}
