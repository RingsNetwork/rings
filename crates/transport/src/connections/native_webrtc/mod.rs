use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::media_engine::MIME_TYPE_OPUS;
use webrtc::api::media_engine::MIME_TYPE_VP8;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_remote::TrackRemote;

use crate::callback::InnerTransportCallback;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::media::ChannelConfig;
use crate::core::media::MediaChannelConfig;
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
use crate::ice_server::IceServer;
use crate::notifier::Notifier;
use crate::pool::Pool;

const WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT: u8 = 8; // seconds
const WEBRTC_GATHER_TIMEOUT: u8 = 60; // seconds
/// pool size of data channel
const DATA_CHANNEL_POOL_SIZE: u8 = 4;

/// How often the delivery future re-checks whether a message has been flushed.
const DELIVERY_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// A data channel paired with a monotonic counter of the total bytes ever
/// enqueued onto it, plus a lock that serializes sends. The counter lets the
/// delivery future tell, per message, whether the bytes have been flushed to
/// the wire: `enqueued_total - buffered_amount` is the number of bytes already
/// handed off, so a message whose end offset is below that has left the local
/// send buffer.
///
/// The lock is held across reserve+send so the reserved end offset always
/// matches the order bytes are actually enqueued in. Without it, two concurrent
/// senders could reserve offsets in one order but reach `channel.send().await`
/// (which yields) in the other, making an earlier future resolve against a
/// later message's bytes.
type TrackedChannel = (Arc<RTCDataChannel>, Arc<AtomicU64>, Arc<Mutex<()>>);

/// Build the future that resolves once the message ending at `end_offset` on
/// this channel has been flushed to the wire, or errors if the channel closes
/// first. It re-checks on a timer, driving its own wake-ups.
fn delivery_future(
    channel: Arc<RTCDataChannel>,
    enqueued: Arc<AtomicU64>,
    end_offset: u64,
) -> DeliveryFuture {
    Box::pin(async move {
        loop {
            let buffered = channel.buffered_amount().await as u64;
            if enqueued.load(Ordering::SeqCst).saturating_sub(buffered) >= end_offset {
                return Ok(());
            }
            if matches!(
                channel.ready_state(),
                RTCDataChannelState::Closing | RTCDataChannelState::Closed
            ) {
                return Err(Error::MessageNotDelivered(
                    "data channel closed before the message was flushed".to_string(),
                ));
            }
            tokio::time::sleep(DELIVERY_POLL_INTERVAL).await;
        }
    })
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl MessageSenderPool<TrackedChannel> for RoundRobinPool<TrackedChannel> {
    type Message = TransportMessage;
    async fn send(&self, msg: TransportMessage) -> Result<DeliveryFuture> {
        let (channel, enqueued, send_lock) = self.select()?;
        let data = bincode::serialize(&msg).map(Bytes::from)?;
        // Hold the per-channel lock across send + counter advance so the bytes
        // are enqueued and accounted in the same (FIFO) order: concurrent senders
        // can't interleave the yielding send and the counter update. Advance
        // `enqueued` ONLY after a successful send — otherwise a failed send would
        // leave the counter ahead of what was actually queued, making earlier
        // messages' delivery futures resolve early on phantom bytes.
        let end_offset = {
            let _guard = send_lock.lock().await;
            if let Err(e) = channel.send(&data).await {
                tracing::error!("{:?}, Data size: {:?}", e, data.len());
                return Err(e.into());
            }
            enqueued.fetch_add(data.len() as u64, Ordering::SeqCst) + data.len() as u64
        };
        Ok(delivery_future(channel, enqueued, end_offset))
    }
}

impl StatusPool<TrackedChannel> for RoundRobinPool<TrackedChannel> {
    fn all_ready(&self) -> Result<bool> {
        self.all(|(c, _, _)| c.ready_state() == RTCDataChannelState::Open)
    }
}

/// Build the codec capability + RTP codec type for a media track configuration. The MIME type is
/// derived from the kind (we move opaque RTP, so the codec is nominal — only the audio/video split
/// and the clock rate / payload type matter for negotiation).
fn media_codec(cfg: &MediaChannelConfig) -> (RTCRtpCodecCapability, RTPCodecType) {
    let (mime_type, channels, codec_type) = match cfg.kind {
        MediaKind::Audio => (MIME_TYPE_OPUS.to_owned(), 2, RTPCodecType::Audio),
        MediaKind::Video => (MIME_TYPE_VP8.to_owned(), 0, RTPCodecType::Video),
    };
    (
        RTCRtpCodecCapability {
            mime_type,
            clock_rate: cfg.clock_rate,
            channels,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: vec![],
        },
        codec_type,
    )
}

/// A local, sendable media track (native side of [`MediaTrack`]). Built by the application via
/// [`NativeMediaTrack::new`], handed to a connection with `add_media_track`, then fed encoded media
/// with [`NativeMediaTrack::write_sample`] — the native analogue of attaching a browser
/// `MediaStreamTrack` and letting it source frames.
#[derive(Clone)]
pub struct NativeMediaTrack {
    track: Arc<TrackLocalStaticSample>,
    kind: MediaKind,
    enabled: Arc<AtomicBool>,
}

impl NativeMediaTrack {
    /// Create a local track for the given media configuration.
    pub fn new(config: &MediaChannelConfig) -> Self {
        let (capability, _) = media_codec(config);
        let track = Arc::new(TrackLocalStaticSample::new(
            capability,
            "rings-media".to_string(),
            "rings-stream".to_string(),
        ));
        Self {
            track,
            kind: config.kind,
            enabled: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Write one encoded media sample (`data` + its `duration`) to be sent to the peer; no-op while
    /// disabled. Takes plain bytes rather than a webrtc `Sample` so callers need not depend on
    /// webrtc-rs directly.
    pub async fn write_sample(
        &self,
        data: Bytes,
        duration: std::time::Duration,
    ) -> std::result::Result<(), MediaError> {
        if !self.enabled.load(Ordering::SeqCst) {
            return Ok(());
        }
        let sample = Sample {
            data,
            duration,
            ..Default::default()
        };
        self.track
            .write_sample(&sample)
            .await
            .map_err(|e| MediaError::AddTrack(e.to_string()))
    }
}

impl MediaTrack for NativeMediaTrack {
    fn id(&self) -> String {
        self.track.id().to_string()
    }

    fn kind(&self) -> MediaKind {
        self.kind
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }
}

/// A remote, inbound media track delivered to `on_media_track`. Read its RTP with
/// [`NativeRemoteTrack::read_rtp`] (the native analogue of attaching a browser remote
/// `MediaStreamTrack` to a sink).
#[derive(Clone)]
pub struct NativeRemoteTrack {
    track: Arc<TrackRemote>,
    kind: MediaKind,
}

impl NativeRemoteTrack {
    /// Read the next RTP packet from the remote track. Errors once the track ends.
    pub async fn read_rtp(&self) -> std::result::Result<webrtc::rtp::packet::Packet, MediaError> {
        self.track
            .read_rtp()
            .await
            .map(|(packet, _)| packet)
            .map_err(|e| MediaError::AddTrack(e.to_string()))
    }
}

impl MediaTrack for NativeRemoteTrack {
    fn id(&self) -> String {
        self.track.id()
    }

    fn kind(&self) -> MediaKind {
        self.kind
    }

    fn enabled(&self) -> bool {
        true
    }

    fn set_enabled(&self, _enabled: bool) {}
}

impl RemoteMediaTrack for NativeRemoteTrack {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A connection that implemented by webrtc-rs library.
/// Used for native environment.
pub struct WebrtcConnection {
    webrtc_conn: RTCPeerConnection,
    webrtc_data_channel: Arc<RoundRobinPool<TrackedChannel>>,
    webrtc_data_channel_state_notifier: Notifier,
    cancel_token: CancellationToken,
    /// Negotiated SCTP `max_message_size` (RFC 8841), parsed from the remote SDP at handshake.
    /// `0` means not yet negotiated. webrtc-rs exposes no getter, so we track it ourselves.
    remote_max_message_size: Arc<AtomicUsize>,
    /// The channel contract this connection was created with. `add_media_track` enforces it (data-
    /// only connections reject media; a track's kind must match) so the policy is identical to the
    /// browser backend.
    channel_config: ChannelConfig,
}

/// [WebrtcTransport] manages all the [WebrtcConnection] and
/// provides methods to create, get and close connections.
pub struct WebrtcTransport {
    ice_servers: Vec<IceServer>,
    external_address: Option<String>,
    channel_config: ChannelConfig,
    pool: Pool<WebrtcConnection>,
}

impl WebrtcConnection {
    fn new(
        webrtc_conn: RTCPeerConnection,
        webrtc_data_channel: Arc<RoundRobinPool<TrackedChannel>>,
        webrtc_data_channel_state_notifier: Notifier,
        channel_config: ChannelConfig,
    ) -> Self {
        Self {
            webrtc_conn,
            webrtc_data_channel,
            webrtc_data_channel_state_notifier,
            cancel_token: CancellationToken::new(),
            remote_max_message_size: Arc::new(AtomicUsize::new(0)),
            channel_config,
        }
    }

    async fn webrtc_gather(&self) -> Result<String> {
        let mut gathering_complete_promise = self.webrtc_conn.gathering_complete_promise().await;
        let gathering_complete_promise_with_timeout = tokio::time::timeout(
            std::time::Duration::from_secs(WEBRTC_GATHER_TIMEOUT.into()),
            gathering_complete_promise.recv(),
        );

        tokio::select! {
            _ = self.cancel_token.cancelled() => {
                return Err(Error::WebrtcLocalSdpGenerationError("Local connection closed".to_string()))
            }
            _ = gathering_complete_promise_with_timeout => {}
        }

        Ok(self
            .webrtc_conn
            .local_description()
            .await
            .ok_or(Error::WebrtcLocalSdpGenerationError(
                "Failed to get local description".to_string(),
            ))?
            .sdp)
    }
}

impl WebrtcTransport {
    /// Create a new [WebrtcTransport] instance. `channel_config` selects whether connections also
    /// negotiate a media track (data-only by default).
    pub fn new(
        ice_servers: &str,
        external_address: Option<String>,
        channel_config: ChannelConfig,
    ) -> Self {
        let ice_servers = IceServer::vec_from_str(ice_servers).unwrap();

        Self {
            ice_servers,
            external_address,
            channel_config,
            pool: Pool::new(),
        }
    }
}

#[async_trait]
impl ConnectionInterface for WebrtcConnection {
    type Sdp = String;
    type Error = Error;
    type LocalMediaTrack = NativeMediaTrack;

    async fn send_message(&self, msg: TransportMessage) -> Result<DeliveryFuture> {
        self.webrtc_wait_for_data_channel_open().await?;
        self.webrtc_data_channel.send(msg).await
    }

    async fn get_stats(&self) -> Vec<String> {
        self.webrtc_conn
            .get_stats()
            .await
            .reports
            .into_iter()
            .map(|x| serde_json::to_string(&x).unwrap_or("failed to dump stats entry".to_string()))
            .collect()
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.webrtc_conn.connection_state().into()
    }

    fn max_message_size(&self) -> usize {
        // The value negotiated from the remote SDP at handshake; `0` = not yet negotiated, so
        // fall back to the interop default.
        match self.remote_max_message_size.load(Ordering::SeqCst) {
            0 => MAX_DATA_CHANNEL_MESSAGE_SIZE,
            n => n,
        }
    }

    async fn add_media_track(
        &self,
        track: NativeMediaTrack,
    ) -> std::result::Result<(), MediaError> {
        // Same contract as the browser backend: reject media on a data-only connection and require
        // the track's kind to match the negotiated one.
        self.channel_config.admit_local_track(track.kind())?;
        let sender = self
            .webrtc_conn
            .add_track(track.track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .map_err(|e| MediaError::AddTrack(e.to_string()))?;
        // Drain the sender's RTCP so the interceptor buffer does not fill.
        tokio::spawn(async move {
            let mut rtcp_buf = vec![0u8; 1500];
            while sender.read(&mut rtcp_buf).await.is_ok() {}
        });
        Ok(())
    }

    async fn webrtc_create_offer(&self) -> Result<Self::Sdp> {
        let setting_offer = self.webrtc_conn.create_offer(None).await?;
        self.webrtc_conn
            .set_local_description(setting_offer.clone())
            .await?;

        self.webrtc_gather().await
    }

    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp> {
        tracing::debug!("webrtc_answer_offer, offer: {offer:?}");
        self.remote_max_message_size
            .store(effective_max_message_size(&offer), Ordering::SeqCst);
        // Parse is pre-apply: a malformed offer is rejected here without mutating signaling state.
        let offer = RTCSessionDescription::offer(offer)
            .map_err(|e| Error::RemoteSdpRejected(e.to_string()))?;
        self.webrtc_conn.set_remote_description(offer).await?;

        let answer = self.webrtc_conn.create_answer(None).await?;
        self.webrtc_conn
            .set_local_description(answer.clone())
            .await?;

        self.webrtc_gather().await
    }

    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<()> {
        tracing::debug!("webrtc_accept_answer, answer: {answer:?}");
        self.remote_max_message_size
            .store(effective_max_message_size(&answer), Ordering::SeqCst);
        // Parse is pre-apply: a malformed answer is rejected here without mutating signaling state.
        let answer = RTCSessionDescription::answer(answer)
            .map_err(|e| Error::RemoteSdpRejected(e.to_string()))?;
        self.webrtc_conn
            .set_remote_description(answer)
            .await
            .map_err(|e| e.into())
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
        self.cancel_token.cancel();
        self.webrtc_conn.close().await.map_err(|e| e.into())
    }
}

#[async_trait]
impl TransportInterface for WebrtcTransport {
    type Connection = WebrtcConnection;
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
        let ice_servers = self.ice_servers.iter().cloned().map(|x| x.into()).collect();

        let webrtc_config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        let mut setting = webrtc::api::setting_engine::SettingEngine::default();
        if let Some(ref addr) = self.external_address {
            tracing::debug!("setting external ip {:?}", addr);
            setting.set_nat_1to1_ips(vec![addr.to_string()], RTCIceCandidateType::Host);
            setting.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        } else {
            setting.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        }

        // A media track needs its codec registered in the media engine so the SDP can negotiate it;
        // the data-only path keeps the default (empty) engine, unchanged.
        let mut api_builder = webrtc::api::APIBuilder::new().with_setting_engine(setting);
        if let Some(media) = self.channel_config.media.as_ref() {
            let (capability, codec_type) = media_codec(media);
            let mut media_engine = MediaEngine::default();
            media_engine.register_codec(
                RTCRtpCodecParameters {
                    capability,
                    payload_type: media.payload_type,
                    stats_id: String::new(),
                },
                codec_type,
            )?;
            api_builder = api_builder.with_media_engine(media_engine);
        }
        let webrtc_api = api_builder.build();

        //
        // Create webrtc connection
        //
        let webrtc_conn: RTCPeerConnection = webrtc_api.new_peer_connection(webrtc_config).await?;

        //
        // Set callbacks
        //
        let webrtc_data_channel_state_notifier = Notifier::default();
        let inner_cb = Arc::new(InnerTransportCallback::new(
            cid,
            callback,
            webrtc_data_channel_state_notifier.clone(),
        ));

        let channel_pool = Arc::new(RoundRobinPool::default());
        let data_channel_inner_cb = inner_cb.clone();
        webrtc_conn.on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
            let d_label = d.label();
            let d_id = d.id();
            tracing::debug!("New DataChannel {d_label} {d_id}");
            // Open/close are detected on the channels we create (the pool, wired
            // below); a received channel only carries inbound messages. Wiring
            // open/close here too would fire on_data_channel_open twice (created
            // + received) and churn join_dht.
            let on_message_inner_cb = data_channel_inner_cb.clone();
            d.on_message(Box::new(move |msg: DataChannelMessage| {
                tracing::debug!(
                    "Received DataChannelMessage from {}: {:?}",
                    on_message_inner_cb.cid,
                    msg
                );

                let inner_cb = on_message_inner_cb.clone();

                Box::pin(async move {
                    inner_cb.on_message(&msg.data).await;
                })
            }));

            Box::pin(async move {})
        }));

        let peer_connection_state_change_inner_cb = inner_cb.clone();
        webrtc_conn.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            tracing::debug!("Peer Connection State has changed: {s:?}");

            let inner_cb = peer_connection_state_change_inner_cb.clone();

            Box::pin(async move {
                inner_cb.on_peer_connection_state_change(s.into()).await;
            })
        }));

        //
        // Inbound media: deliver each remote track to the callback as a `MediaTrack`. The
        // application reads media off it (`NativeRemoteTrack::read_rtp`). No-op for data-only peers,
        // which never fire `on_track`. Outbound tracks are added by the app via `add_media_track`.
        //
        let on_track_inner_cb = inner_cb.clone();
        webrtc_conn.on_track(Box::new(
            move |track: Arc<TrackRemote>, _receiver, _transceiver| {
                let inner_cb = on_track_inner_cb.clone();
                let kind = match track.kind() {
                    RTPCodecType::Audio => MediaKind::Audio,
                    _ => MediaKind::Video,
                };
                Box::pin(async move {
                    let remote = NativeRemoteTrack { track, kind };
                    inner_cb.on_media_track(Box::new(remote)).await;
                })
            },
        ));

        //
        // Create data channel
        //
        // Wire open/close on the channels *this* side creates (the pool), not
        // only on received channels: a received channel's `on_open` can be
        // missed if it opens before the handler is registered, which would mean
        // `on_data_channel_open` (and thus `join_dht`) never fires. The created
        // channels are registered before they can open, so this is reliable.
        for i in 0..DATA_CHANNEL_POOL_SIZE {
            let ch = webrtc_conn
                .create_data_channel(&format!("rings_data_channel_{i}"), None)
                .await?;

            let on_open_pool = channel_pool.clone();
            let on_open_cb = inner_cb.clone();
            ch.on_open(Box::new(move || {
                let pool = on_open_pool.clone();
                let cb = on_open_cb.clone();
                Box::pin(async move {
                    if let Ok(true) = pool.all_ready() {
                        cb.on_data_channel_open().await;
                    }
                })
            }));

            let on_close_pool = channel_pool.clone();
            let on_close_cb = inner_cb.clone();
            ch.on_close(Box::new(move || {
                let pool = on_close_pool.clone();
                let cb = on_close_cb.clone();
                Box::pin(async move {
                    if let Ok(true) =
                        pool.all(|(c, _, _)| c.ready_state() == RTCDataChannelState::Closed)
                    {
                        cb.on_data_channel_close().await;
                    }
                })
            }));

            channel_pool.push((ch, Arc::new(AtomicU64::new(0)), Arc::new(Mutex::new(()))))?;
        }

        //
        // Construct the Connection
        //
        let conn = WebrtcConnection::new(
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

impl From<IceServer> for RTCIceServer {
    fn from(s: IceServer) -> Self {
        // webrtc 0.17 dropped `credential_type` from `RTCIceServer` (only long-term credentials
        // remain); `IceServer::credential_type` is ignored here.
        Self {
            urls: s.urls,
            username: s.username,
            credential: s.credential,
        }
    }
}

impl From<RTCPeerConnectionState> for WebrtcConnectionState {
    fn from(s: RTCPeerConnectionState) -> Self {
        match s {
            RTCPeerConnectionState::Unspecified => Self::Unspecified,
            RTCPeerConnectionState::New => Self::New,
            RTCPeerConnectionState::Connecting => Self::Connecting,
            RTCPeerConnectionState::Connected => Self::Connected,
            RTCPeerConnectionState::Disconnected => Self::Disconnected,
            RTCPeerConnectionState::Failed => Self::Failed,
            RTCPeerConnectionState::Closed => Self::Closed,
        }
    }
}
