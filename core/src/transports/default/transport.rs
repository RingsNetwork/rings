use std::sync::Arc;

use async_lock::RwLock as AsyncRwLock;
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::lock::Mutex as FuturesMutex;
use serde_json;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gathering_state::RTCIceGatheringState;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::channels::Channel as AcChannel;
use crate::chunk::Chunk;
use crate::chunk::ChunkList;
use crate::chunk::ChunkManager;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::consts::TRANSPORT_MTU;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::transports::helper::Promise;
use crate::types::channel::Channel;
use crate::types::channel::TransportEvent;
use crate::types::ice_transport::HandshakeInfo;
use crate::types::ice_transport::IceCandidate;
use crate::types::ice_transport::IceCandidateGathering;
use crate::types::ice_transport::IceServer;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;
use crate::types::ice_transport::IceTransportInterface;
use crate::types::ice_transport::IceTrickleScheme;

type EventSender = <AcChannel<TransportEvent> as Channel<TransportEvent>>::Sender;

/// DefaultTransport use for node.
#[derive(Clone)]
pub struct DefaultTransport {
    /// an unique identity
    pub id: uuid::Uuid,
    /// webrtc'RTCPeerConnection
    connection: Arc<FuturesMutex<Option<Arc<RTCPeerConnection>>>>,
    /// ice candidates will be noticed when connecting
    pending_candidates: Arc<FuturesMutex<Vec<RTCIceCandidate>>>,
    /// ice protocol message communication channel
    data_channel: Arc<FuturesMutex<Option<Arc<RTCDataChannel>>>>,
    /// channel contains `Sender` and `Receiver` with specific `Event`
    event_sender: EventSender,
    remote_did: Arc<AsyncRwLock<Option<Did>>>,
    chunk_list: Arc<FuturesMutex<ChunkList<TRANSPORT_MTU>>>,
}

impl PartialEq for DefaultTransport {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
    }
}

impl Drop for DefaultTransport {
    fn drop(&mut self) {
        tracing::debug!("transport dropped: {}", self.id);
    }
}

#[async_trait]
impl IceTransport for DefaultTransport {
    type Connection = RTCPeerConnection;
    type Candidate = RTCIceCandidate;
    type Sdp = RTCSessionDescription;
    type DataChannel = RTCDataChannel;

    async fn get_peer_connection(&self) -> Option<Arc<RTCPeerConnection>> {
        self.connection.lock().await.clone()
    }

    async fn get_pending_candidates(&self) -> Vec<RTCIceCandidate> {
        self.pending_candidates.lock().await.to_vec()
    }

    async fn get_data_channel(&self) -> Option<Arc<RTCDataChannel>> {
        self.data_channel.lock().await.clone()
    }

    /// get a RTCConnection offer, use to connect other nodes.
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
                        tracing::error!("{}", e);
                        Err(Error::RTCPeerConnectionCreateOfferFailed(e))
                    }
                }
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    /// get a RTCConnection answer, relay `offer` message and make connection.
    async fn get_answer(&self) -> Result<RTCSessionDescription> {
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                let mut gather_complete = peer_connection.gathering_complete_promise().await;
                match peer_connection.create_answer(None).await {
                    Ok(answer) => {
                        self.set_local_description(answer.to_owned()).await?;
                        let _ = gather_complete.recv().await;
                        Ok(answer)
                    }
                    Err(e) => {
                        tracing::error!("{}", e);
                        Err(Error::RTCPeerConnectionCreateAnswerFailed(e))
                    }
                }
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }
}

#[async_trait]
impl IceTransportInterface<TransportEvent, AcChannel<TransportEvent>> for DefaultTransport {
    type IceConnectionState = RTCIceConnectionState;

    fn new(event_sender: EventSender) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            connection: Arc::new(FuturesMutex::new(None)),
            pending_candidates: Arc::new(FuturesMutex::new(vec![])),
            data_channel: Arc::new(FuturesMutex::new(None)),
            event_sender,
            remote_did: Arc::new(AsyncRwLock::new(None)),
            chunk_list: Default::default(),
        }
    }

    async fn start(
        &mut self,
        ice_server: Vec<IceServer>,
        external_ip: Option<String>,
    ) -> Result<&Self> {
        let config = RTCConfiguration {
            ice_servers: ice_server.iter().map(|x| x.clone().into()).collect(),
            ice_candidate_pool_size: 100,
            ..Default::default()
        };
        let mut setting = SettingEngine::default();
        if let Some(addr) = external_ip {
            tracing::debug!("setting external ip {:?}", &addr);
            setting.set_nat_1to1_ips(vec![addr], RTCIceCandidateType::Host);
            setting.set_ice_multicast_dns_mode(MulticastDnsMode::QueryOnly);
        } else {
            // mDNS gathering cannot be used with 1:1 NAT IP mapping for host candidate
            setting.set_ice_multicast_dns_mode(MulticastDnsMode::QueryAndGather);
        }
        let api = APIBuilder::new().with_setting_engine(setting).build();
        match api.new_peer_connection(config).await {
            Ok(c) => {
                let mut conn = self.connection.lock().await;
                *conn = Some(Arc::new(c));
                Ok(())
            }
            Err(e) => Err(Error::RTCPeerConnectionCreateFailed(e)),
        }?;

        self.setup_channel("rings").await?;
        Ok(self)
    }

    async fn apply_callback(&self) -> Result<&Self> {
        let on_ice_candidate_callback = self.on_ice_candidate().await;
        let on_data_channel_callback = self.on_data_channel().await;
        let on_ice_connection_state_change_callback = self.on_ice_connection_state_change().await;
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                peer_connection.on_ice_candidate(on_ice_candidate_callback);
                peer_connection.on_data_channel(on_data_channel_callback);
                peer_connection
                    .on_ice_connection_state_change(on_ice_connection_state_change_callback);
                Ok(self)
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    async fn close(&self) -> Result<()> {
        if let Some(pc) = self.get_peer_connection().await {
            let result = pc
                .close()
                .await
                .map_err(Error::RTCPeerConnectionCloseFailed);

            tracing::info!("close transport {} result: {:?}", self.id, result);

            result?
        }

        Ok(())
    }

    async fn ice_connection_state(&self) -> Option<Self::IceConnectionState> {
        self.get_peer_connection()
            .await
            .map(|pc| pc.ice_connection_state())
    }

    async fn is_disconnected(&self) -> bool {
        matches!(
            self.ice_connection_state().await,
            Some(Self::IceConnectionState::Failed)
                | Some(Self::IceConnectionState::Disconnected)
                | Some(Self::IceConnectionState::Closed)
        )
    }

    async fn is_connected(&self) -> bool {
        self.ice_connection_state()
            .await
            .map(|s| s == RTCIceConnectionState::Connected)
            .unwrap_or(false)
    }

    async fn send_message(&self, msg: &Bytes) -> Result<()> {
        if msg.len() > TRANSPORT_MAX_SIZE {
            return Err(Error::MessageTooLarge);
        }

        let dc = self
            .get_data_channel()
            .await
            .ok_or(Error::RTCDataChannelNotReady)?;

        let chunks = ChunkList::<TRANSPORT_MTU>::from(msg);

        for c in chunks {
            tracing::debug!("Transport chunk data len: {}", c.data.len());
            let bytes = c.to_bincode()?;
            tracing::debug!("Transport chunk len: {}", bytes.len());

            let size = bytes.len();
            match dc.send(&bytes).await {
                Ok(s) => {
                    if !s == size {
                        return Err(Error::RTCDataChannelMessageIncomplete(s, size));
                    }
                }
                Err(e) => {
                    if dc.ready_state() != RTCDataChannelState::Open {
                        return Err(Error::RTCDataChannelStateNotOpen);
                    } else {
                        return Err(Error::RTCDataChannelSendTextFailed(e));
                    }
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl IceTransportCallback for DefaultTransport {
    type OnLocalCandidateHdlrFn =
        Box<dyn FnMut(Option<RTCIceCandidate>) -> BoxFuture<'static, ()> + Send + Sync>;
    type OnDataChannelHdlrFn =
        Box<dyn FnMut(Arc<RTCDataChannel>) -> BoxFuture<'static, ()> + Send + Sync>;
    type OnIceConnectionStateChangeHdlrFn =
        Box<(dyn FnMut(RTCIceConnectionState) -> BoxFuture<'static, ()> + Sync + Send + 'static)>;

    async fn on_ice_connection_state_change(&self) -> Self::OnIceConnectionStateChangeHdlrFn {
        let event_sender = self.event_sender.clone();
        let id = self.id;
        let remote_did = self.remote_did.clone();
        Box::new(move |cs: RTCIceConnectionState| {
            let event_sender = event_sender.clone();
            let id = id;
            let remote_did = remote_did.clone();
            Box::pin(async move {
                match cs {
                    RTCIceConnectionState::Connected => {
                        let remote_did = remote_did.read().await.unwrap();
                        if AcChannel::send(
                            &event_sender,
                            TransportEvent::RegisterTransport((remote_did, id)),
                        )
                        .await
                        .is_err()
                        {
                            tracing::error!("Failed when send RegisterTransport");
                        }
                    }
                    RTCIceConnectionState::Failed
                    | RTCIceConnectionState::Disconnected
                    | RTCIceConnectionState::Closed => {
                        let remote_did = remote_did.read().await.unwrap();
                        if AcChannel::send(
                            &event_sender,
                            TransportEvent::ConnectClosed((remote_did, id)),
                        )
                        .await
                        .is_err()
                        {
                            tracing::error!("Failed when send RegisterTransport");
                        }
                    }
                    _ => {
                        tracing::debug!("IceTransport state change {:?}", cs);
                    }
                }
            })
        })
    }

    async fn on_ice_candidate(&self) -> Self::OnLocalCandidateHdlrFn {
        let pending_candidates = Arc::clone(&self.pending_candidates);
        let peer_connection = self.get_peer_connection().await;

        Box::new(move |c: Option<RTCIceCandidate>| {
            let pending_candidates = Arc::clone(&pending_candidates);
            let peer_connection = peer_connection.clone();
            Box::pin(async move {
                if let Some(candidate) = c {
                    if peer_connection.is_some() {
                        let mut candidates = pending_candidates.lock().await;
                        candidates.push(candidate.clone());
                    }
                }
            })
        })
    }

    async fn on_data_channel(&self) -> Self::OnDataChannelHdlrFn {
        let event_sender = self.event_sender.clone();
        let chunk_list = self.chunk_list.clone();

        Box::new(move |d: Arc<RTCDataChannel>| {
            let event_sender = event_sender.clone();
            let chunk_list = chunk_list.clone();
            Box::pin(async move {
                d.on_message(Box::new(move |msg: DataChannelMessage| {
                    tracing::debug!("Chunked message from DataChannel: '{:?}'", msg);
                    let event_sender = event_sender.clone();
                    let chunk_list = chunk_list.clone();
                    Box::pin(async move {
                        let mut chunk_list = chunk_list.lock().await;

                        let chunk_item = Chunk::from_bincode(&msg.data);
                        if chunk_item.is_err() {
                            tracing::error!("Failed to deserialize transport chunk item");
                            return;
                        }
                        let chunk_item = chunk_item.unwrap();

                        let data = chunk_list.handle(chunk_item);
                        if data.is_none() {
                            return;
                        }
                        let data = data.unwrap();
                        tracing::debug!("Complete message from DataChannel: '{:?}'", data);

                        if AcChannel::send(
                            &event_sender,
                            TransportEvent::DataChannelMessage(data.into()),
                        )
                        .await
                        .is_err()
                        {
                            tracing::error!("Failed on handle msg")
                        };
                    })
                }));
            })
        })
    }
}

#[async_trait]
impl IceCandidateGathering for DefaultTransport {
    async fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<()> {
        match self.get_peer_connection().await {
            Some(peer_connection) => peer_connection
                .add_ice_candidate(candidate.into())
                .await
                .map_err(Error::RTCPeerConnectionAddIceCandidateError),
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where T: Into<RTCSessionDescription> + Send {
        match self.get_peer_connection().await {
            Some(peer_connection) => peer_connection
                .set_local_description(desc.into())
                .await
                .map_err(Error::RTCPeerConnectionSetLocalDescFailed),
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where T: Into<RTCSessionDescription> + Send {
        match self.get_peer_connection().await {
            Some(peer_connection) => peer_connection
                .set_remote_description(desc.into())
                .await
                .map_err(Error::RTCPeerConnectionSetRemoteDescFailed),
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }
}

#[async_trait]
impl IceTrickleScheme for DefaultTransport {
    // https://datatracker.ietf.org/doc/html/rfc5245
    // 1. Send (SdpOffer, IceCandidates) to remote
    // 2. Recv (SdpAnswer, IceCandidate) From Remote

    type SdpType = RTCSdpType;

    async fn get_handshake_info(&self, kind: RTCSdpType) -> Result<HandshakeInfo> {
        tracing::trace!("prepareing handshake info {:?}", kind);
        let sdp = match kind {
            RTCSdpType::Answer => self.get_answer().await?,
            RTCSdpType::Offer => self.get_offer().await?,
            kind => {
                let mut sdp = self.get_offer().await?;
                sdp.sdp_type = kind;
                sdp
            }
        };
        let local_candidates_json = self
            .pending_candidates
            .lock()
            .await
            .iter()
            .map(|c| c.clone().to_json().unwrap().into())
            .collect::<Vec<_>>();
        if local_candidates_json.is_empty() {
            return Err(Error::FailedOnGatherLocalCandidate);
        }
        let data = HandshakeInfo {
            sdp: serde_json::to_string(&sdp).unwrap(),
            candidates: local_candidates_json,
        };
        tracing::trace!("prepared handshake info :{:?}", data);
        Ok(data)
    }

    async fn register_remote_info(&self, data: &HandshakeInfo, did: Did) -> Result<()> {
        tracing::trace!("register remote info: {:?}", data);

        let sdp =
            serde_json::from_str::<RTCSessionDescription>(&data.sdp).map_err(Error::Deserialize)?;

        tracing::trace!("setting remote sdp: {:?}", sdp);
        self.set_remote_description(sdp).await?;

        tracing::trace!("setting remote candidate");
        for c in &data.candidates {
            tracing::trace!("add candidates: {:?}", c);
            if self.add_ice_candidate(c.clone()).await.is_err() {
                tracing::warn!("failed on add add candidates: {:?}", c.clone());
            };
        }

        let mut remote_did = self.remote_did.write().await;
        *remote_did = Some(did);

        Ok(())
    }

    async fn wait_for_connected(&self) -> Result<()> {
        let promise = self.connect_success_promise().await?;
        promise.await
    }
}

impl DefaultTransport {
    pub async fn ice_gathering_state(&self) -> Option<RTCIceGatheringState> {
        self.get_peer_connection()
            .await
            .map(|pc| pc.ice_gathering_state())
    }

    pub async fn setup_channel(&mut self, name: &str) -> Result<()> {
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                let channel = peer_connection.create_data_channel(name, None).await;
                match channel {
                    Ok(ch) => {
                        let mut channel = self.data_channel.lock().await;
                        *channel = Some(ch);
                        Ok(())
                    }
                    Err(_) => Err(Error::RTCDataChannelNotReady),
                }
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    pub async fn wait_for_data_channel_open(&self) -> Result<()> {
        if self.is_disconnected().await {
            return Err(Error::RTCPeerConnectionNotEstablish);
        }

        match self.get_data_channel().await {
            Some(dc) => {
                if dc.ready_state() == RTCDataChannelState::Open {
                    Ok(())
                } else {
                    let promise = Promise::default();
                    let state = Arc::clone(&promise.state());
                    dc.on_open(Box::new(move || {
                        let state = Arc::clone(&state);
                        Box::pin(async move {
                            let mut s = state.lock().unwrap();
                            if let Some(w) = s.waker.take() {
                                s.completed = true;
                                s.succeeded = Some(true);
                                w.wake();
                            }
                        })
                    }));
                    promise.await
                }
            }
            None => {
                tracing::error!("{:?}", Error::RTCDataChannelNotReady);
                Err(Error::RTCDataChannelNotReady)
            }
        }
    }

    pub async fn connect_success_promise(&self) -> Result<Promise> {
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                let promise = Promise::default();
                let state = Arc::clone(&promise.state());
                let state_clone = Arc::clone(&state);
                peer_connection.on_peer_connection_state_change(Box::new(move |st| {
                    let state = Arc::clone(&state);
                    Box::pin(async move {
                        match st {
                            RTCPeerConnectionState::Connected => {
                                let mut s = state.lock().unwrap();
                                if let Some(w) = s.waker.take() {
                                    s.completed = true;
                                    s.succeeded = Some(true);
                                    w.wake();
                                }
                            }
                            RTCPeerConnectionState::Failed => {
                                let mut s = state.lock().unwrap();
                                if let Some(w) = s.waker.take() {
                                    s.completed = true;
                                    s.succeeded = Some(false);
                                    w.wake();
                                }
                            }
                            _ => {
                                tracing::trace!("Connect State changed to {:?}", st);
                            }
                        }
                    })
                }));
                let current_state = peer_connection.connection_state();
                if RTCPeerConnectionState::Connected == current_state {
                    let mut s = state_clone.lock().unwrap();
                    if let Some(w) = s.waker.take() {
                        s.completed = true;
                        s.succeeded = Some(true);
                        w.wake();
                    }
                };
                Ok(promise)
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::str::FromStr;

    use async_channel::Receiver;

    use super::DefaultTransport as Transport;
    use super::*;
    use crate::ecc::SecretKey;
    use crate::types::ice_transport::IceServer;

    async fn prepare_transport() -> Result<(Transport, Receiver<TransportEvent>)> {
        let ch = Arc::new(AcChannel::new());
        let mut trans = Transport::new(ch.sender());

        let stun = IceServer::from_str("stun://stun.l.google.com:19302").unwrap();
        trans
            .start(vec![stun], None)
            .await?
            .apply_callback()
            .await?;
        Ok((trans, ch.receiver()))
    }

    pub async fn establish_connection(
        transport1: &Transport,
        transport2: &Transport,
    ) -> Result<(Did, Did)> {
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport1.ice_gathering_state().await,
            Some(RTCIceGatheringState::New)
        );
        assert_eq!(
            transport2.ice_gathering_state().await,
            Some(RTCIceGatheringState::New)
        );

        // Generate key pairs for did register
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();

        // Peer 1 try to connect peer 2
        let handshake_info1 = transport1.get_handshake_info(RTCSdpType::Offer).await?;
        assert_eq!(
            transport1.ice_gathering_state().await,
            Some(RTCIceGatheringState::Complete)
        );
        assert_eq!(
            transport2.ice_gathering_state().await,
            Some(RTCIceGatheringState::New)
        );
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );

        // Peer 2 got offer then register
        transport2
            .register_remote_info(&handshake_info1, key1.address().into())
            .await?;

        assert_eq!(
            transport1.ice_gathering_state().await,
            Some(RTCIceGatheringState::Complete)
        );
        assert_eq!(
            transport2.ice_gathering_state().await,
            Some(RTCIceGatheringState::New)
        );
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );

        // Peer 2 create answer
        let handshake_info2 = transport2.get_handshake_info(RTCSdpType::Answer).await?;

        assert_eq!(
            transport1.ice_gathering_state().await,
            Some(RTCIceGatheringState::Complete)
        );
        assert_eq!(
            transport2.ice_gathering_state().await,
            Some(RTCIceGatheringState::Complete)
        );
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::Checking)
        );

        // Peer 1 got answer then register
        transport1
            .register_remote_info(&handshake_info2, key2.address().into())
            .await?;
        let promise_1 = transport1.connect_success_promise().await?;
        let promise_2 = transport2.connect_success_promise().await?;
        promise_1.await?;
        promise_2.await?;
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );

        Ok((key1.address().into(), key2.address().into()))
    }

    #[tokio::test]
    async fn test_ice_connection_establish() {
        let (transport1, receiver1) = prepare_transport().await.unwrap();
        let (transport2, receiver2) = prepare_transport().await.unwrap();

        let (did1, did2) = establish_connection(&transport1, &transport2)
            .await
            .unwrap();

        assert!(matches!(
            receiver1.recv().await.unwrap(),
            TransportEvent::RegisterTransport((did, _)) if did == did2
        ));
        assert!(matches!(
            receiver2.recv().await.unwrap(),
            TransportEvent::RegisterTransport((did, _)) if did == did1
        ));
    }

    #[tokio::test]
    async fn test_send_message() {
        let (transport1, receiver1) = prepare_transport().await.unwrap();
        let (transport2, receiver2) = prepare_transport().await.unwrap();

        let (did1, did2) = establish_connection(&transport1, &transport2)
            .await
            .unwrap();

        assert!(matches!(
            receiver1.recv().await.unwrap(),
            TransportEvent::RegisterTransport((did, _)) if did == did2
        ));
        assert!(matches!(
            receiver2.recv().await.unwrap(),
            TransportEvent::RegisterTransport((did, _)) if did == did1
        ));

        transport1.wait_for_data_channel_open().await.unwrap();
        transport2.wait_for_data_channel_open().await.unwrap();

        // Check send message
        transport1.send_message(&"hello1".into()).await.unwrap();
        assert!(matches!(
            receiver2.recv().await.unwrap(),
            TransportEvent::DataChannelMessage(msg) if msg == "hello1".as_bytes()
        ));
        transport2.send_message(&"hello2".into()).await.unwrap();
        assert!(matches!(
            receiver1.recv().await.unwrap(),
            TransportEvent::DataChannelMessage(msg) if msg == "hello2".as_bytes()
        ));

        // Check send long message
        let long_message1: Bytes = (0..TRANSPORT_MAX_SIZE - 1)
            .map(|_| rand::random::<u8>())
            .collect();
        assert_eq!(long_message1.len(), TRANSPORT_MAX_SIZE - 1);
        transport1.send_message(&long_message1).await.unwrap();
        assert!(matches!(
            receiver2.recv().await.unwrap(),
            TransportEvent::DataChannelMessage(msg) if msg == long_message1.to_vec()
        ));
        let long_message2: Bytes = (0..TRANSPORT_MAX_SIZE)
            .map(|_| rand::random::<u8>())
            .collect();
        assert_eq!(long_message2.len(), TRANSPORT_MAX_SIZE);
        transport2.send_message(&long_message2).await.unwrap();
        assert!(matches!(
            receiver1.recv().await.unwrap(),
            TransportEvent::DataChannelMessage(msg) if msg == long_message2.to_vec()
        ));

        // Check send over sized message
        let oversize_message: Bytes = (0..TRANSPORT_MAX_SIZE + 1)
            .map(|_| rand::random::<u8>())
            .collect();
        assert_eq!(oversize_message.len(), TRANSPORT_MAX_SIZE + 1);
        assert!(transport1.send_message(&oversize_message).await.is_err());
    }
}
