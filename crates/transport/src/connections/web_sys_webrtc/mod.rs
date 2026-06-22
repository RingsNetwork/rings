use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use js_sys::Array;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use web_sys::MediaStream;
use web_sys::MediaStreamTrack;
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
use web_sys::RtcRtpSender;
use web_sys::RtcSdpType;
use web_sys::RtcSessionDescription;
use web_sys::RtcSessionDescriptionInit;
use web_sys::RtcStatsReport;
use web_sys::RtcTrackEvent;

use crate::callback::InnerTransportCallback;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::media::ChannelConfig;
use crate::core::media::MediaError;
use crate::core::media::MediaKind;
use crate::core::media::MediaTrack;
use crate::core::media::RemoteMediaTrack;
use crate::core::pool::MessageSenderPool;
use crate::core::pool::RoundRobin;
use crate::core::pool::RoundRobinPool;
use crate::core::pool::StatusPool;
use crate::core::transport::effective_max_message_size;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::TransportInterface;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;
use crate::delivery::DeliveryFuture;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::IceCredentialType;
use crate::ice_server::IceServer;
use crate::notifier::Notifier;
use crate::pool::Pool;

const WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT: u8 = 8; // seconds
const WEBRTC_GATHER_TIMEOUT: u8 = 60; // seconds
/// pool size of data channel
const DATA_CHANNEL_POOL_SIZE: u8 = 4;

/// How often the delivery future re-checks whether a message has been flushed.
const DELIVERY_POLL_INTERVAL_MS: u64 = 300;

/// A data channel paired with a monotonic counter of the total bytes ever
/// enqueued onto it. See the native backend for the rationale; the counter
/// lets the delivery future tell, per message, whether the bytes have left the
/// local send buffer (`enqueued_total - buffered_amount`).
type TrackedChannel = (RtcDataChannel, Arc<AtomicU64>);

/// Build the future that resolves once the message ending at `end_offset` on
/// this channel has been flushed to the wire, or errors if the channel closes
/// first. It re-checks on a timer, driving its own wake-ups.
fn delivery_future(
    channel: RtcDataChannel,
    enqueued: Arc<AtomicU64>,
    end_offset: u64,
) -> DeliveryFuture {
    Box::pin(async move {
        loop {
            let buffered = channel.buffered_amount() as u64;
            if enqueued.load(Ordering::SeqCst).saturating_sub(buffered) >= end_offset {
                return Ok(());
            }
            if matches!(
                channel.ready_state(),
                RtcDataChannelState::Closing | RtcDataChannelState::Closed
            ) {
                return Err(Error::MessageNotDelivered(
                    "data channel closed before the message was flushed".to_string(),
                ));
            }
            let notifier = Notifier::default();
            notifier.set_timeout_ms(DELIVERY_POLL_INTERVAL_MS);
            notifier.await;
        }
    })
}

#[async_trait(?Send)]
impl MessageSenderPool<TrackedChannel> for RoundRobinPool<TrackedChannel> {
    type Message = TransportMessage;
    async fn send(&self, msg: TransportMessage) -> Result<DeliveryFuture> {
        let (channel, enqueued) = self.select()?;
        let data = bincode::serialize(&msg)?;
        // `send_with_u8_array` is synchronous, so there's no interleaving to
        // guard; just advance `enqueued` ONLY after a successful send. Advancing
        // first would, on a rejected send, leave the counter ahead of the bytes
        // actually buffered, making earlier messages' delivery futures resolve
        // early on phantom bytes (`enqueued_total - buffered_amount`).
        if let Err(e) = channel
            .send_with_u8_array(&data)
            .map_err(Error::WebSysWebrtc)
        {
            tracing::error!("{:?}, Data size: {:?}", e, data.len());
            return Err(e.into());
        }
        let end_offset =
            enqueued.fetch_add(data.len() as u64, Ordering::SeqCst) + data.len() as u64;
        Ok(delivery_future(channel, enqueued, end_offset))
    }
}

impl StatusPool<TrackedChannel> for RoundRobinPool<TrackedChannel> {
    fn all_ready(&self) -> Result<bool> {
        self.all(|(c, _)| c.ready_state() == RtcDataChannelState::Open)
    }
}

/// A connection that implemented by web_sys library.
/// Used for browser environment.
pub struct WebSysWebrtcConnection {
    webrtc_conn: RtcPeerConnection,
    webrtc_data_channel: Arc<RoundRobinPool<TrackedChannel>>,
    webrtc_data_channel_state_notifier: Notifier,
    /// Negotiated SCTP `max_message_size` (RFC 8841), parsed from the remote SDP at handshake.
    /// `0` means not yet negotiated. Parsed identically to native for consistent behaviour.
    remote_max_message_size: Arc<AtomicUsize>,
    /// The channel contract this connection was created with. `add_media_track` enforces it (data-
    /// only connections reject media; a track's kind must match) so the policy is identical to the
    /// native backend — the same `ChannelConfig` means the same thing on both platforms.
    channel_config: ChannelConfig,
}

/// [WebSysWebrtcTransport] manages all the [WebSysWebrtcConnection] and
/// provides methods to create, get and close connections.
pub struct WebSysWebrtcTransport {
    ice_servers: Vec<IceServer>,
    channel_config: ChannelConfig,
    pool: Pool<WebSysWebrtcConnection>,
}

impl WebSysWebrtcConnection {
    fn new(
        webrtc_conn: RtcPeerConnection,
        webrtc_data_channel: Arc<RoundRobinPool<TrackedChannel>>,
        webrtc_data_channel_state_notifier: Notifier,
        channel_config: ChannelConfig,
    ) -> Self {
        Self {
            webrtc_conn,
            webrtc_data_channel,
            webrtc_data_channel_state_notifier,
            remote_max_message_size: Arc::new(AtomicUsize::new(0)),
            channel_config,
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
    pub fn new(
        ice_servers: &str,
        _external_address: Option<String>,
        channel_config: ChannelConfig,
    ) -> Self {
        let ice_servers = IceServer::vec_from_str(ice_servers).unwrap();

        Self {
            ice_servers,
            channel_config,
            pool: Pool::new(),
        }
    }
}

/// A browser media track (the web-sys side of [`MediaTrack`]), wrapping a `MediaStreamTrack`. The
/// application obtains the local one from `getUserMedia`/canvas and attaches the remote one to a
/// `<video>`; this is the browser analogue of [`NativeMediaTrack`](crate::connections::NativeMediaTrack).
pub struct BrowserMediaTrack {
    track: MediaStreamTrack,
}

impl BrowserMediaTrack {
    /// Wrap a `MediaStreamTrack`.
    pub fn new(track: MediaStreamTrack) -> Self {
        Self { track }
    }
}

impl MediaTrack for BrowserMediaTrack {
    fn id(&self) -> String {
        self.track.id()
    }

    fn kind(&self) -> MediaKind {
        if self.track.kind() == "audio" {
            MediaKind::Audio
        } else {
            MediaKind::Video
        }
    }

    fn enabled(&self) -> bool {
        self.track.enabled()
    }

    fn set_enabled(&self, enabled: bool) {
        self.track.set_enabled(enabled);
    }
}

impl RemoteMediaTrack for BrowserMediaTrack {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[async_trait(?Send)]
impl ConnectionInterface for WebSysWebrtcConnection {
    type Sdp = String;
    type Error = Error;
    type LocalMediaTrack = BrowserMediaTrack;

    async fn send_message(&self, msg: TransportMessage) -> Result<DeliveryFuture> {
        self.webrtc_wait_for_data_channel_open().await?;
        self.webrtc_data_channel.send(msg).await
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.webrtc_conn.connection_state().into()
    }

    fn max_message_size(&self) -> usize {
        // The value negotiated from the remote SDP at handshake; `0` = not yet negotiated, so
        // fall back to the interop default. Same parsing as native (consistent behaviour).
        match self.remote_max_message_size.load(Ordering::SeqCst) {
            0 => MAX_DATA_CHANNEL_MESSAGE_SIZE,
            n => n,
        }
    }

    async fn add_media_track(
        &self,
        track: BrowserMediaTrack,
    ) -> std::result::Result<String, MediaError> {
        // Same contract as native: reject media on a data-only connection and require the track's
        // kind to match the negotiated one, so `ChannelConfig` is enforced identically here.
        self.channel_config.admit_local_track(track.kind())?;
        let track_id = track.id();
        let stream = MediaStream::new().map_err(|e| MediaError::AddTrack(format!("{e:?}")))?;
        self.webrtc_conn
            .add_track(&track.track, &stream, &js_sys::Array::new());
        Ok(track_id)
    }

    async fn remove_media_track(&self, track_id: &str) -> std::result::Result<(), MediaError> {
        // Detach exactly the one sender carrying this (unique) track id, undoing `add_media_track`.
        // Removing an unknown id is a no-op. `MediaStreamTrack` ids are browser-generated and unique.
        for sender in self.webrtc_conn.get_senders().iter() {
            let sender: RtcRtpSender = sender.into();
            let matches = sender.track().map(|t| t.id() == track_id).unwrap_or(false);
            if matches {
                self.webrtc_conn.remove_track(&sender);
                break;
            }
        }
        Ok(())
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
        // `createOffer` is pre-apply: a failure here has not touched signaling state.
        let promise = self.webrtc_conn.create_offer();
        let offer_js_value = JsFuture::from(promise)
            .await
            .map_err(|e| Error::WebrtcSignalingPreApply(format!("{e:?}")))?;
        let offer = RtcSessionDescription::from(offer_js_value);
        let sdp = offer.sdp();

        // `setLocalDescription` and everything after it mutate signaling state (post-apply).
        let set_local_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        set_local_init.set_sdp(&sdp);

        let promise = self.webrtc_conn.set_local_description(&set_local_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;

        self.webrtc_gather().await
    }

    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp> {
        tracing::debug!("webrtc_answer_offer, offer: {offer:?}");
        // Compute the negotiated limit but do not record it until the offer is actually applied — the
        // store is an observable mutation that must not survive a rejected/un-applied offer. (web-sys
        // has no separate pre-apply parse; `setRemoteDescription` is both the first fallible step and
        // the apply, so its failure is post-apply.)
        let negotiated_max_message_size = effective_max_message_size(&offer);

        let set_remote_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        set_remote_init.set_sdp(&offer);

        let promise = self.webrtc_conn.set_remote_description(&set_remote_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;
        self.remote_max_message_size
            .store(negotiated_max_message_size, Ordering::SeqCst);

        let promise = self.webrtc_conn.create_answer();
        let answer_js_value = JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;
        let answer = RtcSessionDescription::from(answer_js_value);
        let sdp = answer.sdp();

        let set_local_init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        set_local_init.set_sdp(&sdp);

        let promise = self.webrtc_conn.set_local_description(&set_local_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;

        self.webrtc_gather().await
    }

    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<()> {
        tracing::debug!("webrtc_accept_answer, answer: {answer:?}");
        let negotiated_max_message_size = effective_max_message_size(&answer);

        let set_remote_init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        set_remote_init.set_sdp(&answer);

        let promise = self.webrtc_conn.set_remote_description(&set_remote_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;
        // Applied: now it is correct to record the negotiated limit.
        self.remote_max_message_size
            .store(negotiated_max_message_size, Ordering::SeqCst);

        Ok(())
    }

    async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
        // `Disconnected` is intentionally not treated as unavailable: it is a
        // transient ICE state in which the data channel stays open, so we let
        // the send proceed (the bytes buffer and flush on recovery). The
        // returned `DeliveryFuture` reports whether they actually made it out.
        if matches!(
            self.webrtc_connection_state(),
            WebrtcConnectionState::Failed | WebrtcConnectionState::Closed
        ) {
            return Err(Error::DataChannelOpen("Connection unavailable".to_string()));
        }

        if self.webrtc_data_channel.all_ready()? {
            return Ok(());
        }

        self.webrtc_data_channel_state_notifier
            .set_timeout(WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT);
        self.webrtc_data_channel_state_notifier.clone().await;

        if self.webrtc_data_channel.all_ready()? {
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
        let config = RtcConfiguration::new();
        let ice_servers: js_sys::Array =
            js_sys::Array::from_iter(self.ice_servers.iter().cloned().map(RtcIceServer::from));
        config.set_ice_servers(&ice_servers.into());

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
        let channel_pool = Arc::new(RoundRobinPool::default());

        let on_data_channel = Box::new(move |ev: RtcDataChannelEvent| {
            let d = ev.channel();
            let d_label = d.label();
            tracing::debug!("New DataChannel {d_label}");
            // Open/close are detected on the channels we create (the pool, wired
            // below); a received channel only carries inbound messages. Wiring
            // open/close here too would fire on_data_channel_open twice (created
            // + received) and churn join_dht.
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

        // Inbound media: deliver each remote track to the callback as a `MediaTrack` (the app
        // attaches it to a sink). Outbound tracks are added by the app via `add_media_track`.
        let on_track_inner_cb = inner_cb.clone();
        let on_track = Box::new(move |ev: RtcTrackEvent| {
            let track = ev.track();
            let inner_cb = on_track_inner_cb.clone();
            spawn_local(async move {
                inner_cb
                    .on_media_track(Box::new(BrowserMediaTrack::new(track)))
                    .await;
            });
        });
        let c = Closure::wrap(on_track as Box<dyn FnMut(RtcTrackEvent)>);
        webrtc_conn.set_ontrack(Some(c.as_ref().unchecked_ref()));
        c.forget();

        //
        // Create data channel
        //
        // Wire open/close on the channels this side creates (the pool), not only
        // on received channels: a received channel's `onopen` can be missed if
        // it opens before the handler is registered, so `on_data_channel_open`
        // (and thus `join_dht`) would never fire. Created channels are wired
        // before they can open, so this is reliable.
        for i in 0..DATA_CHANNEL_POOL_SIZE {
            let ch = webrtc_conn.create_data_channel(&format!("rings_data_channel_{}", i));

            let on_open_pool = channel_pool.clone();
            let on_open_cb = inner_cb.clone();
            let on_open = Box::new(move || {
                let pool = on_open_pool.clone();
                let cb = on_open_cb.clone();
                spawn_local(async move {
                    if let Ok(true) = pool.all_ready() {
                        cb.on_data_channel_open().await;
                    }
                });
            });
            let c = Closure::wrap(on_open as Box<dyn FnMut()>);
            ch.set_onopen(Some(c.as_ref().unchecked_ref()));
            c.forget();

            let on_close_pool = channel_pool.clone();
            let on_close_cb = inner_cb.clone();
            let on_close = Box::new(move || {
                let pool = on_close_pool.clone();
                let cb = on_close_cb.clone();
                spawn_local(async move {
                    if let Ok(true) =
                        pool.all(|(c, _)| c.ready_state() == RtcDataChannelState::Closed)
                    {
                        cb.on_data_channel_close().await;
                    }
                });
            });
            let c = Closure::wrap(on_close as Box<dyn FnMut()>);
            ch.set_onclose(Some(c.as_ref().unchecked_ref()));
            c.forget();

            channel_pool.push((ch, Arc::new(AtomicU64::new(0))))?;
        }

        //
        // Construct the Connection
        //
        let conn = WebSysWebrtcConnection::new(
            webrtc_conn,
            channel_pool,
            webrtc_data_channel_state_notifier,
            self.channel_config.clone(),
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
        let ret = RtcIceServer::new();
        let urls = Array::new();
        for u in s.urls {
            let url = JsValue::from_str(&u);
            urls.push(&url);
        }
        if !s.username.is_empty() {
            ret.set_username(&s.username);
        }
        if !s.credential.is_empty() {
            ret.set_credential(&s.credential);
        }
        ret.set_credential_type(s.credential_type.into());
        ret.set_urls(&urls);
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
