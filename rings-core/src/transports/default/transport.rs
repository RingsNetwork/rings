use std::sync::Arc;

use async_lock::RwLock as AsyncRwLock;
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::join_all;
use futures::future::BoxFuture;
use futures::lock::Mutex as FuturesMutex;
use serde_json;
use web3::types::Address;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::channels::Channel as AcChannel;
use crate::ecc::PublicKey;
use crate::err::Error;
use crate::err::Result;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessagePayload;
use crate::session::SessionManager;
use crate::transports::helper::Promise;
use crate::transports::helper::TricklePayload;
use crate::types::channel::Channel;
use crate::types::channel::Event;
use crate::types::ice_transport::IceCandidate;
use crate::types::ice_transport::IceServer;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;
use crate::types::ice_transport::IceTrickleScheme;

type EventSender = <AcChannel<Event> as Channel<Event>>::Sender;

#[derive(Clone)]
pub struct DefaultTransport {
    pub id: uuid::Uuid,
    connection: Arc<FuturesMutex<Option<Arc<RTCPeerConnection>>>>,
    pending_candidates: Arc<FuturesMutex<Vec<RTCIceCandidate>>>,
    data_channel: Arc<FuturesMutex<Option<Arc<RTCDataChannel>>>>,
    event_sender: EventSender,
    public_key: Arc<AsyncRwLock<Option<PublicKey>>>,
}

impl PartialEq for DefaultTransport {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
    }
}

impl Drop for DefaultTransport {
    fn drop(&mut self) {
        log::debug!("transport dropped: {}", self.id);
    }
}

#[async_trait]
impl IceTransport<Event, AcChannel<Event>> for DefaultTransport {
    type Connection = RTCPeerConnection;
    type Candidate = RTCIceCandidate;
    type Sdp = RTCSessionDescription;
    type DataChannel = RTCDataChannel;
    type IceConnectionState = RTCIceConnectionState;
    type Msg = DataChannelMessage;

    fn new(event_sender: EventSender) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            connection: Arc::new(FuturesMutex::new(None)),
            pending_candidates: Arc::new(FuturesMutex::new(vec![])),
            data_channel: Arc::new(FuturesMutex::new(None)),
            public_key: Arc::new(AsyncRwLock::new(None)),
            event_sender,
        }
    }

    async fn start(&mut self, ice_server: &IceServer) -> Result<&Self> {
        let config = RTCConfiguration {
            ice_servers: vec![ice_server.clone().into()],
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
            Err(e) => Err(Error::RTCPeerConnectionCreateFailed(e)),
        }?;

        self.setup_channel("rings").await?;
        Ok(self)
    }

    async fn close(&self) -> Result<()> {
        if let Some(pc) = self.get_peer_connection().await {
            pc.close()
                .await
                .map_err(Error::RTCPeerConnectionCloseFailed)?;
        }

        Ok(())
    }

    async fn ice_connection_state(&self) -> Option<Self::IceConnectionState> {
        self.get_peer_connection()
            .await
            .map(|pc| pc.ice_connection_state())
    }

    async fn is_connected(&self) -> bool {
        self.ice_connection_state()
            .await
            .map(|s| s == RTCIceConnectionState::Connected)
            .unwrap_or(false)
    }

    async fn pubkey(&self) -> PublicKey {
        self.public_key.read().await.unwrap()
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
                let mut gather_complete = peer_connection.gathering_complete_promise().await;
                let answer = peer_connection
                    .create_answer(None)
                    .await
                    .map_err(Error::RTCPeerConnectionCreateAnswerFailed)?;
                self.set_local_description(answer.to_owned()).await?;
                let _ = gather_complete.recv().await;
                Ok(answer)
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
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
                        Err(Error::RTCPeerConnectionCreateOfferFailed(e))
                    }
                }
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    async fn get_offer_str(&self) -> Result<String> {
        Ok(self.get_offer().await?.sdp)
    }

    async fn get_data_channel(&self) -> Option<Arc<RTCDataChannel>> {
        self.data_channel.lock().await.clone()
    }

    async fn send_message(&self, msg: &[u8]) -> Result<()> {
        let size = msg.len();
        match self.get_data_channel().await {
            Some(cnn) => match cnn.send(&Bytes::from(msg.to_vec())).await {
                Ok(s) => {
                    if !s == size {
                        Err(Error::RTCDataChannelMessageIncomplete(s, size))
                    } else {
                        Ok(())
                    }
                }
                Err(e) => {
                    if cnn.ready_state() != RTCDataChannelState::Open {
                        Err(Error::RTCDataChannelStateNotOpen)
                    } else {
                        Err(Error::RTCDataChannelSendTextFailed(e))
                    }
                }
            },
            None => Err(Error::RTCDataChannelNotReady),
        }
    }

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

impl DefaultTransport {
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
}

#[async_trait]
impl IceTransportCallback<Event, AcChannel<Event>> for DefaultTransport {
    type OnLocalCandidateHdlrFn =
        Box<dyn FnMut(Option<Self::Candidate>) -> BoxFuture<'static, ()> + Send + Sync>;
    type OnDataChannelHdlrFn =
        Box<dyn FnMut(Arc<Self::DataChannel>) -> BoxFuture<'static, ()> + Send + Sync>;
    type OnIceConnectionStateChangeHdlrFn =
        Box<(dyn FnMut(RTCIceConnectionState) -> BoxFuture<'static, ()> + Sync + Send + 'static)>;

    async fn apply_callback(&self) -> Result<&Self> {
        let on_ice_candidate_callback = self.on_ice_candidate().await;
        let on_data_channel_callback = self.on_data_channel().await;
        let on_ice_connection_state_change_callback = self.on_ice_connection_state_change().await;
        match self.get_peer_connection().await {
            Some(peer_connection) => {
                peer_connection
                    .on_ice_candidate(on_ice_candidate_callback)
                    .await;
                peer_connection
                    .on_data_channel(on_data_channel_callback)
                    .await;
                peer_connection
                    .on_ice_connection_state_change(on_ice_connection_state_change_callback)
                    .await;
                Ok(self)
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    async fn on_ice_connection_state_change(&self) -> Self::OnIceConnectionStateChangeHdlrFn {
        let event_sender = self.event_sender.clone();
        let public_key = Arc::clone(&self.public_key);
        box move |cs: Self::IceConnectionState| {
            let event_sender = event_sender.clone();
            let public_key = Arc::clone(&public_key);
            Box::pin(async move {
                match cs {
                    Self::IceConnectionState::Connected => {
                        let local_address: Address = public_key.read().await.unwrap().address();
                        if event_sender
                            .send(Event::RegisterTransport(local_address))
                            .await
                            .is_err()
                        {
                            log::error!("Failed when send RegisterTransport");
                        }
                    }
                    Self::IceConnectionState::Failed => {
                        let local_address: Address = public_key.read().await.unwrap().address();
                        if event_sender
                            .send(Event::ConnectFailed(local_address))
                            .await
                            .is_err()
                        {
                            log::error!("Failed when send RegisterTransport");
                        }
                    }
                    _ => {
                        log::debug!("IceTransport state change {:?}", cs);
                    }
                }
            })
        }
    }

    async fn on_ice_candidate(&self) -> Self::OnLocalCandidateHdlrFn {
        let peer_connection = self.get_peer_connection().await;
        let pending_candidates = Arc::clone(&self.pending_candidates);

        box move |c: Option<<Self as IceTransport<Event, AcChannel<Event>>>::Candidate>| {
            let peer_connection = peer_connection.clone();
            let pending_candidates = Arc::clone(&pending_candidates);
            Box::pin(async move {
                if let Some(candidate) = c {
                    if let Some(peer_connection) = peer_connection {
                        let desc = peer_connection.remote_description().await;
                        if desc.is_none() {
                            let mut candidates = pending_candidates.lock().await;
                            candidates.push(candidate.clone());
                        }
                    }
                }
            })
        }
    }

    async fn on_data_channel(&self) -> Self::OnDataChannelHdlrFn {
        let event_sender = self.event_sender.clone();

        box move |d: Arc<RTCDataChannel>| {
            let event_sender = event_sender.clone();
            Box::pin(async move {
                d.on_message(Box::new(move |msg: DataChannelMessage| {
                    log::debug!("Message from DataChannel: '{:?}'", msg);
                    let event_sender = event_sender.clone();
                    Box::pin(async move {
                        if event_sender
                            .send(Event::DataChannelMessage(msg.data.to_vec()))
                            .await
                            .is_err()
                        {
                            log::error!("Failed on handle msg")
                        };
                    })
                }))
                .await;
            })
        }
    }
}

#[async_trait]
impl IceTrickleScheme<Event, AcChannel<Event>> for DefaultTransport {
    // https://datatracker.ietf.org/doc/html/rfc5245
    // 1. Send (SdpOffer, IceCandidates) to remote
    // 2. Recv (SdpAnswer, IceCandidate) From Remote

    type SdpType = RTCSdpType;

    async fn get_handshake_info(
        &self,
        session_manager: &SessionManager,
        kind: RTCSdpType,
    ) -> Result<Encoded> {
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
                .map(async move |c| c.clone().to_json().await.unwrap().into()),
        )
        .await;
        let data = TricklePayload {
            sdp: serde_json::to_string(&sdp).unwrap(),
            candidates: local_candidates_json,
        };
        log::trace!("prepared hanshake info :{:?}", data);
        let resp = MessagePayload::new_direct(
            data,
            session_manager,
            session_manager.authorizer()?.to_owned().into(), // This is a fake destination
        )?;
        Ok(resp.gzip(9)?.encode()?)
    }

    async fn register_remote_info(&self, data: Encoded) -> Result<Address> {
        let data: MessagePayload<TricklePayload> = data.decode()?;
        log::trace!("register remote info: {:?}", data);
        match data.verify() {
            true => {
                let sdp = serde_json::from_str::<RTCSessionDescription>(&data.data.sdp)
                    .map_err(Error::Deserialize)?;
                log::trace!("setting remote sdp: {:?}", sdp);
                self.set_remote_description(sdp).await?;
                log::trace!("setting remote candidate");
                for c in &data.data.candidates {
                    log::trace!("add candiates: {:?}", c);
                    self.add_ice_candidate(c.clone()).await?;
                }
                if let Ok(public_key) = data.origin_verification.session.authorizer_pubkey() {
                    let mut pk = self.public_key.write().await;
                    *pk = Some(public_key);
                };
                Ok(data.addr)
            }
            _ => {
                log::error!("cannot verify message sig");
                return Err(Error::VerifySignatureFailed);
            }
        }
    }

    async fn wait_for_connected(&self) -> Result<()> {
        let promise = self.connect_success_promise().await?;
        promise.await
    }
}

impl DefaultTransport {
    pub async fn wait_for_data_channel_open(&self) -> Result<()> {
        match self.get_data_channel().await {
            Some(dc) => {
                if dc.ready_state() == RTCDataChannelState::Open {
                    Ok(())
                } else {
                    let promise = Promise::default();
                    let state = Arc::clone(&promise.state());
                    dc.on_open(box move || {
                        let state = Arc::clone(&state);
                        Box::pin(async move {
                            let mut s = state.lock().unwrap();
                            if let Some(w) = s.waker.take() {
                                s.completed = true;
                                s.successed = Some(true);
                                w.wake();
                            }
                        })
                    })
                    .await;
                    promise.await
                }
            }
            None => {
                log::error!("{:?}", Error::RTCDataChannelNotReady);
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
                peer_connection
                    .on_peer_connection_state_change(box move |st| {
                        let state = Arc::clone(&state);
                        Box::pin(async move {
                            match st {
                                RTCPeerConnectionState::Connected => {
                                    let mut s = state.lock().unwrap();
                                    if let Some(w) = s.waker.take() {
                                        s.completed = true;
                                        s.successed = Some(true);
                                        w.wake();
                                    }
                                }
                                RTCPeerConnectionState::Failed => {
                                    let mut s = state.lock().unwrap();
                                    if let Some(w) = s.waker.take() {
                                        s.completed = true;
                                        s.successed = Some(false);
                                        w.wake();
                                    }
                                }
                                _ => {
                                    log::trace!("Connect State changed to {:?}", st);
                                }
                            }
                        })
                    })
                    .await;
                let current_state = peer_connection.connection_state();
                if RTCPeerConnectionState::Connected == current_state {
                    let mut s = state_clone.lock().unwrap();
                    if let Some(w) = s.waker.take() {
                        s.completed = true;
                        s.successed = Some(true);
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

    use super::DefaultTransport as Transport;
    use super::*;
    use crate::ecc::SecretKey;
    use crate::types::ice_transport::IceServer;

    async fn prepare_transport() -> Result<Transport> {
        let ch = Arc::new(AcChannel::new());
        let mut trans = Transport::new(ch.sender());

        let stun = IceServer::from_str("stun://stun.l.google.com:19302").unwrap();
        trans.start(&stun).await?.apply_callback().await?;
        Ok(trans)
    }

    pub async fn establish_connection(
        transport1: &Transport,
        transport2: &Transport,
    ) -> Result<()> {
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );

        // Generate key pairs for signing and verification
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();

        // Generate Session associated to Keys
        let sm1 = SessionManager::new_with_seckey(&key1)?;
        let sm2 = SessionManager::new_with_seckey(&key2)?;

        // Peer 1 try to connect peer 2
        let handshake_info1 = transport1
            .get_handshake_info(&sm1, RTCSdpType::Offer)
            .await?;
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );

        // Peer 2 got offer then register
        let addr1 = transport2.register_remote_info(handshake_info1).await?;
        assert_eq!(addr1, key1.address());
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );

        // Peer 2 create answer
        let handshake_info2 = transport2
            .get_handshake_info(&sm2, RTCSdpType::Answer)
            .await?;
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::Checking)
        );

        // Peer 1 got answer then register
        let addr2 = transport1.register_remote_info(handshake_info2).await?;
        assert_eq!(addr2, key2.address());
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

        Ok(())
    }

    #[tokio::test]
    async fn test_ice_connection_establish() -> Result<()> {
        let transport1 = prepare_transport().await?;
        let transport2 = prepare_transport().await?;

        establish_connection(&transport1, &transport2).await?;

        Ok(())
    }
}
