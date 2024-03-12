use std::sync::Arc;

use async_trait::async_trait;
use js_sys::Array;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use web_sys::MessageEvent;
use web_sys::RtcConfiguration;
use web_sys::RtcDataChannel;
use web_sys::RtcDataChannelEvent;
use web_sys::RtcDataChannelState;
use web_sys::RtcIceCredentialType;
use web_sys::RtcIceGatheringState;
use web_sys::RtcIceServer;
use web_sys::RtcPeerConnection;
use web_sys::RtcPeerConnectionState;
use web_sys::RtcSdpType;
use web_sys::RtcSessionDescription;
use web_sys::RtcSessionDescriptionInit;
use web_sys::RtcStatsReport;

use crate::callback::InnerTransportCallback;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::TransportInterface;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::IceCredentialType;
use crate::ice_server::IceServer;
use crate::notifier::Notifier;
use crate::pool::Pool;
use crate::connections::channel_pool::RoundRobin;
use crate::connections::channel_pool::RoundRobinPool;
use crate::connections::channel_pool::ChannelPool;
use crate::connections::channel_pool::ChannelPoolStatus;


const WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT: u8 = 8; // seconds
const WEBRTC_GATHER_TIMEOUT: u8 = 60; // seconds
const DATA_CHANNEL_POOL_SIZE: u8 = 4; /// pool size of data channel

#[async_trait(?Send)]
impl ChannelPool<RtcDataChannel> for RoundRobinPool<RtcDataChannel> {
    async fn send(&self, msg: TransportMessage) -> Result<()> {
	let channel = self.select();
        let data = bincode::serialize(&msg)?;
        if let Err(e) = channel.send_with_u8_array(&data).map_err(Error::WebSysWebrtc) {
            tracing::error!("{:?}, Data size: {:?}", e, data.len());
            return Err(e.into());
        }
	Ok(())
    }
}

impl ChannelPoolStatus<RtcDataChannel> for RoundRobinPool<RtcDataChannel> {
    fn all_ready(&self) -> bool {
	self.all().iter().all(|c| c.ready_state() == RtcDataChannelState::Open )
    }
}


/// A connection that implemented by web_sys library.
/// Used for browser environment.
pub struct WebSysWebrtcConnection {
    webrtc_conn: RtcPeerConnection,
    webrtc_data_channel: RoundRobinPool<RtcDataChannel>,
    webrtc_data_channel_state_notifier: Notifier,
}

/// [WebSysWebrtcTransport] manages all the [WebSysWebrtcConnection] and
/// provides methods to create, get and close connections.
pub struct WebSysWebrtcTransport {
    ice_servers: Vec<IceServer>,
    pool: Pool<WebSysWebrtcConnection>,
}

impl WebSysWebrtcConnection {
    fn new(
        webrtc_conn: RtcPeerConnection,
        webrtc_data_channel: RoundRobinPool<RtcDataChannel>,
        webrtc_data_channel_state_notifier: Notifier,
    ) -> Self {
        Self {
            webrtc_conn,
            webrtc_data_channel,
            webrtc_data_channel_state_notifier,
        }
    }

    async fn webrtc_gather(&self) -> Result<String> {
        let notifier = Notifier::default();

        let notifier_clone = notifier.clone();
        let conn_clone = self.webrtc_conn.clone();
        let onicegatheringstatechange = Box::new(move || match conn_clone.ice_gathering_state() {
            RtcIceGatheringState::Complete => notifier_clone.wake(),
            x => {
                tracing::trace!("gather status: {:?}", x)
            }
        });

        let c = Closure::wrap(onicegatheringstatechange as Box<dyn FnMut()>);
        self.webrtc_conn
            .set_onicegatheringstatechange(Some(c.as_ref().unchecked_ref()));
        c.forget();

        notifier.set_timeout(WEBRTC_GATHER_TIMEOUT);
        notifier.await;
        if self.webrtc_conn.ice_gathering_state() != RtcIceGatheringState::Complete {
            return Err(Error::WebrtcLocalSdpGenerationError(format!(
                "Webrtc gathering is not completed in {WEBRTC_GATHER_TIMEOUT} seconds"
            )));
        }

        self.webrtc_conn
            .local_description()
            .ok_or(Error::WebrtcLocalSdpGenerationError(
                "local_description is None".to_string(),
            ))
            .map(|x| x.sdp())
    }
}

impl WebSysWebrtcTransport {
    /// Create a new [WebSysWebrtcTransport] instance.
    pub fn new(ice_servers: &str, _external_address: Option<String>) -> Self {
        let ice_servers = IceServer::vec_from_str(ice_servers).unwrap();

        Self {
            ice_servers,
            pool: Pool::new(),
        }
    }
}

#[async_trait(?Send)]
impl ConnectionInterface for WebSysWebrtcConnection {
    type Sdp = String;
    type Error = Error;

    async fn send_message(&self, msg: TransportMessage) -> Result<()> {
        self.webrtc_wait_for_data_channel_open().await?;
        self.webrtc_data_channel.send(msg).await?;
        Ok(())
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.webrtc_conn.connection_state().into()
    }

    async fn get_stats(&self) -> Vec<String> {
        let promise = self.webrtc_conn.get_stats();
        let Ok(value) = wasm_bindgen_futures::JsFuture::from(promise).await else {
            return vec![];
        };

        let stats: RtcStatsReport = value.into();

        stats
            .entries()
            .into_iter()
            .map(|x| dump_stats_entry(&x.ok()).unwrap_or("failed to dump stats entry".to_string()))
            .collect::<Vec<_>>()
    }

    async fn webrtc_create_offer(&self) -> Result<Self::Sdp> {
        let promise = self.webrtc_conn.create_offer();
        let offer_js_value = JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;
        let offer = RtcSessionDescription::from(offer_js_value);
        let sdp = offer.sdp();

        let mut set_local_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        set_local_init.sdp(&sdp);

        let promise = self.webrtc_conn.set_local_description(&set_local_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;

        self.webrtc_gather().await
    }

    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp> {
        tracing::debug!("webrtc_answer_offer, offer: {offer:?}");

        let mut set_remote_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        set_remote_init.sdp(&offer);

        let promise = self.webrtc_conn.set_remote_description(&set_remote_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;

        let promise = self.webrtc_conn.create_answer();
        let answer_js_value = JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;
        let answer = RtcSessionDescription::from(answer_js_value);
        let sdp = answer.sdp();

        let mut set_local_init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        set_local_init.sdp(&sdp);

        let promise = self.webrtc_conn.set_local_description(&set_local_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;

        self.webrtc_gather().await
    }

    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<()> {
        tracing::debug!("webrtc_accept_answer, answer: {answer:?}");

        let mut set_remote_init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        set_remote_init.sdp(&answer);

        let promise = self.webrtc_conn.set_remote_description(&set_remote_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;

        Ok(())
    }

    async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
        if matches!(
            self.webrtc_connection_state(),
            WebrtcConnectionState::Failed
                | WebrtcConnectionState::Closed
                | WebrtcConnectionState::Disconnected
        ) {
            return Err(Error::DataChannelOpen("Connection unavailable".to_string()));
        }

        if self.webrtc_data_channel.all_ready() {
            return Ok(());
        }

        self.webrtc_data_channel_state_notifier
            .set_timeout(WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT);
        self.webrtc_data_channel_state_notifier.clone().await;

        if self.webrtc_data_channel.all_ready() {
            return Ok(());
        } else {
            return Err(Error::DataChannelOpen(format!(
                "DataChannel not open in {WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT} seconds"
            )));
        }
    }

    async fn close(&self) -> Result<()> {
        self.webrtc_conn.close();
        Ok(())
    }
}

#[async_trait(?Send)]
impl TransportInterface for WebSysWebrtcTransport {
    type Connection = WebSysWebrtcConnection;
    type Error = Error;

    async fn new_connection(&self, cid: &str, callback: BoxedTransportCallback) -> Result<()> {
        if let Ok(existed_conn) = self.pool.connection(cid) {
            if matches!(
                existed_conn.webrtc_connection_state(),
                WebrtcConnectionState::New
                    | WebrtcConnectionState::Connecting
                    | WebrtcConnectionState::Connected
            ) {
                return Err(Error::ConnectionAlreadyExists(cid.to_string()));
            }
        }

        //
        // Setup webrtc connection env
        //
        let mut config = RtcConfiguration::new();
        let ice_servers: js_sys::Array =
            js_sys::Array::from_iter(self.ice_servers.iter().cloned().map(RtcIceServer::from));
        config.ice_servers(&ice_servers.into());

        //
        // Create webrtc connection
        //
        let webrtc_conn =
            RtcPeerConnection::new_with_configuration(&config).map_err(Error::WebSysWebrtc)?;

        //
        // Set callbacks
        //
        let webrtc_data_channel_state_notifier = Notifier::default();
        let inner_cb = Arc::new(InnerTransportCallback::new(
            cid,
            callback,
            webrtc_data_channel_state_notifier.clone(),
        ));

        let data_channel_inner_cb = inner_cb.clone();
        let on_data_channel = Box::new(move |ev: RtcDataChannelEvent| {
            let d = ev.channel();
            let d_label = d.label();
            tracing::debug!("New DataChannel {d_label}");

            let on_open_inner_cb = data_channel_inner_cb.clone();
            let on_open = Box::new(move || {
                on_open_inner_cb.on_data_channel_open();
            });

            let on_close_inner_cb = data_channel_inner_cb.clone();
            let on_close = Box::new(move || {
                on_close_inner_cb.on_data_channel_close();
            });

            let on_message_inner_cb = data_channel_inner_cb.clone();
            let on_message = Box::new(move |ev: MessageEvent| {
                let data = ev.data();

                let inner_cb = on_message_inner_cb.clone();

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
                        js_sys::Uint8Array::new(&data_buffer.unwrap()).to_vec()
                    } else {
                        js_sys::Uint8Array::new(data.as_ref()).to_vec()
                    };

                    if msg.is_empty() {
                        tracing::debug!("Received empty DataChannelMessage from {}", inner_cb.cid);
                        return;
                    }

                    tracing::debug!(
                        "Received DataChannelMessage from {}: {:?}",
                        inner_cb.cid,
                        data
                    );

                    inner_cb.on_message(&msg.into()).await;
                })
            });

            let c = Closure::wrap(on_open as Box<dyn FnMut()>);
            d.set_onopen(Some(c.as_ref().unchecked_ref()));
            c.forget();

            let c = Closure::wrap(on_close as Box<dyn FnMut()>);
            d.set_onclose(Some(c.as_ref().unchecked_ref()));
            c.forget();

            let c = Closure::wrap(on_message as Box<dyn FnMut(MessageEvent)>);
            d.set_onmessage(Some(c.as_ref().unchecked_ref()));
            c.forget();
        });

        let peer_connection_state_change_inner_cb = inner_cb.clone();
        let peer_connection_state_change_webrtc_conn = webrtc_conn.clone();
        let on_peer_connection_state_change = Box::new(move |_| {
            let s = peer_connection_state_change_webrtc_conn.connection_state();
            tracing::debug!("Peer Connection State has changed: {s:?}");

            let inner_cb = peer_connection_state_change_inner_cb.clone();

            spawn_local(async move {
                inner_cb.on_peer_connection_state_change(s.into()).await;
            })
        });

        let c = Closure::wrap(on_data_channel as Box<dyn FnMut(RtcDataChannelEvent)>);
        webrtc_conn.set_ondatachannel(Some(c.as_ref().unchecked_ref()));
        c.forget();

        let c = Closure::wrap(on_peer_connection_state_change as Box<dyn FnMut(web_sys::Event)>);
        webrtc_conn.set_onconnectionstatechange(Some(c.as_ref().unchecked_ref()));
        c.forget();

        //
        // Create data channel
        //
	let mut channel_pool = vec![];
	for i in 0..DATA_CHANNEL_POOL_SIZE {
            let ch = webrtc_conn.create_data_channel(&format!("rings_data_chanel_{}", i));
	    channel_pool.push(ch);
	}

        //
        // Construct the Connection
        //
        let conn = WebSysWebrtcConnection::new(
            webrtc_conn,
            RoundRobinPool::from_vec(channel_pool),
            webrtc_data_channel_state_notifier,
        );

        self.pool.safely_insert(cid, conn)?;
        Ok(())
    }

    async fn close_connection(&self, cid: &str) -> Result<()> {
        self.pool.safely_remove(cid).await
    }

    fn connection(&self, cid: &str) -> Result<ConnectionRef<Self::Connection>> {
        self.pool.connection(cid)
    }

    fn connections(&self) -> Vec<(String, ConnectionRef<Self::Connection>)> {
        self.pool.connections()
    }

    fn connection_ids(&self) -> Vec<String> {
        self.pool.connection_ids()
    }
}

// set default to password
impl From<IceCredentialType> for RtcIceCredentialType {
    fn from(s: IceCredentialType) -> Self {
        match s {
            IceCredentialType::Password => Self::Password,
            IceCredentialType::Oauth => Self::Token,
        }
    }
}

impl From<IceServer> for RtcIceServer {
    fn from(s: IceServer) -> Self {
        let mut ret = RtcIceServer::new();
        let urls = Array::new();
        for u in s.urls {
            let url = JsValue::from_str(&u);
            urls.push(&url);
        }
        if !s.username.is_empty() {
            ret.username(&s.username);
        }
        if !s.credential.is_empty() {
            ret.credential(&s.credential);
        }
        ret.credential_type(s.credential_type.into());
        ret.urls(&urls);
        ret
    }
}

impl From<IceServer> for JsValue {
    fn from(a: IceServer) -> Self {
        let ret: RtcIceServer = a.into();
        ret.into()
    }
}

impl From<RtcPeerConnectionState> for WebrtcConnectionState {
    fn from(s: RtcPeerConnectionState) -> Self {
        match s {
            RtcPeerConnectionState::New => Self::New,
            RtcPeerConnectionState::Connecting => Self::Connecting,
            RtcPeerConnectionState::Connected => Self::Connected,
            RtcPeerConnectionState::Disconnected => Self::Disconnected,
            RtcPeerConnectionState::Failed => Self::Failed,
            RtcPeerConnectionState::Closed => Self::Closed,
            _ => {
                tracing::warn!("Unknown RtcPeerConnectionState: {s:?}");
                Self::Unspecified
            }
        }
    }
}

fn dump_stats_entry(entry: &Option<JsValue>) -> Option<String> {
    js_sys::JSON::stringify(entry.as_ref()?)
        .ok()
        .and_then(|x| x.as_string())
}
