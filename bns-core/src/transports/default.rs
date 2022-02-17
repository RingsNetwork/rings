use crate::channels::default::TkChannel;
use crate::encoder::{decode, encode};
use crate::signing::SigMsg;
use crate::types::channel::Channel;
use crate::types::channel::Events;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;
use crate::types::ice_transport::IceTrickleScheme;
use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use secp256k1::SecretKey;
use serde::Deserialize;
use serde::Serialize;
use serde_json;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use tokio::sync::Mutex;
use tokio::time::Duration;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::math_rand_alpha;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

#[derive(Clone)]
pub struct DefaultTransport {
    pub connection: Arc<Mutex<Option<Arc<RTCPeerConnection>>>>,
    pub pending_candidates: Arc<Mutex<Vec<RTCIceCandidate>>>,
    pub channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    pub signaler: Arc<SyncMutex<TkChannel>>,
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
        }
    }

    fn signaler(&self) -> Arc<SyncMutex<TkChannel>> {
        Arc::clone(&self.signaler)
    }

    async fn start(&mut self, stun: String) -> Result<()> {
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec![stun.to_owned()],
                ..Default::default()
            }],
            ice_candidate_pool_size: 100,
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
        self.on_ice_candidate(self.on_ice_candidate_callback().await)
            .await?;
        self.on_peer_connection_state_change(self.on_peer_connection_state_change_callback().await)
            .await?;
        self.on_data_channel(self.on_data_channel_callback().await)
            .await?;
        self.on_open(self.on_open_callback().await).await?;
        self.on_message(self.on_message_callback().await).await?;
        Ok(())
    }

    async fn get_peer_connection(&self) -> Option<Arc<RTCPeerConnection>> {
        self.connection.lock().await.clone()
    }

    async fn get_pending_candidates(&self) -> Vec<RTCIceCandidate> {
        self.pending_candidates.lock().await.to_vec()
    }

    async fn get_answer(&self) -> Result<RTCSessionDescription> {
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                // wait gather candidates
                let mut gather_complete = peer_connection.gathering_complete_promise().await;
                let answer = peer_connection.create_answer(None).await?;
                self.set_local_description(answer.to_owned()).await?;
                let _ = gather_complete.recv().await;
                Ok(answer)
            }
            None => Err(anyhow!("cannot get answer")),
        }
    }

    async fn get_answer_str(&self) -> Result<String> {
        Ok(self.get_answer().await?.sdp)
    }

    async fn get_offer(&self) -> Result<RTCSessionDescription> {
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                // wait gather candidates
                let mut gather_complete = peer_connection.gathering_complete_promise().await;
                match peer_connection.create_offer(None).await {
                    Ok(offer) => {
                        self.set_local_description(offer.to_owned()).await?;
                        let _ = gather_complete.recv().await;
                        Ok(offer)
                    }
                    Err(e) => {
                        log::error!("{}", e);
                        Err(anyhow!(e))
                    }
                }
            }
            None => Err(anyhow!("cannot get offer")),
        }
    }

    async fn get_offer_str(&self) -> Result<String> {
        Ok(self.get_offer().await?.sdp)
    }

    async fn get_data_channel(&self) -> Option<Arc<RTCDataChannel>> {
        self.channel.lock().await.clone()
    }

    async fn add_ice_candidate(&self, candidate: String) -> Result<()> {
        match self.get_peer_connection().await {
            Some(peer_connection) => peer_connection
                .add_ice_candidate(RTCIceCandidateInit {
                    candidate,
                    ..Default::default()
                })
                .await
                .map_err(|e| anyhow!(e)),
            None => Err(anyhow!("cannot add ice candidate")),
        }
    }

    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<RTCSessionDescription> + Send,
    {
        match self.get_peer_connection().await {
            Some(peer_connection) => peer_connection
                .set_local_description(desc.into())
                .await
                .map_err(|e| anyhow!(e)),
            None => Err(anyhow!("cannot get local description")),
        }
    }

    async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<RTCSessionDescription> + Send,
    {
        match self.get_peer_connection().await {
            Some(peer_connection) => peer_connection
                .set_remote_description(desc.into())
                .await
                .map_err(|e| anyhow!(e)),
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
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                peer_connection.on_data_channel(f).await;
            }
            None => panic!("Connection Failed."),
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
                ch.on_message(f).await;
            }
            None => panic!("Connection Failed."),
        }
        Ok(())
    }

    async fn on_open(
        &self,
        f: Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + Sync>,
    ) -> Result<()> {
        match self.get_data_channel().await {
            Some(ch) => {
                ch.on_open(f).await;
            }
            None => panic!("Connection Failed."),
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
                    Err(e) => Err(anyhow!(e)),
                }
            }
            None => Err(anyhow!("cannot get data channel")),
        }
    }
}

#[async_trait]
impl IceTransportCallback<TkChannel> for DefaultTransport {
    async fn on_ice_candidate_callback(
        &self,
    ) -> Box<
        dyn FnMut(
                Option<<Self as IceTransport<TkChannel>>::Candidate>,
            ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
            + Send
            + Sync,
    > {
        let peer_connection = Arc::downgrade(&self.connection.lock().await.clone().unwrap());
        let pending_candidates = Arc::clone(&self.pending_candidates);

        box move |c: Option<<Self as IceTransport<TkChannel>>::Candidate>| {
            let peer_connection = peer_connection.clone();
            let pending_candidates = Arc::clone(&pending_candidates);
            Box::pin(async move {
                if let Some(candidate) = c {
                    if let Some(peer_connection) = peer_connection.upgrade() {
                        let desc = peer_connection.remote_description().await;
                        if desc.is_none() {
                            let mut candidates = pending_candidates.lock().await;
                            candidates.push(candidate.clone());
                            println!("Candidates Number: {:?}", candidates.len());
                        }
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
            Box::pin(async move {
                log::debug!("Connect State changed to {:?}", s);
            })
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
            let recv = Arc::clone(&d);
            Box::pin(async move {
                d.on_open(Box::new(move || {
                    Box::pin(async move {
                        let mut result = Result::<usize>::Ok(0);
                        while result.is_ok() {
                            let timeout = tokio::time::sleep(Duration::from_secs(5));
                            tokio::pin!(timeout);
                            tokio::select! {
                                _ = timeout.as_mut() =>{
                                    let message = math_rand_alpha(15);
                                    println!("Sending '{}'", message);
                                    result = recv.send_text(message).await.map_err(Into::into);
                                }
                            };
                        }
                    })
                }))
                .await;

                d.on_message(Box::new(move |msg: DataChannelMessage| {
                    let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
                    println!("Message from DataChannel: '{}'", msg_str);
                    Box::pin(async move {})
                }))
                .await;
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
            println!("Message from DataChannel: '{}'", msg_str);
            Box::pin(async move {})
        }
    }

    async fn on_open_callback(
        &self,
    ) -> Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + Sync> {
        let channel = self.get_data_channel().await.unwrap();
        box move || {
            let channel = Arc::clone(&channel);
            Box::pin(async move {
                let channel = Arc::clone(&channel);
                let mut result = Result::<usize>::Ok(0);
                while result.is_ok() {
                    let timeout = tokio::time::sleep(Duration::from_secs(5));
                    tokio::pin!(timeout);
                    tokio::select! {
                        _ = timeout.as_mut() =>{
                            let message = math_rand_alpha(15);
                            println!("Sending '{}'", message);
                            result = channel.send_text(message).await.map_err(Into::into);
                        }
                    };
                }
            })
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct TricklePayload {
    pub sdp: String,
    pub candidates: Vec<RTCIceCandidateInit>,
}

#[async_trait]
impl IceTrickleScheme<TkChannel> for DefaultTransport {
    // https://datatracker.ietf.org/doc/html/rfc5245
    // 1. Send (SdpOffer, IceCandidates) to remote
    // 2. Recv (SdpAnswer, IceCandidate) From Remote
    // 3. Set (SdpAnser)

    type SdpType = RTCSdpType;

    async fn get_handshake_info(&self, key: SecretKey, kind: RTCSdpType) -> Result<String> {
        log::trace!("prepareing handshake info {:?}", kind);
        let sdp = match kind {
            RTCSdpType::Answer => self.get_answer().await?,
            RTCSdpType::Offer => self.get_offer().await?,
            kind => {
                let mut sdp = self.get_offer().await?;
                sdp.sdp_type = kind;
                sdp
            }
        };
        let local_candidates_json = join_all(
            self.get_pending_candidates()
                .await
                .iter()
                .map(async move |c| c.clone().to_json().await.unwrap()),
        )
        .await;
        let data = TricklePayload {
            sdp: serde_json::to_string(&sdp).unwrap(),
            candidates: local_candidates_json,
        };
        log::trace!("prepared hanshake info :{:?}", data);
        let resp = SigMsg::new(data, key)?;
        Ok(encode(serde_json::to_string(&resp)?))
    }

    async fn register_remote_info(&self, data: String) -> anyhow::Result<()> {
        let data: SigMsg<TricklePayload> =
            serde_json::from_slice(decode(data).map_err(|e| anyhow!(e))?.as_bytes())
                .map_err(|e| anyhow!(e))?;
        log::trace!("register remote info: {:?}", data);

        match data.verify() {
            Ok(true) => {
                let sdp = serde_json::from_str::<RTCSessionDescription>(&data.data.sdp)?;
                log::trace!("setting remote sdp: {:?}", sdp);
                self.set_remote_description(sdp).await?;
                log::trace!("setting remote candidate");
                for c in data.data.candidates {
                    log::trace!("add candiates: {:?}", c);
                    self.add_ice_candidate(c.candidate.clone()).await.unwrap();
                }
                Ok(())
            }
            _ => {
                log::error!("cannot verify message sig");
                return Err(anyhow!("failed on verify message sigature"));
            }
        }
    }
}
