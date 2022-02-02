use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use tokio::sync::Mutex;
use tokio::time::Duration;
use webrtc::api::APIBuilder;

use crate::channels::default::TkChannel;
use crate::types::channel::Channel;
use crate::types::channel::Events;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::math_rand_alpha;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

#[derive(Clone)]
pub struct DefaultTransport {
    pub connection: Arc<Mutex<Option<Arc<RTCPeerConnection>>>>,
    pub pending_candidates: Arc<Mutex<Vec<RTCIceCandidate>>>,
    pub channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    pub signaler: Arc<SyncMutex<TkChannel>>,
    pub offer: Option<RTCSessionDescription>,
}

#[async_trait]
impl IceTransport<TkChannel> for DefaultTransport {
    type Connection = RTCPeerConnection;
    type Candidate = RTCIceCandidate;
    type Sdp = RTCSessionDescription;
    type Channel = RTCDataChannel;
    type ConnectionState = RTCPeerConnectionState;
    type Msg = DataChannelMessage;

    fn new(ch: Arc<SyncMutex<TkChannel>>) -> Self {
        Self {
            connection: Arc::new(Mutex::new(None)),
            pending_candidates: Arc::new(Mutex::new(vec![])),
            channel: Arc::new(Mutex::new(None)),
            signaler: Arc::clone(&ch),
            offer: None,
        }
    }

    fn signaler(&self) -> Arc<SyncMutex<TkChannel>> {
        Arc::clone(&self.signaler)
    }

    async fn start(&mut self, stun_addr: String) -> Result<()> {
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec![stun_addr.to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let api = APIBuilder::new().build();
        match api.new_peer_connection(config).await {
            Ok(c) => {
                let mut conn = self.connection.lock().await;
                *conn = Some(Arc::new(c));
                Ok(())
            }
            Err(e) => Err(anyhow!(e)),
        }?;
        self.setup_channel("bns").await?;
        self.setup_offer().await
    }

    async fn get_peer_connection(&self) -> Option<Arc<RTCPeerConnection>> {
        self.connection.lock().await.clone()
    }

    async fn get_pending_candidates(&self) -> Vec<RTCIceCandidate> {
        self.pending_candidates.lock().await.clone()
    }

    async fn get_answer(&self) -> Result<RTCSessionDescription> {
        match self.get_peer_connection().await {
            Some(peer_connection) => peer_connection
                .create_answer(None)
                .await
                .map_err(|e| anyhow!(e)),
            None => Err(anyhow!("cannot get answer")),
        }
    }

    fn get_offer(&self) -> Result<RTCSessionDescription> {
        match &self.offer {
            Some(o) => Ok(o.clone()),
            None => Err(anyhow!("cannot get offer")),
        }
    }

    fn get_offer_str(&self) -> Result<String> {
        self.get_offer().map(|o| o.sdp)
    }

    async fn get_data_channel(&self) -> Option<Arc<RTCDataChannel>> {
        self.channel.lock().await.clone()
    }

    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<RTCSessionDescription> + Send,
    {
        let offer = desc.into();
        log::info!("try set_local_descrition as: {:?}", offer.clone());

        match self.get_peer_connection().await {
            Some(peer_connection) => {
                match peer_connection
                    .set_local_description(offer)
                    .await {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            log::error!("failed on set local description");
                            Err(anyhow!(e))
                        }
                    }
            },
            None => Err(anyhow!("cannot get local description")),
        }
    }

    async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<RTCSessionDescription> + Send,
    {
        let answer = desc.into();
        log::info!("try set_remote_descrition as: {:?}", answer.clone());
        match self.get_peer_connection().await {
            Some(peer_connection) => peer_connection
                .set_remote_description(answer)
                .await
                .map_err(|e| {
                    log::error!("failed on set remote description");
                    anyhow!(e)
                }),
            None => Err(anyhow!("connection is not setup")),
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
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                peer_connection.on_ice_candidate(f).await;
                Ok(())
            }
            None => Err(anyhow!("connection is not setup")),
        }
    }

    async fn on_peer_connection_state_change(
        &self,
        f: Box<
            dyn FnMut(RTCPeerConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                peer_connection.on_peer_connection_state_change(f).await;
            }
            None => {
                log::error!("cannot get connection");
            }
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
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                log::info!("register data channel callback");
                peer_connection.on_data_channel(f).await;
            }
            None => {
                log::error!("cannot get connection");
            },
        }
        Ok(())
    }

    async fn on_message(
        &self,
        f: Box<
            dyn FnMut(DataChannelMessage) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        match self.get_data_channel().await {
            Some(ch) => {
                log::info!("setting on message callback");
                ch.on_message(f).await;
            }
            None => {
                log::error!("cannot get data channel");
            },
        }
        Ok(())
    }
}

impl DefaultTransport {
    pub async fn setup_channel(&mut self, name: &str) -> Result<()> {
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                let channel = peer_connection.create_data_channel(name, None).await;
                match channel {
                    Ok(ch) => {
                        let mut channel = self.channel.lock().await;
                        *channel = Some(ch);
                        Ok(())
                    }
                    Err(e) => {
                        log::error!("{}: Failed on setup channel", e);
                        Err(anyhow!(e))
                    },
                }
            }
            None => {
                log::error!("cannot get peer connection");
                Err(anyhow!("cannot get peer connection"))
            }
        }
    }

    pub async fn setup_offer(&mut self) -> Result<()> {
        // setup offer and set it to local description
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                // Create channel that is blocked until ICE Gathering is complete
                // ref: https://github.com/webrtc-rs/examples/blob/5a0e2861c66a45fca93aadf9e70a5b045b26dc9e/examples/data-channels-create/data-channels-create.rs#L159
                let mut gather_complete = peer_connection.gathering_complete_promise().await;
                match peer_connection.create_offer(None).await {
                    Ok(offer) => {
                        self.offer = Some(offer.clone());
                        self.set_local_description(offer).await?;
                        let _ = gather_complete.recv().await;
                        Ok(())
                    },
                    Err(e) => {
                        log::error!("{}", e);
                        Err(anyhow!(e))
                    },
                }
            }
            None => {
                log::error!("setup_offer:: Cannot create offer");
                Err(anyhow!("cannot get offer"))
            }
        }
    }
}

#[async_trait]
impl IceTransportCallback<TkChannel> for DefaultTransport {
    async fn setup_callback(&self) -> Result<()> {
        self.on_ice_candidate(self.on_ice_candidate_callback().await)
            .await?;
        self.on_peer_connection_state_change(self.on_peer_connection_state_change_callback().await)
            .await?;
        self.on_data_channel(self.on_data_channel_callback().await)
            .await?;
        self.on_message(self.on_message_callback().await).await?;
        Ok(())
    }

    async fn on_ice_candidate_callback(
        &self,
    ) -> Box<
        dyn FnMut(
                Option<<Self as IceTransport<TkChannel>>::Candidate>,
            ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
            + Send
            + Sync,
    > {
        let peer_connection = self.get_peer_connection().await.unwrap();
        let pending_candidates = self.get_pending_candidates().await;

        box move |c: Option<<Self as IceTransport<TkChannel>>::Candidate>| {
            let peer_connection = peer_connection.to_owned();
            let pending_candidates = pending_candidates.to_owned();
            Box::pin(async move {
                if let Some(candidate) = c {
                    log::info!("start answer candidate: {:?}", candidate);
                    let desc = peer_connection.remote_description().await;
                    if desc.is_none() {
                        let mut candidates = pending_candidates;
                        candidates.push(candidate.clone());
                    } else {
                        log::info!("desc existed");
                    }
                }
            })
        }
    }

    async fn on_peer_connection_state_change_callback(
        &self,
    ) -> Box<
        dyn FnMut(RTCPeerConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
            + Send
            + Sync,
    > {
        let sender = self.signaler();
        box move |s: RTCPeerConnectionState| {
            let sender = Arc::clone(&sender);
            if s == RTCPeerConnectionState::Failed {
                let _ = sender.lock().unwrap().send(Events::ConnectFailed);
            }
            log::info!("peer connection state changes {:?}", s);
            Box::pin(async move {})
        }
    }

    async fn on_data_channel_callback(
        &self,
    ) -> Box<
        dyn FnMut(Arc<RTCDataChannel>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
            + Send
            + Sync,
    > {
        box move |d: Arc<RTCDataChannel>| {
            let d = Arc::clone(&d);
            Box::pin(async move {
                log::info!("created data channel");
                let mut result = Result::<usize>::Ok(0);
                while result.is_ok() {
                    let timeout = tokio::time::sleep(Duration::from_secs(5));
                    tokio::pin!(timeout);
                    tokio::select! {
                        _ = timeout.as_mut() =>{
                            let message = math_rand_alpha(15);
                            println!("Sending '{}'", message);
                            result = d.send_text(message).await.map_err(Into::into);
                        }
                    };
                }
            })
        }
    }

    async fn on_message_callback(
        &self,
    ) -> Box<
        dyn FnMut(DataChannelMessage) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
            + Send
            + Sync,
    > {
        box move |msg: DataChannelMessage| {
            let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
            log::info!("Message from DataChannel: '{}'", msg_str);
            Box::pin(async move {})
        }
    }
}
