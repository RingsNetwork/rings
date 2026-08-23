use std::net::IpAddr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;
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

use crate::callback::admit_inbound_data_channel;
use crate::callback::InboundFrameCapacity;
use crate::callback::InnerTransportCallback;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::pool::MessageSenderPool;
use crate::core::pool::RoundRobin;
use crate::core::pool::RoundRobinPool;
use crate::core::pool::StatusPool;
use crate::core::transport::effective_max_message_size;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateCell;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::SendPermit;
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

#[cfg(test)]
const NATIVE_SEND_COMPLETION_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const NATIVE_SEND_COMPLETION_TIMEOUT: Duration = Duration::from_secs(25);

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

fn sdp_candidate_count(sdp: &str) -> usize {
    sdp.lines()
        .filter(|line| line.starts_with("a=candidate:"))
        .count()
}

fn external_address_candidates(external_address: Option<&str>) -> Vec<String> {
    let Some(external_address) = external_address else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for candidate in external_address.split(',') {
        let candidate = candidate.trim();
        if candidate.is_empty() || candidates.iter().any(|seen| seen == candidate) {
            continue;
        }
        candidates.push(candidate.to_string());
    }
    candidates
}

fn is_loopback_address(candidate: &str) -> bool {
    candidate
        .parse::<IpAddr>()
        .map(|addr| addr.is_loopback())
        .unwrap_or(false)
}

fn nat_1to1_host_candidates(candidates: &[String]) -> Vec<String> {
    candidates
        .iter()
        .filter(|candidate| !is_loopback_address(candidate))
        .cloned()
        .collect()
}

fn sdp_extra_host_candidates(candidates: &[String]) -> Vec<String> {
    candidates
        .iter()
        .filter(|candidate| is_loopback_address(candidate))
        .cloned()
        .collect()
}

fn duplicate_host_candidate_line(line: &str, extra_addresses: &[String]) -> Vec<String> {
    if !line.starts_with("a=candidate:") {
        return Vec::new();
    }

    let fields = line.split_whitespace().collect::<Vec<_>>();
    let Some(address) = fields.get(4) else {
        return Vec::new();
    };
    let Some(candidate_type_marker) = fields.get(6) else {
        return Vec::new();
    };
    let Some(candidate_type) = fields.get(7) else {
        return Vec::new();
    };
    if *candidate_type_marker != "typ" || *candidate_type != "host" {
        return Vec::new();
    }

    extra_addresses
        .iter()
        .filter(|candidate| candidate.as_str() != *address)
        .filter_map(|candidate| {
            let mut duplicate = fields
                .iter()
                .map(|field| field.to_string())
                .collect::<Vec<_>>();
            let address_slot = duplicate.get_mut(4)?;
            *address_slot = candidate.clone();
            Some(duplicate.join(" "))
        })
        .collect()
}

fn append_sdp_extra_host_candidates(sdp: String, extra_addresses: &[String]) -> String {
    if extra_addresses.is_empty() {
        return sdp;
    }

    let mut output = String::with_capacity(sdp.len());
    for segment in sdp.split_inclusive('\n') {
        output.push_str(segment);

        let line = segment.trim_end_matches(['\r', '\n']);
        let line_ending = if segment.ends_with("\r\n") {
            "\r\n"
        } else if segment.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        for duplicate in duplicate_host_candidate_line(line, extra_addresses) {
            output.push_str(&duplicate);
            output.push_str(line_ending);
        }
    }

    output
}

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

fn native_send_runtime() -> Result<tokio::runtime::Handle> {
    tokio::runtime::Handle::try_current().map_err(|_| Error::NativeSendRuntimeUnavailable)
}

async fn run_irrevocable_send<T>(
    runtime: &tokio::runtime::Handle,
    send: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    runtime
        .spawn(async move {
            tokio::time::timeout(NATIVE_SEND_COMPLETION_TIMEOUT, send)
                .await
                .map_err(|_| Error::NativeSendCompletionTimeout {
                    timeout_ms: NATIVE_SEND_COMPLETION_TIMEOUT.as_millis(),
                })?
        })
        .await
        .map_err(Error::NativeSendTask)?
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl MessageSenderPool<TrackedChannel> for RoundRobinPool<TrackedChannel> {
    type Message = TransportMessage;
    async fn send_with_permit(
        &self,
        msg: TransportMessage,
        permit: SendPermit,
    ) -> Result<DeliveryFuture> {
        let (channel, enqueued, send_lock) = self.select()?;
        let data = rings_codec::serialize(&msg).map(Bytes::from)?;
        let runtime = native_send_runtime()?;
        // Hold the per-channel lock across send + counter advance so the bytes
        // are enqueued and accounted in the same (FIFO) order: concurrent senders
        // can't interleave the yielding send and the counter update. Advance
        // `enqueued` ONLY after a successful send — otherwise a failed send would
        // leave the counter ahead of what was actually queued, making earlier
        // messages' delivery futures resolve early on phantom bytes.
        let guard = send_lock.lock_owned().await;
        if !permit.allows() {
            return Err(Error::SendPermitRevoked);
        }
        permit.mark_irrevocable();
        let send_channel = Arc::clone(&channel);
        let send_enqueued = Arc::clone(&enqueued);
        let end_offset = run_irrevocable_send(&runtime, async move {
            let _guard = guard;
            if let Err(error) = send_channel.send(&data).await {
                tracing::error!("{:?}, Data size: {:?}", error, data.len());
                return Err(error.into());
            }
            permit.mark_accepted();
            Ok(send_enqueued.fetch_add(data.len() as u64, Ordering::SeqCst) + data.len() as u64)
        })
        .await?;
        Ok(delivery_future(channel, enqueued, end_offset))
    }
}

impl StatusPool<TrackedChannel> for RoundRobinPool<TrackedChannel> {
    fn all_ready(&self) -> Result<bool> {
        self.all(|(c, _, _)| c.ready_state() == RTCDataChannelState::Open)
    }
}

#[cfg(test)]
mod send_cancellation_tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[tokio::test]
    async fn started_native_send_outlives_cancelled_caller() {
        let started = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let release = Arc::new(tokio::sync::Notify::new());
        let send = {
            let started = Arc::clone(&started);
            let completed = Arc::clone(&completed);
            let release = Arc::clone(&release);
            async move {
                started.store(true, Ordering::Release);
                release.notified().await;
                completed.store(true, Ordering::Release);
                Ok(())
            }
        };
        let runtime = native_send_runtime().expect("Tokio test runtime must be available");
        let caller = tokio::spawn(async move { run_irrevocable_send(&runtime, send).await });
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }

        caller.abort();
        release.notify_one();
        while !completed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }

        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn missing_runtime_returns_typed_error() {
        assert!(matches!(
            native_send_runtime(),
            Err(Error::NativeSendRuntimeUnavailable)
        ));
    }

    #[tokio::test]
    async fn native_send_task_preserves_join_error_source() {
        let runtime = native_send_runtime().expect("Tokio test runtime must be available");
        let error = run_irrevocable_send(&runtime, async {
            panic!("native send task panic witness");
            #[allow(unreachable_code)]
            Ok(())
        })
        .await
        .expect_err("panicking send task must fail");

        assert!(matches!(&error, Error::NativeSendTask(source) if source.is_panic()));
        assert!(std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<tokio::task::JoinError>())
            .is_some_and(tokio::task::JoinError::is_panic));
    }

    #[tokio::test]
    async fn irrevocable_native_send_has_a_completion_bound() {
        let runtime = native_send_runtime().expect("Tokio test runtime must be available");
        let error = run_irrevocable_send(&runtime, std::future::pending::<Result<()>>())
            .await
            .expect_err("an irrevocable send must not remain pending forever");

        assert!(matches!(
            error,
            Error::NativeSendCompletionTimeout { timeout_ms }
                if timeout_ms == NATIVE_SEND_COMPLETION_TIMEOUT.as_millis()
        ));
    }
}

/// A connection that implemented by webrtc-rs library.
/// Used for native environment.
pub struct WebrtcConnection {
    webrtc_conn: RTCPeerConnection,
    webrtc_data_channel: Arc<RoundRobinPool<TrackedChannel>>,
    webrtc_data_channel_state_notifier: Notifier,
    connection_state: ConnectionStateCell,
    cancel_token: CancellationToken,
    sdp_extra_host_candidates: Vec<String>,
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
    inbound_frames: Arc<InboundFrameCapacity>,
}

impl WebrtcConnection {
    fn new(
        webrtc_conn: RTCPeerConnection,
        webrtc_data_channel: Arc<RoundRobinPool<TrackedChannel>>,
        webrtc_data_channel_state_notifier: Notifier,
        connection_state: ConnectionStateCell,
        sdp_extra_host_candidates: Vec<String>,
    ) -> Self {
        Self {
            webrtc_conn,
            webrtc_data_channel,
            webrtc_data_channel_state_notifier,
            connection_state,
            cancel_token: CancellationToken::new(),
            sdp_extra_host_candidates,
            remote_max_message_size: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn webrtc_gather(
        &self,
        mut gathering_complete_promise: mpsc::Receiver<()>,
    ) -> Result<String> {
        let gathering_complete_promise_with_timeout = tokio::time::timeout(
            std::time::Duration::from_secs(WEBRTC_GATHER_TIMEOUT.into()),
            gathering_complete_promise.recv(),
        );

        tokio::select! {
            _ = self.cancel_token.cancelled() => {
                return Err(Error::WebrtcLocalSdpGenerationError("Local connection closed".to_string()))
            }
            result = gathering_complete_promise_with_timeout => {
                if result.is_err() {
                    return Err(Error::WebrtcLocalSdpGenerationError(format!(
                        "Webrtc gathering is not completed in {WEBRTC_GATHER_TIMEOUT} seconds"
                    )));
                }
            }
        }

        let sdp = self
            .webrtc_conn
            .local_description()
            .await
            .ok_or(Error::WebrtcLocalSdpGenerationError(
                "Failed to get local description".to_string(),
            ))?
            .sdp;
        let sdp = append_sdp_extra_host_candidates(sdp, &self.sdp_extra_host_candidates);

        let candidate_count = sdp_candidate_count(&sdp);
        if candidate_count == 0 {
            tracing::warn!(
                sdp_bytes = sdp.len(),
                "WebRTC local SDP has no ICE candidates after gathering"
            );
        } else {
            tracing::debug!(
                sdp_bytes = sdp.len(),
                candidate_count,
                "WebRTC ICE gathering complete"
            );
        }

        Ok(sdp)
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
            inbound_frames: Arc::new(InboundFrameCapacity::new()),
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

async fn create_peer_connection(
    transport: &WebrtcTransport,
) -> Result<(RTCPeerConnection, Vec<String>)> {
    let ice_servers = transport
        .ice_servers
        .iter()
        .cloned()
        .map(RTCIceServer::from)
        .collect();
    let configuration = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    let mut setting = webrtc::api::setting_engine::SettingEngine::default();
    set_udp_network_range(&mut setting, transport.udp_port_range)?;
    let external_addresses = external_address_candidates(transport.external_address.as_deref());
    let nat_host_candidates = nat_1to1_host_candidates(&external_addresses);
    let sdp_extra_host_candidates = sdp_extra_host_candidates(&external_addresses);
    setting.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
    if !nat_host_candidates.is_empty() {
        tracing::debug!(
            external_addresses = ?nat_host_candidates,
            "setting external WebRTC host candidates"
        );
        setting.set_nat_1to1_ips(nat_host_candidates, RTCIceCandidateType::Host);
    }
    if !sdp_extra_host_candidates.is_empty() {
        tracing::debug!(
            extra_host_candidates = ?sdp_extra_host_candidates,
            "will append SDP-only WebRTC host candidates"
        );
    }

    let api = webrtc::api::APIBuilder::new()
        .with_setting_engine(setting)
        .build();
    let connection = api.new_peer_connection(configuration).await?;
    Ok((connection, sdp_extra_host_candidates))
}

#[async_trait]
impl ConnectionInterface for WebrtcConnection {
    type Sdp = String;
    type Error = Error;

    async fn send_message_with_permit(
        &self,
        msg: TransportMessage,
        permit: SendPermit,
    ) -> Result<DeliveryFuture> {
        self.webrtc_wait_for_data_channel_open().await?;
        self.webrtc_data_channel.send_with_permit(msg, permit).await
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
        self.connection_state.snapshot().webrtc()
    }

    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
        self.connection_state.snapshot()
    }

    fn data_channel_is_open(&self) -> Result<bool> {
        Ok(self.connection_state.snapshot().data_channel_open())
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
        let gathering_complete_promise = self.webrtc_conn.gathering_complete_promise().await;
        self.webrtc_conn
            .set_local_description(setting_offer.clone())
            .await?;

        self.webrtc_gather(gathering_complete_promise).await
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
        let gathering_complete_promise = self.webrtc_conn.gathering_complete_promise().await;
        self.webrtc_conn
            .set_local_description(answer.clone())
            .await?;
        let local_sdp = self.webrtc_gather(gathering_complete_promise).await?;

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

        if self.data_channel_is_open()? {
            return Ok(());
        }

        self.webrtc_data_channel_state_notifier
            .set_timeout(WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT);
        self.webrtc_data_channel_state_notifier.clone().await;

        if self.data_channel_is_open()? {
            return Ok(());
        } else {
            return Err(Error::DataChannelOpen(format!(
                "DataChannel not open in {WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT} seconds"
            )));
        }
    }

    async fn close(&self) -> Result<()> {
        self.connection_state.close();
        self.cancel_token.cancel();
        self.webrtc_conn.close().await.map_err(|e| e.into())
    }
}

fn wire_received_data_channels(
    webrtc_conn: &RTCPeerConnection,
    inner_cb: Arc<InnerTransportCallback>,
    inbound_frames: Arc<InboundFrameCapacity>,
) {
    // Inbound channels carry messages only. One remote-created channel closing
    // does not prove the SCTP association is gone; outbound-pool state owns
    // readiness and emits the terminal data-channel callback when all close.
    let admitted_channels = AtomicUsize::new(0);
    webrtc_conn.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
        if !admit_inbound_data_channel(&admitted_channels) {
            tracing::warn!(
                peer = %inner_cb.cid,
                label = channel.label(),
                "rejected excess inbound data channel"
            );
            return Box::pin(async move {
                if let Err(error) = channel.close().await {
                    tracing::debug!(%error, "failed to close excess inbound data channel");
                }
            });
        }
        tracing::debug!(
            label = channel.label(),
            id = channel.id(),
            "new received data channel"
        );
        let message_cb = Arc::clone(&inner_cb);
        let frame_capacity = inbound_frames.clone();
        channel.on_message(Box::new(move |msg: DataChannelMessage| {
            tracing::debug!(
                peer = %message_cb.cid,
                is_string = msg.is_string,
                bytes = msg.data.len(),
                "received data-channel message"
            );
            if msg.data.len() > MAX_DATA_CHANNEL_MESSAGE_SIZE {
                tracing::warn!(
                    peer = %message_cb.cid,
                    bytes = msg.data.len(),
                    max_bytes = MAX_DATA_CHANNEL_MESSAGE_SIZE,
                    "rejected oversized data-channel message before dispatch"
                );
                return Box::pin(async {});
            }
            let Some(permit) = frame_capacity.try_acquire(msg.data.len()) else {
                tracing::warn!(
                    peer = %message_cb.cid,
                    bytes = msg.data.len(),
                    "rejected data-channel message before dispatch"
                );
                return Box::pin(async {});
            };
            let cb = Arc::clone(&message_cb);
            Box::pin(async move {
                cb.on_message(&msg.data).await;
                drop(permit);
            })
        }));
        Box::pin(async {})
    }));
}

fn wire_peer_connection_state(
    webrtc_conn: &RTCPeerConnection,
    inner_cb: Arc<InnerTransportCallback>,
    connection_state: ConnectionStateCell,
) {
    webrtc_conn.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        tracing::debug!("Peer Connection State has changed: {state:?}");
        let cb = Arc::clone(&inner_cb);
        let state_change = state.into();
        connection_state.observe_webrtc(state_change);
        Box::pin(async move { cb.on_peer_connection_state_change(state_change).await })
    }));
}

async fn create_outbound_data_channels(
    webrtc_conn: &RTCPeerConnection,
    channel_pool: &Arc<RoundRobinPool<TrackedChannel>>,
    inner_cb: &Arc<InnerTransportCallback>,
    connection_state: &ConnectionStateCell,
) -> Result<()> {
    for index in 0..DATA_CHANNEL_POOL_SIZE {
        let channel = webrtc_conn
            .create_data_channel(&format!("rings_data_channel_{index}"), None)
            .await?;
        let open_pool = Arc::clone(channel_pool);
        let open_cb = Arc::clone(inner_cb);
        let open_state = connection_state.clone();
        channel.on_open(Box::new(move || {
            let all_ready = matches!(open_pool.all_ready(), Ok(true));
            if all_ready {
                open_state.observe_outbound_data_channels(true);
            }
            let cb = Arc::clone(&open_cb);
            Box::pin(async move {
                if all_ready {
                    cb.on_data_channel_open().await;
                }
            })
        }));

        let close_pool = Arc::clone(channel_pool);
        let close_cb = Arc::clone(inner_cb);
        let close_state = connection_state.clone();
        channel.on_close(Box::new(move || {
            close_state.observe_outbound_data_channels(false);
            let all_closed = matches!(
                close_pool.all(|(candidate, _, _)| {
                    candidate.ready_state() == RTCDataChannelState::Closed
                }),
                Ok(true)
            );
            let cb = Arc::clone(&close_cb);
            Box::pin(async move {
                if all_closed {
                    cb.on_data_channel_close().await;
                }
            })
        }));

        channel_pool.push((
            channel,
            Arc::new(AtomicU64::new(0)),
            Arc::new(Mutex::new(())),
        ))?;
    }
    Ok(())
}

#[async_trait]
impl TransportInterface for WebrtcTransport {
    type Connection = WebrtcConnection;
    type Error = Error;

    async fn new_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> Result<ConnectionRef<Self::Connection>> {
        if let Ok(existed_conn) = self.pool.connection(cid) {
            if existed_conn.webrtc_connection_state().occupies_peer_slot() {
                return Err(Error::ConnectionAlreadyExists(cid.to_string()));
            }
        }

        let (webrtc_conn, sdp_extra_host_candidates) = create_peer_connection(self).await?;

        //
        // Set callbacks
        //
        let webrtc_data_channel_state_notifier = Notifier::default();
        let connection_state = ConnectionStateCell::new();
        let inner_cb = Arc::new(InnerTransportCallback::new(
            cid,
            callback,
            webrtc_data_channel_state_notifier.clone(),
        ));

        // Wire open/close on the channels *this* side creates (the pool), not
        // only on received channels: a received channel's `on_open` can be
        // missed if it opens before the handler is registered, which would mean
        // `on_data_channel_open` (and thus `join_dht`) never fires. The created
        // channels are registered before they can open, so this is reliable.
        let channel_pool = Arc::new(RoundRobinPool::default());
        wire_received_data_channels(
            &webrtc_conn,
            Arc::clone(&inner_cb),
            self.inbound_frames.clone(),
        );
        wire_peer_connection_state(
            &webrtc_conn,
            Arc::clone(&inner_cb),
            connection_state.clone(),
        );
        create_outbound_data_channels(&webrtc_conn, &channel_pool, &inner_cb, &connection_state)
            .await?;

        //
        // Construct the Connection
        //
        let conn = WebrtcConnection::new(
            webrtc_conn,
            channel_pool,
            webrtc_data_channel_state_notifier,
            connection_state,
            sdp_extra_host_candidates,
        );

        self.pool.safely_insert(cid, conn).await
    }

    async fn close_connection(&self, cid: &str) -> Result<()> {
        self.pool.safely_remove(cid).await
    }

    async fn close_connection_if_current(
        &self,
        connection: &ConnectionRef<Self::Connection>,
    ) -> Result<bool> {
        self.pool.safely_remove_if_current(connection).await
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

    #[test]
    fn external_address_candidates_split_trim_and_deduplicate() {
        let candidates = external_address_candidates(Some(" 127.0.0.1, 192.168.215.2,127.0.0.1, "));

        assert_eq!(candidates, vec![
            "127.0.0.1".to_string(),
            "192.168.215.2".to_string()
        ]);
    }

    #[test]
    fn external_address_candidates_ignore_blank_config() {
        assert!(external_address_candidates(Some("  ,  ")).is_empty());
        assert!(external_address_candidates(None).is_empty());
    }

    #[test]
    fn loopback_external_addresses_are_sdp_only_candidates() {
        let candidates = vec!["127.0.0.1".to_string(), "192.168.215.2".to_string()];

        assert_eq!(nat_1to1_host_candidates(&candidates), vec!["192.168.215.2"]);
        assert_eq!(sdp_extra_host_candidates(&candidates), vec!["127.0.0.1"]);
    }

    #[test]
    fn append_sdp_extra_host_candidates_duplicates_host_candidates() {
        let sdp = "v=0\r\n\
a=candidate:1 1 udp 2130706431 192.168.215.2 49160 typ host\r\n\
a=end-of-candidates\r\n"
            .to_string();

        let rewritten = append_sdp_extra_host_candidates(sdp, &["127.0.0.1".to_string()]);

        assert!(rewritten.contains(
            "a=candidate:1 1 udp 2130706431 192.168.215.2 49160 typ host\r\n\
a=candidate:1 1 udp 2130706431 127.0.0.1 49160 typ host\r\n"
        ));
        assert!(rewritten.ends_with("a=end-of-candidates\r\n"));
    }
}
