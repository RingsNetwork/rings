use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use async_trait::async_trait;
use bytes::Bytes;
use js_sys::Uint8Array;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use web_sys::MessageEvent;
use web_sys::RtcConfiguration;
use web_sys::RtcDataChannel;
use web_sys::RtcDataChannelEvent;
use web_sys::RtcDataChannelState;
use web_sys::RtcIceCandidate;
use web_sys::RtcIceCandidateInit;
use web_sys::RtcIceConnectionState;
use web_sys::RtcIceGatheringState;
use web_sys::RtcPeerConnection;
use web_sys::RtcPeerConnectionIceEvent;
use web_sys::RtcSdpType;
use web_sys::RtcSessionDescription;
use web_sys::RtcSessionDescriptionInit;

use super::helper::RtcSessionDescriptionWrapper;
use crate::channels::Channel as CbChannel;
use crate::chunk::Chunk;
use crate::chunk::ChunkList;
use crate::chunk::ChunkManager;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::consts::TRANSPORT_MTU;
use crate::dht::Did;
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
use crate::types::ice_transport::IceCandidateGathering;
use crate::types::ice_transport::IceServer;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;
use crate::types::ice_transport::IceTransportInterface;
use crate::types::ice_transport::IceTrickleScheme;
use crate::utils::js_value;

type EventSender = <CbChannel<Event> as Channel<Event>>::Sender;

/// WasmTransport use for browser.
#[derive(Clone)]
pub struct WasmTransport {
    pub id: uuid::Uuid,
    connection: Option<Arc<RtcPeerConnection>>,
    pending_candidates: Arc<Mutex<Vec<RtcIceCandidate>>>,
    channel: Option<Arc<RtcDataChannel>>,
    event_sender: EventSender,
    public_key: Arc<RwLock<Option<PublicKey>>>,
    chunk_list: Arc<Mutex<ChunkList<TRANSPORT_MTU>>>,
}

impl PartialEq for WasmTransport {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
    }
}

impl Drop for WasmTransport {
    fn drop(&mut self) {
        tracing::trace!("[WASM] Transport dropped!");
        if let Some(conn) = &self.connection {
            conn.close();
        }
    }
}

#[async_trait(?Send)]
impl IceTransport for WasmTransport {
    type Connection = RtcPeerConnection;
    type Candidate = RtcIceCandidate;
    type Sdp = RtcSessionDescription;
    type DataChannel = RtcDataChannel;

    async fn get_peer_connection(&self) -> Option<Arc<RtcPeerConnection>> {
        self.connection.as_ref().map(Arc::clone)
    }

    async fn get_pending_candidates(&self) -> Vec<RtcIceCandidate> {
        self.pending_candidates.lock().unwrap().to_vec()
    }

    async fn get_answer(&self) -> Result<RtcSessionDescription> {
        match self.get_peer_connection().await {
            Some(c) => {
                let promise = c.create_answer();
                match JsFuture::from(promise).await {
                    Ok(answer) => {
                        self.set_local_description(RtcSessionDescriptionWrapper::from(
                            answer.to_owned(),
                        ))
                        .await?;
                        let promise = self.gather_complete_promise().await?;
                        promise.await?;
                        Ok(answer.into())
                    }
                    Err(e) => Err(Error::RTCPeerConnectionCreateAnswerFailed(format!(
                        "{:?}",
                        e
                    ))),
                }
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    async fn get_offer(&self) -> Result<RtcSessionDescription> {
        match self.get_peer_connection().await {
            Some(c) => {
                let promise = c.create_offer();
                match JsFuture::from(promise).await {
                    Ok(offer) => {
                        self.set_local_description(RtcSessionDescriptionWrapper::from(
                            offer.to_owned(),
                        ))
                        .await?;
                        let promise = self.gather_complete_promise().await?;
                        promise.await?;
                        Ok(offer.into())
                    }
                    Err(e) => Err(Error::RTCPeerConnectionCreateOfferFailed(format!(
                        "{:?}",
                        e
                    ))),
                }
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    async fn get_data_channel(&self) -> Option<Arc<RtcDataChannel>> {
        self.channel.as_ref().map(Arc::clone)
    }
}

#[async_trait(?Send)]
impl IceTransportInterface<Event, CbChannel<Event>> for WasmTransport {
    type IceConnectionState = RtcIceConnectionState;

    fn new(event_sender: EventSender) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            connection: None,
            pending_candidates: Arc::new(Mutex::new(vec![])),
            channel: None,
            public_key: Arc::new(RwLock::new(None)),
            event_sender,
            chunk_list: Default::default(),
        }
    }

    async fn start(
        &mut self,
        ice_server: Vec<IceServer>,
        _external_ip: Option<String>,
    ) -> Result<&Self> {
        let mut config = RtcConfiguration::new();
        let ice_servers: js_sys::Array = js_sys::Array::from_iter(
            ice_server
                .into_iter()
                .map(<IceServer as Into<JsValue>>::into),
        );
        config.ice_servers(&ice_servers.into());
        // hack here
        let r = js_sys::Reflect::set(
            &config,
            &JsValue::from("iceCandidatePoolSize"),
            &JsValue::from(10),
        );
        debug_assert!(
            r.is_ok(),
            "setting properties should never fail on our dictionary objects"
        );

        let conn = RtcPeerConnection::new_with_configuration(&config)
            .map_err(|e| Error::CreateConnectionError(format!("{:?}", e)))?;
        self.connection = Some(Arc::new(conn));
        self.setup_channel("rings").await;
        return Ok(self);
    }

    async fn apply_callback(&self) -> Result<&Self> {
        match &self.get_peer_connection().await {
            Some(c) => {
                let on_ice_candidate_callback = Closure::wrap(self.on_ice_candidate().await);
                let on_data_channel_callback = Closure::wrap(self.on_data_channel().await);
                let on_ice_connection_state_change_callback =
                    Closure::wrap(self.on_ice_connection_state_change().await);

                c.set_onicecandidate(Some(on_ice_candidate_callback.as_ref().unchecked_ref()));
                c.set_ondatachannel(Some(on_data_channel_callback.as_ref().unchecked_ref()));
                c.set_oniceconnectionstatechange(Some(
                    on_ice_connection_state_change_callback
                        .as_ref()
                        .unchecked_ref(),
                ));
                on_ice_candidate_callback.forget();
                on_data_channel_callback.forget();
                on_ice_connection_state_change_callback.forget();
                Ok(self)
            }
            None => {
                tracing::error!("cannot get connection");
                Err(Error::RTCPeerConnectionNotEstablish)
            }
        }
    }

    async fn close(&self) -> Result<()> {
        if let Some(pc) = self.get_peer_connection().await {
            pc.close();
            tracing::info!("close transport {}", self.id);
        }
        Ok(())
    }

    async fn pubkey(&self) -> PublicKey {
        self.public_key.read().unwrap().unwrap()
    }

    async fn ice_connection_state(&self) -> Option<Self::IceConnectionState> {
        self.get_peer_connection()
            .await
            .map(|pc| pc.ice_connection_state())
    }

    async fn is_connected(&self) -> bool {
        self.ice_connection_state()
            .await
            .map(|s| s == RtcIceConnectionState::Connected)
            .unwrap_or(false)
    }

    async fn is_disconnected(&self) -> bool {
        matches!(
            self.ice_connection_state().await,
            Some(Self::IceConnectionState::Failed)
                | Some(Self::IceConnectionState::Disconnected)
                | Some(Self::IceConnectionState::Closed)
        )
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
            let bytes = c.to_bincode()?;
            dc.send_with_u8_array(&bytes)
                .map_err(|e| Error::RTCDataChannelSendTextFailed(format!("{:?}", e)))?
        }

        Ok(())
    }
}

impl WasmTransport {
    pub async fn setup_channel(&mut self, name: &str) {
        if let Some(conn) = &self.connection {
            let channel = conn.create_data_channel(name);
            self.channel = Some(Arc::new(channel));
        }
    }
}

#[async_trait(?Send)]
impl IceTransportCallback for WasmTransport {
    type OnLocalCandidateHdlrFn = Box<dyn FnMut(RtcPeerConnectionIceEvent)>;
    type OnDataChannelHdlrFn = Box<dyn FnMut(RtcDataChannelEvent)>;
    type OnIceConnectionStateChangeHdlrFn = Box<dyn FnMut(web_sys::Event)>;

    async fn on_ice_connection_state_change(&self) -> Self::OnIceConnectionStateChangeHdlrFn {
        let event_sender = self.event_sender.clone();
        let peer_connection = self.get_peer_connection().await;
        let id = self.id;
        let public_key = Arc::clone(&self.public_key);
        Box::new(move |ev: web_sys::Event| {
            let mut peer_connection = peer_connection.clone();
            let event_sender = Arc::clone(&event_sender);
            let public_key = Arc::clone(&public_key);
            let id = id;

            // tracing::debug!("got state event {:?}", ev.type_());
            if ev.type_() == *"iceconnectionstatechange" {
                let peer_connection = peer_connection.take().unwrap();
                let ice_connection_state = peer_connection.ice_connection_state();
                tracing::debug!(
                    "got state event {:?}, {:?}",
                    ev.type_(),
                    ice_connection_state
                );
                spawn_local(async move {
                    let event_sender = Arc::clone(&event_sender);
                    match ice_connection_state {
                        RtcIceConnectionState::Connected => {
                            let local_did = (*public_key.read().unwrap()).unwrap().address().into();
                            if CbChannel::send(
                                &event_sender,
                                Event::RegisterTransport((local_did, id)),
                            )
                            .await
                            .is_err()
                            {
                                tracing::error!("Failed when send RegisterTransport");
                            }
                        }
                        RtcIceConnectionState::Failed
                        | RtcIceConnectionState::Disconnected
                        | RtcIceConnectionState::Closed => {
                            let local_did = (*public_key.read().unwrap()).unwrap().address().into();
                            if CbChannel::send(&event_sender, Event::ConnectClosed((local_did, id)))
                                .await
                                .is_err()
                            {
                                tracing::error!("Failed when send ConnectFailed");
                            }
                        }
                        _ => {
                            tracing::debug!("IceTransport state change {:?}", ice_connection_state);
                        }
                    }
                })
            }
        })
    }

    async fn on_ice_candidate(&self) -> Self::OnLocalCandidateHdlrFn {
        let peer_connection = self.get_peer_connection().await;
        let pending_candidates = Arc::clone(&self.pending_candidates);
        tracing::debug!("binding ice candidate callback");
        Box::new(move |ev: RtcPeerConnectionIceEvent| {
            tracing::info!("ice_Candidate {:?}", ev.candidate());
            let mut candidates = pending_candidates.lock().unwrap();
            let peer_connection = peer_connection.clone();
            if let Some(candidate) = ev.candidate() {
                if peer_connection.is_some() {
                    candidates.push(candidate);
                    println!("Candidates Number: {:?}", candidates.len());
                }
            }
        })
    }

    async fn on_data_channel(&self) -> Self::OnDataChannelHdlrFn {
        let event_sender = self.event_sender.clone();
        let chunk_list = self.chunk_list.clone();

        Box::new(move |ev: RtcDataChannelEvent| {
            tracing::debug!("channel open");
            let event_sender = event_sender.clone();
            let chunk_list = chunk_list.clone();
            let ch = ev.channel();
            let on_message_cb = Closure::wrap(
                (Box::new(move |ev: MessageEvent| {
                    let data = ev.data();
                    let event_sender = event_sender.clone();
                    let chunk_list = chunk_list.clone();
                    spawn_local(async move {
                        let msg = if data.has_type::<web_sys::Blob>() {
                            let data: web_sys::Blob = data.clone().into();
                            if data.size() == 0f64 {
                                return;
                            }
                            let data_buffer =
                                wasm_bindgen_futures::JsFuture::from(data.array_buffer()).await;
                            if let Err(e) = data_buffer {
                                tracing::error!("Failed to read array_buffer from Blob, {:?}", e);
                                return;
                            }
                            Uint8Array::new(&data_buffer.unwrap()).to_vec()
                        } else {
                            Uint8Array::new(data.as_ref()).to_vec()
                        };

                        if msg.is_empty() {
                            return;
                        }

                        let c_lock = chunk_list.try_lock();
                        if c_lock.is_err() {
                            tracing::error!("Failed to lock chunk_list");
                            return;
                        }
                        let mut chunk_list = c_lock.unwrap();

                        let chunk_item = Chunk::from_bincode(&msg);
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

                        if let Err(e) =
                            CbChannel::send(&event_sender, Event::DataChannelMessage(data.into()))
                                .await
                        {
                            tracing::error!("Failed on handle msg, {:?}", e);
                        }
                    });
                })) as Box<dyn FnMut(MessageEvent)>,
            );
            ch.set_onmessage(Some(on_message_cb.as_ref().unchecked_ref()));
            on_message_cb.forget();
        })
    }
}

#[async_trait(?Send)]
impl IceCandidateGathering for WasmTransport {
    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where T: Into<RtcSessionDescription> {
        match &self.get_peer_connection().await {
            Some(c) => {
                let sdp: RtcSessionDescription = desc.into();
                let mut offer_obj = RtcSessionDescriptionInit::new(sdp.type_());
                offer_obj.sdp(&sdp.sdp());
                let promise = c.set_local_description(&offer_obj);
                match JsFuture::from(promise).await {
                    Ok(_) => Ok(()),
                    Err(e) => Err(Error::RTCPeerConnectionSetLocalDescFailed(format!(
                        "{:?}",
                        e
                    ))),
                }
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where T: Into<RtcSessionDescription> {
        match &self.get_peer_connection().await {
            Some(c) => {
                let sdp: RtcSessionDescription = desc.into();
                let mut offer_obj = RtcSessionDescriptionInit::new(sdp.type_());
                let sdp = &sdp.sdp();
                offer_obj.sdp(sdp);
                let promise = c.set_remote_description(&offer_obj);

                match JsFuture::from(promise).await {
                    Ok(_) => {
                        tracing::debug!("set remote sdp success");
                        Ok(())
                    }
                    Err(e) => {
                        tracing::error!("failed to set remote desc: {:?}", e);
                        Err(Error::RTCPeerConnectionSetRemoteDescFailed(format!(
                            "{:?}",
                            e
                        )))
                    }
                }
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    async fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<()> {
        match &self.get_peer_connection().await {
            Some(c) => {
                let cand: RtcIceCandidateInit = candidate.clone().into();
                let promise = c.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&cand));

                match JsFuture::from(promise).await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        tracing::error!("failed to add ice candate");
                        Err(Error::RTCPeerConnectionAddIceCandidateError(format!(
                            "{:?}",
                            e
                        )))
                    }
                }
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }
}

#[async_trait(?Send)]
impl IceTrickleScheme for WasmTransport {
    // https://datatracker.ietf.org/doc/html/rfc5245
    // 1. Send (SdpOffer, IceCandidates) to remote
    // 2. Recv (SdpAnswer, IceCandidate) From Remote

    type SdpType = RtcSdpType;

    async fn get_handshake_info(
        &self,
        session_manager: &SessionManager,
        kind: Self::SdpType,
    ) -> Result<Encoded> {
        let sdp = match kind {
            RtcSdpType::Answer => self.get_answer().await?,
            RtcSdpType::Offer => self.get_offer().await?,
            _ => {
                return Err(Error::RTCSdpTypeNotMatch);
            }
        };

        let local_candidates_json: Vec<IceCandidate> = self
            .get_pending_candidates()
            .await
            .iter()
            .map(|c| js_value::deserialize::<IceCandidate>(&c.clone().to_json()).unwrap())
            .collect();

        if local_candidates_json.is_empty() {
            return Err(Error::FailedOnGatherLocalCandidate);
        }

        let data = TricklePayload {
            sdp: serde_json::to_string(&RtcSessionDescriptionWrapper::from(sdp))
                .map_err(Error::Deserialize)?,
            candidates: local_candidates_json,
        };
        tracing::debug!("prepared handshake info :{:?}", data);
        let fake_did = session_manager.authorizer()?.to_owned();
        let resp = MessagePayload::new_send(data, session_manager, fake_did, fake_did)?;
        Ok(resp.encode()?)
    }

    async fn register_remote_info(&self, data: Encoded) -> Result<Did> {
        let data: MessagePayload<TricklePayload> = data.decode()?;
        tracing::debug!("register remote info: {:?}", &data);

        match data.verify() {
            true => {
                if let Ok(public_key) = data.origin_verification.session.authorizer_pubkey() {
                    let mut pk = self.public_key.write().unwrap();
                    *pk = Some(public_key);
                }
                let sdp: RtcSessionDescriptionWrapper = data.data.sdp.try_into()?;
                self.set_remote_description(sdp.to_owned()).await?;
                for c in &data.data.candidates {
                    tracing::debug!("add remote candidates: {:?}", c);
                    if self.add_ice_candidate(c.clone()).await.is_err() {
                        tracing::warn!("failed on add add candidates: {:?}", c.clone());
                    };
                }
                Ok(data.addr)
            }
            _ => {
                tracing::error!("cannot verify message sig");
                return Err(Error::VerifySignatureFailed);
            }
        }
    }

    async fn wait_for_connected(&self) -> Result<()> {
        tracing::warn!("callback is replaced");
        let promise = self.connect_success_promise().await?;
        promise.await
    }
}

impl WasmTransport {
    pub async fn wait_for_data_channel_open(&self) -> Result<()> {
        if self.is_disconnected().await {
            return Err(Error::RTCPeerConnectionNotEstablish);
        }

        let dc = self.get_data_channel().await;
        match dc {
            Some(dc) => {
                if dc.ready_state() == RtcDataChannelState::Open {
                    return Ok(());
                }
                let promise = Promise::default();
                let state = Arc::clone(&promise.state());
                let dc_cloned = Arc::clone(&dc);
                let callback = Closure::wrap(Box::new(move || {
                    tracing::debug!(
                        "wait_for_data_channel_open, state: {:?}",
                        dc_cloned.ready_state()
                    );
                    match dc_cloned.ready_state() {
                        RtcDataChannelState::Open => {
                            let state = Arc::clone(&state);
                            let mut s = state.lock().unwrap();
                            if let Some(w) = s.waker.take() {
                                w.wake();
                                s.completed = true;
                                s.succeeded = Some(true);
                            }
                        }
                        x => {
                            tracing::debug!("datachannel status: {:?}", x)
                        }
                    }
                }) as Box<dyn FnMut()>);
                dc.set_onopen(Some(callback.as_ref().unchecked_ref()));
                callback.forget();
                promise.await?;
            }
            None => {
                tracing::error!("{:?}", Error::RTCDataChannelNotReady);
                return Err(Error::RTCDataChannelNotReady);
            }
        };
        Ok(())
    }

    pub async fn gather_complete_promise(&self) -> Result<Promise> {
        match self.get_peer_connection().await {
            Some(conn) => {
                let promise = Promise::default();
                let state = Arc::clone(&promise.state());
                let conn_clone = Arc::clone(&conn);
                let callback =
                    Closure::wrap(Box::new(move || match conn_clone.ice_gathering_state() {
                        RtcIceGatheringState::Complete => {
                            let state = Arc::clone(&state);
                            let mut s = state.lock().unwrap();
                            if let Some(w) = s.waker.take() {
                                w.wake();
                                s.completed = true;
                                s.succeeded = Some(true);
                            }
                        }
                        x => {
                            tracing::trace!("gather status: {:?}", x)
                        }
                    }) as Box<dyn FnMut()>);
                conn.set_onicegatheringstatechange(Some(callback.as_ref().unchecked_ref()));
                callback.forget();
                Ok(promise)
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }

    pub async fn connect_success_promise(&self) -> Result<Promise> {
        match self.get_peer_connection().await {
            Some(conn) => {
                let promise = Promise::default();
                let state = Arc::clone(&promise.state());
                let callback = Closure::wrap(Box::new(move |st: RtcIceConnectionState| match st {
                    RtcIceConnectionState::Connected => {
                        let mut s = state.lock().unwrap();
                        if let Some(w) = s.waker.take() {
                            w.wake();
                            s.completed = true;
                            s.succeeded = Some(true);
                        }
                    }
                    RtcIceConnectionState::Failed => {
                        let mut s = state.lock().unwrap();
                        if let Some(w) = s.waker.take() {
                            w.wake();
                            s.completed = true;
                            s.succeeded = Some(false);
                        }
                    }
                    _ => {
                        tracing::trace!("Connect State changed to {:?}", st);
                    }
                })
                    as Box<dyn FnMut(RtcIceConnectionState)>);
                conn.set_oniceconnectionstatechange(Some(callback.as_ref().unchecked_ref()));
                callback.forget();
                Ok(promise)
            }
            None => Err(Error::RTCPeerConnectionNotEstablish),
        }
    }
}
