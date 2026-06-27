use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice::udp_network::EphemeralUDP;
use webrtc::ice::udp_network::UDPNetwork;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::callback::InnerTransportCallback;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
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
use crate::ice_server::parse_ice_servers_or_warn;
use crate::ice_server::IceCredentialType;
use crate::ice_server::IceServer;
use crate::notifier::Notifier;
use crate::pool::Pool;
use crate::webrtc_config::WebrtcUdpPortRange;

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
}

/// [WebrtcTransport] manages all the [WebrtcConnection] and
/// provides methods to create, get and close connections.
pub struct WebrtcTransport {
    ice_servers: Vec<IceServer>,
    external_address: Option<String>,
    udp_port_range: Option<WebrtcUdpPortRange>,
    pool: Pool<WebrtcConnection>,
}

impl WebrtcConnection {
    fn new(
        webrtc_conn: RTCPeerConnection,
        webrtc_data_channel: Arc<RoundRobinPool<TrackedChannel>>,
        webrtc_data_channel_state_notifier: Notifier,
    ) -> Self {
        Self {
            webrtc_conn,
            webrtc_data_channel,
            webrtc_data_channel_state_notifier,
            cancel_token: CancellationToken::new(),
            remote_max_message_size: Arc::new(AtomicUsize::new(0)),
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
    /// Create a new [WebrtcTransport] instance.
    pub fn new(
        ice_servers: &str,
        external_address: Option<String>,
        udp_port_range: Option<WebrtcUdpPortRange>,
    ) -> Self {
        let ice_servers = parse_ice_servers_or_warn(ice_servers, "native-webrtc");

        Self {
            ice_servers,
            external_address,
            udp_port_range,
            pool: Pool::new(),
        }
    }
}

fn ephemeral_udp_for_range(range: WebrtcUdpPortRange) -> Result<EphemeralUDP> {
    EphemeralUDP::new(range.min(), range.max()).map_err(|e| {
        Error::WebrtcUdpPortRange(format!(
            "min={}, max={}, reason={e}",
            range.min(),
            range.max()
        ))
    })
}

fn set_udp_network_range(
    setting: &mut webrtc::api::setting_engine::SettingEngine,
    range: Option<WebrtcUdpPortRange>,
) -> Result<()> {
    if let Some(range) = range {
        setting.set_udp_network(UDPNetwork::Ephemeral(ephemeral_udp_for_range(range)?));
    }
    Ok(())
}

#[async_trait]
impl ConnectionInterface for WebrtcConnection {
    type Sdp = String;
    type Error = Error;

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

    async fn webrtc_create_offer(&self) -> Result<Self::Sdp> {
        let setting_offer = self.webrtc_conn.create_offer(None).await?;
        self.webrtc_conn
            .set_local_description(setting_offer.clone())
            .await?;

        self.webrtc_gather().await
    }

    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp> {
        tracing::debug!("webrtc_answer_offer, offer: {offer:?}");
        // Read the negotiated limit from the SDP text, but record it only after the *whole* answer
        // path (create_answer + set_local_description + gather) has succeeded, so a failure midway
        // does not leave a partially-updated connection carrying a stale negotiated size.
        let negotiated_max_message_size = effective_max_message_size(&offer);
        let offer = RTCSessionDescription::offer(offer)?;
        self.webrtc_conn.set_remote_description(offer).await?;

        let answer = self.webrtc_conn.create_answer(None).await?;
        self.webrtc_conn
            .set_local_description(answer.clone())
            .await?;
        let local_sdp = self.webrtc_gather().await?;

        self.remote_max_message_size
            .store(negotiated_max_message_size, Ordering::SeqCst);
        Ok(local_sdp)
    }

    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<()> {
        tracing::debug!("webrtc_accept_answer, answer: {answer:?}");
        let negotiated_max_message_size = effective_max_message_size(&answer);
        let answer = RTCSessionDescription::answer(answer)?;
        self.webrtc_conn.set_remote_description(answer).await?;
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
        set_udp_network_range(&mut setting, self.udp_port_range)?;
        if let Some(ref addr) = self.external_address {
            tracing::debug!("setting external ip {:?}", addr);
            setting.set_nat_1to1_ips(vec![addr.to_string()], RTCIceCandidateType::Host);
            setting.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        } else {
            setting.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        }

        let webrtc_api = webrtc::api::APIBuilder::new()
            .with_setting_engine(setting)
            .build();

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
        // webrtc 0.17 dropped `credential_type` from `RTCIceServer` (only long-term/password
        // credentials remain). Password creds are carried as-is; an OAuth credential cannot be
        // expressed, so warn rather than silently degrade an explicitly-configured one.
        if s.credential_type == IceCredentialType::Oauth {
            tracing::warn!(
                "ICE server {:?} configured with OAuth credentials, which webrtc 0.17 does not \
                 support; falling back to long-term credential fields",
                s.urls
            );
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_test_range() -> WebrtcUdpPortRange {
        match WebrtcUdpPortRange::new(49160, 49200) {
            Ok(range) => range,
            Err(error) => panic!("valid range rejected: {error}"),
        }
    }

    #[test]
    fn native_udp_range_builds_ephemeral_udp_with_same_bounds() {
        let udp = ephemeral_udp_for_range(valid_test_range());
        let udp = match udp {
            Ok(udp) => udp,
            Err(error) => panic!("valid range rejected by ICE stack: {error}"),
        };

        assert_eq!(udp.port_min(), 49160);
        assert_eq!(udp.port_max(), 49200);
    }

    #[test]
    fn native_transport_keeps_configured_udp_range() {
        let range = valid_test_range();
        let transport = WebrtcTransport::new("", None, Some(range));

        assert_eq!(transport.udp_port_range, Some(range));
    }
}
