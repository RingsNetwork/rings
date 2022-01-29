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

use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::math_rand_alpha;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;
use crate::types::channel::Channel;
use crate::types::channel::Events;
use crate::channels::default::TkChannel;

#[derive(Clone)]
pub struct DefaultTransport {
    pub connection: Arc<Mutex<Option<Arc<RTCPeerConnection>>>>,
    pub pending_candidates: Arc<Mutex<Vec<RTCIceCandidate>>>,
    pub channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    pub signaler: Arc<SyncMutex<TkChannel>>
}

#[async_trait(?Send)]
impl IceTransport<TkChannel>for DefaultTransport {
    type Connection = RTCPeerConnection;
    type Candidate = RTCIceCandidate;
    type Sdp = RTCSessionDescription;
    type Channel = RTCDataChannel;
    type ConnectionState = RTCPeerConnectionState;
    type Msg = DataChannelMessage;

    fn new(sender: TkChannel) -> Self {
        Self {
            connection: Arc::new(Mutex::new(None)),
            pending_candidates: Arc::new(Mutex::new(vec![])),
            channel: Arc::new(Mutex::new(None)),
            signaler: Arc::new(SyncMutex::new(sender))
        }
    }

    fn signaler(&self) -> Arc<SyncMutex<TkChannel>> {
        return Arc::clone(&self.signaler);
    }

    async fn start(&mut self) -> Result<()> {
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
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
        self.setup_channel(&"bns".to_owned()).await
    }

    async fn get_peer_connection(&self) -> Option<Arc<RTCPeerConnection>> {
        return self.connection.lock().await.clone();
    }

    async fn get_pending_candidates(&self) -> Vec<RTCIceCandidate> {
        return self.pending_candidates.lock().await.clone();
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

    async fn get_offer(&self) -> Result<RTCSessionDescription> {
        match self.get_peer_connection().await {
            Some(peer_connection) => peer_connection
                .create_offer(None)
                .await
                .map_err(|e| anyhow!(e)),
            None => Err(anyhow!("cannot get offer")),
        }
    }

    async fn get_data_channel(&self) -> Option<Arc<RTCDataChannel>> {
        self.channel.lock().await.clone()
    }

    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<RTCSessionDescription>,
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
        T: Into<RTCSessionDescription>,
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

#[async_trait(?Send)]
impl IceTransportCallback<TkChannel> for DefaultTransport {

    async fn setup_callback(&self) -> Result<()> {
        self.on_ice_candidate(self.on_ice_candidate_callback().await).await?;
        self.on_peer_connection_state_change(self.on_peer_connection_state_change_callback().await).await?;
        self.on_data_channel(self.on_data_channel_callback().await).await?;
        self.on_message(self.on_message_callback().await).await?;
        Ok(())
    }

    async fn on_ice_candidate_callback(&self) -> Box<
            dyn FnMut(Option<<Self as IceTransport<TkChannel>>::Candidate>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
            + Send
            + Sync> {
        let peer_connection = Arc::downgrade(&self.get_peer_connection().await.unwrap());
        let pending_candidates = self.get_pending_candidates().await;

        box move |c: Option<<Self as IceTransport<TkChannel>>::Candidate>| {
            let peer_connection = peer_connection.to_owned();
            let pending_candidates = pending_candidates.to_owned();
            Box::pin(async move {
                if let Some(candidate) = c {
                    if let Some(peer_connection) = peer_connection.upgrade() {
                        let desc = peer_connection.remote_description().await;
                        if desc.is_none() {
                            let mut candidates = pending_candidates;
                            println!("start answer candidate: {:?}", candidate);
                            candidates.push(candidate.clone());
                        }
                    }
                }
            })
        }
    }

    async fn on_peer_connection_state_change_callback(&self) ->  Box<
            dyn FnMut(RTCPeerConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        > {
        let sender = self.signaler().lock().unwrap().sender();
        box move |s: RTCPeerConnectionState| {
            let sender = Arc::clone(&sender);
            if s == RTCPeerConnectionState::Failed {
                let _ = sender.send(Events::ConnectFailed);
            }
            Box::pin(async move {

            })
        }
    }

    async fn on_data_channel_callback(&self) -> Box<
            dyn FnMut(Arc<RTCDataChannel>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        > {
        box move |d: Arc<RTCDataChannel>| {
            let d =  Arc::clone(&d);
            Box::pin(async move {
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

    async fn on_message_callback(&self) -> Box<
            dyn FnMut(DataChannelMessage) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        > {
        box move |msg: DataChannelMessage| {
            let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
            println!("Message from DataChannel: '{}'", msg_str);
            Box::pin(async move {
            })
        }
    }
}
