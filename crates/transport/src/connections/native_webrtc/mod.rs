use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Weak;
#[cfg(test)]
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::sync::watch;
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
use crate::core::pool::RoundRobin;
use crate::core::pool::RoundRobinPool;
use crate::core::transport::effective_max_message_size;
use crate::core::transport::stored_max_message_size;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateCell;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportInterface;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::core::transport::CONNECTION_RETIRE_TIMEOUT as NATIVE_CONNECTION_RETIRE_TIMEOUT;
use crate::core::transport::IRREVOCABLE_SEND_COMPLETION_TIMEOUT as NATIVE_SEND_COMPLETION_TIMEOUT;
use crate::delivery::tracker::BufferedChannel;
use crate::delivery::tracker::DeliveryTracker;
use crate::delivery::tracker::RoundLease;
use crate::delivery::DeliveryFuture;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::parse_ice_servers_or_warn;
use crate::ice_server::IceServer;
use crate::notifier::wait_for_data_channel_open;
use crate::notifier::Notifier;
use crate::pool::Pool;
use crate::webrtc_config::WebrtcUdpPortRange;

mod close_actor;
mod send_lifecycle;
#[cfg(test)]
use crate::core::send::model as send_model;
use crate::core::send::operation as send_operation;
mod send_runtime;
#[cfg(test)]
mod test_close_actor;
#[cfg(test)]
mod test_send_lifecycle;

#[cfg(test)]
use send_lifecycle::OwnedSend;
use send_lifecycle::SendLifecycle;
use send_operation::prepare_native;
use send_runtime::native_send_runtime;
use send_runtime::run_irrevocable_send;
#[cfg(test)]
use send_runtime::run_irrevocable_send_with_timeout;
use send_runtime::run_native_close_task;
use send_runtime::run_send_with_retirement;
use send_runtime::NativeRetirementFence;

const WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT: u8 = 8; // seconds
const WEBRTC_GATHER_TIMEOUT: u8 = 60; // seconds
/// pool size of data channel
const DATA_CHANNEL_POOL_SIZE: u8 = 4;

#[cfg(test)]
const NATIVE_SEND_TEST_COMPLETION_TIMEOUT: Duration = Duration::from_millis(100);

/// A data channel paired with its delivery tracker, plus a lock that
/// serializes sends. The tracker owns the monotonic counter of the total bytes
/// ever enqueued onto the channel, which lets the delivery future tell, per
/// message, whether the bytes have been flushed to the wire:
/// `enqueued_total - buffered_amount` is the number of bytes already handed
/// off, so a message whose end offset is below that has left the local send
/// buffer. The channel's `bufferedamountlow` event drives the tracker.
///
/// The lock is held across reserve+send so the reserved end offset always
/// matches the order bytes are actually enqueued in. Without it, two concurrent
/// senders could reserve offsets in one order but reach `channel.send().await`
/// (which yields) in the other, making an earlier future resolve against a
/// later message's bytes.
#[derive(Clone)]
struct TrackedChannel {
    channel: Arc<RTCDataChannel>,
    delivery: Arc<DeliveryTracker>,
    send_lock: Arc<Mutex<()>>,
}

impl BufferedChannel for RTCDataChannel {
    async fn arm_low_threshold(&self, threshold: u64) {
        // A threshold beyond the address space saturates: the event then
        // fires on any drain, which is a spurious (never a lost) wake-up.
        let threshold = usize::try_from(threshold).unwrap_or(usize::MAX);
        self.set_buffered_amount_low_threshold(threshold).await;
    }

    async fn observe_buffered(&self) -> u64 {
        // Saturating up is conservative: an oversized buffer never flushes.
        u64::try_from(self.buffered_amount().await).unwrap_or(u64::MAX)
    }
}

/// Run a delivery settle round on the current executor, detached from the
/// requester so that cancelling a send cannot interrupt an arm-and-read step.
fn spawn_delivery_round(lease: Option<RoundLease>, channel: Arc<RTCDataChannel>) {
    let Some(lease) = lease else {
        return;
    };
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn(async move { lease.run(channel.as_ref()).await });
        }
        // Dropping the lease returns the round to idle; the next event retries.
        Err(error) => tracing::error!(%error, "no executor for a delivery settle round"),
    }
}

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
/// first. Registering it re-arms the channel's low-water threshold.
fn delivery_future(
    channel: Arc<RTCDataChannel>,
    delivery: &Arc<DeliveryTracker>,
    end_offset: u64,
) -> DeliveryFuture {
    let (wait, lease) = delivery.track(end_offset);
    spawn_delivery_round(lease, channel);
    Box::pin(wait)
}

impl RoundRobinPool<TrackedChannel> {
    /// Serialize one channel operation and hand its resource owner to the bounded executor.
    async fn send_with_retirement_fence(
        &self,
        msg: TransportMessage,
        permit: SendPermit,
        retirement_fence: NativeRetirementFence,
        lifecycle: Arc<SendLifecycle>,
    ) -> Result<DeliveryFuture> {
        let TrackedChannel {
            channel,
            delivery,
            send_lock,
        } = self.select()?;
        let data = rings_codec::serialize(&msg).map(Bytes::from)?;
        let runtime = native_send_runtime()?;
        // Hold the per-channel lock across send + counter advance so the bytes
        // are enqueued and accounted in the same (FIFO) order: concurrent senders
        // can't interleave the yielding send and the counter update. Advance
        // `enqueued` ONLY after a successful send — otherwise a failed send would
        // leave the counter ahead of what was actually queued, making earlier
        // messages' delivery futures resolve early on phantom bytes.
        let guard = send_lock.lock_owned().await;
        let send_channel = Arc::clone(&channel);
        // The encoded size shares the existing u64 accounting domain.
        let data_len = u64::try_from(data.len()).map_err(|_| Error::SendByteCountOverflow)?;
        // The primitive owns its bytes and channel reference. The QueueSend
        // wrapper separately retains the serialization lease and permit proof.
        let primitive = async move {
            send_channel
                .send(&data)
                .await
                .inspect_err(|error| tracing::error!(%error, data_len, "native send failed"))
                .map(|_| ())
                .map_err(Error::from)
        };
        // Construct the complete resource owner before final admission. Its
        // first-poll boundary releases the gate before fencing a failure.
        let prepared = prepare_native(
            primitive,
            permit,
            guard,
            Arc::clone(delivery.enqueued()),
            data_len,
            lifecycle,
        )?;
        let admission = retirement_fence
            .try_send_admission()
            .ok_or(Error::SendPermitRevoked)?;
        let (send, first_poll) = prepared.start(admission);
        // The owner moves, unchanged, from inline first poll into its bounded
        // continuation. There is no permit-owned retirement closure.
        let end_offset = match first_poll {
            std::task::Poll::Ready(result) => result?,
            std::task::Poll::Pending => run_irrevocable_send(&runtime, send).await?,
        };
        Ok(delivery_future(channel, &delivery, end_offset))
    }
}

impl RoundRobinPool<TrackedChannel> {
    /// Return whether every channel in this backend pool is open.
    fn all_ready(&self) -> Result<bool> {
        self.all(|tracked| tracked.channel.ready_state() == RTCDataChannelState::Open)
    }
}

#[cfg(test)]
mod test_send_cancellation;

/// A connection that implemented by webrtc-rs library.
/// Used for native environment.
pub struct WebrtcConnection {
    webrtc_conn: Arc<RTCPeerConnection>,
    webrtc_data_channel: Arc<RoundRobinPool<TrackedChannel>>,
    webrtc_data_channel_state_notifier: Notifier,
    connection_state: ConnectionStateCell,
    cancel_token: CancellationToken,
    retirement_fence: NativeRetirementFence,
    sdp_extra_host_candidates: Vec<String>,
    /// Negotiated SCTP `max_message_size` (RFC 8841), parsed from the remote SDP at handshake.
    /// `0` means not yet negotiated. webrtc-rs exposes no getter, so we track it ourselves.
    remote_max_message_size: Arc<AtomicUsize>,
    physical_close_completed: PhysicalCloseCompletion,
}

/// Write side of [`NativePhysicalCloseWitness`]: the fact "`RTCPeerConnection::close()`
/// succeeded", published at most once and never retracted.
///
/// The cell is a watch channel, so the fact is a stored state rather than a pulse: a witness
/// subscribed before or after the publication observes it alike, and the channel closes when the
/// last holder (the connection or a detached close task) drops without publishing.
#[derive(Clone)]
struct PhysicalCloseCompletion(Arc<watch::Sender<bool>>);

impl PhysicalCloseCompletion {
    /// A cell that has not observed a successful close.
    fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    /// Record that the physical close succeeded; idempotent.
    fn publish(&self) {
        self.0.send_replace(true);
    }

    /// Subscribe a read-only witness to this cell.
    fn witness(&self) -> NativePhysicalCloseWitness {
        NativePhysicalCloseWitness {
            completed: self.0.subscribe(),
        }
    }
}

/// Stable observation that the native peer connection's close future completed successfully.
#[derive(Clone)]
pub struct NativePhysicalCloseWitness {
    completed: watch::Receiver<bool>,
}

impl NativePhysicalCloseWitness {
    /// Return true only after the underlying `RTCPeerConnection::close()` future succeeds.
    pub fn is_complete(&self) -> bool {
        *self.completed.borrow()
    }

    /// Await the close outcome: `true` once `RTCPeerConnection::close()` has succeeded, `false`
    /// once every holder of the connection released it without a successful close.
    ///
    /// This is supported API: the awaitable form of [`Self::is_complete`] for any caller that
    /// must act after the physical close, rather than polling for it.
    ///
    /// Law (no lost wake-up): `wait_for` tests the stored value before it suspends, and the
    /// value is monotone (`false → true` only), so a publication that precedes this call is
    /// observed as readily as one that follows it.
    pub async fn completed(mut self) -> bool {
        self.completed
            .wait_for(|completed| *completed)
            .await
            .is_ok()
    }
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
        let cancel_token = CancellationToken::new();
        let retirement_fence =
            NativeRetirementFence::new(connection_state.clone(), cancel_token.clone());
        Self {
            webrtc_conn: Arc::new(webrtc_conn),
            webrtc_data_channel,
            webrtc_data_channel_state_notifier,
            connection_state,
            cancel_token,
            retirement_fence,
            sdp_extra_host_candidates,
            remote_max_message_size: Arc::new(AtomicUsize::new(0)),
            physical_close_completed: PhysicalCloseCompletion::new(),
        }
    }

    pub(crate) fn physical_close_witness(&self) -> NativePhysicalCloseWitness {
        self.physical_close_completed.witness()
    }

    fn request_close(&self) {
        self.retirement_fence.request();
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
        let runtime = native_send_runtime()?;
        let acceptance = permit.acceptance();
        let pool = self.webrtc_data_channel.clone();
        let connection = self.webrtc_conn.clone();
        let physical_close_completed = self.physical_close_completed.clone();
        let retirement_fence = self.retirement_fence.clone();
        run_send_with_retirement(
            &runtime,
            acceptance,
            retirement_fence.clone(),
            move |lifecycle| async move {
                pool.send_with_retirement_fence(msg, permit, retirement_fence, lifecycle)
                    .await
            },
            close_failed_native_send(connection, physical_close_completed),
        )
        .await
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
        stored_max_message_size(self.remote_max_message_size.load(Ordering::SeqCst))
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
        wait_for_data_channel_open(
            self.webrtc_connection_state(),
            || self.data_channel_is_open(),
            &self.webrtc_data_channel_state_notifier,
            WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT,
        )
        .await
    }

    async fn close(&self) -> Result<()> {
        self.request_close();
        close_native_connection(
            self.webrtc_conn.clone(),
            self.physical_close_completed.clone(),
        )
        .await
    }
}

async fn close_native_connection(
    connection: Arc<RTCPeerConnection>,
    physical_close_completed: PhysicalCloseCompletion,
) -> Result<()> {
    let runtime = native_send_runtime()?;
    run_native_close_with_witness(
        &runtime,
        async move { connection.close().await.map_err(Into::into) },
        physical_close_completed,
    )
    .await
}

async fn run_native_close_with_witness(
    runtime: &tokio::runtime::Handle,
    close: impl Future<Output = Result<()>> + Send + 'static,
    physical_close_completed: PhysicalCloseCompletion,
) -> Result<()> {
    run_native_close_task(runtime, async move {
        let result = close.await;
        if result.is_ok() {
            physical_close_completed.publish();
        }
        result
    })
    .await
}

/// Close a generation already fenced by its send lifecycle, with bounded waiting.
/// The detached close task retains the separate physical-completion witness.
async fn close_failed_native_send(
    connection: Arc<RTCPeerConnection>,
    physical_close_completed: PhysicalCloseCompletion,
) -> Result<()> {
    tokio::time::timeout(
        NATIVE_CONNECTION_RETIRE_TIMEOUT,
        close_native_connection(connection, physical_close_completed),
    )
    .await
    .map_err(|_| Error::NativeConnectionRetirementTimeout {
        timeout_ms: NATIVE_CONNECTION_RETIRE_TIMEOUT.as_millis(),
    })?
}

fn wire_received_data_channels(
    webrtc_conn: &RTCPeerConnection,
    inner_cb: Arc<InnerTransportCallback>,
) {
    // Inbound channels carry messages only. One remote-created channel closing
    // does not prove the SCTP association is gone; outbound-pool state owns
    // readiness and emits the terminal data-channel callback when all close.
    let admitted_channels = AtomicUsize::new(0);
    webrtc_conn.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
        if !admit_inbound_data_channel(&admitted_channels) {
            tracing::warn!(
                peer = %inner_cb.cid(),
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
        channel.on_message(Box::new(move |msg: DataChannelMessage| {
            let bytes = msg.data.len();
            tracing::debug!(
                peer = %message_cb.cid(),
                is_string = msg.is_string,
                bytes,
                "received data-channel message"
            );
            let Some(frame) = message_cb.prepare_inbound_frame(msg.data) else {
                return Box::pin(async {});
            };
            let cb = Arc::clone(&message_cb);
            Box::pin(async move { cb.handle_admitted_frame(frame).await })
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

/// Drive a channel's delivery tracker from its `bufferedamountlow` and `error`
/// events. The handlers hold the channel weakly: the channel owns them.
async fn wire_delivery_events(channel: &Arc<RTCDataChannel>, delivery: &Arc<DeliveryTracker>) {
    let low_channel: Weak<RTCDataChannel> = Arc::downgrade(channel);
    let low_delivery = Arc::clone(delivery);
    channel
        .on_buffered_amount_low(Box::new(move || {
            if let Some(channel) = low_channel.upgrade() {
                spawn_delivery_round(low_delivery.notify(), channel);
            }
            Box::pin(async {})
        }))
        .await;

    let error_delivery = Arc::clone(delivery);
    channel.on_error(Box::new(move |error| {
        tracing::warn!(%error, "data channel failed; pending deliveries are lost");
        error_delivery.close();
        Box::pin(async {})
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

        let delivery = Arc::new(DeliveryTracker::default());
        wire_delivery_events(&channel, &delivery).await;

        let close_pool = Arc::clone(channel_pool);
        let close_cb = Arc::clone(inner_cb);
        let close_state = connection_state.clone();
        let close_delivery = Arc::clone(&delivery);
        channel.on_close(Box::new(move || {
            close_delivery.close();
            close_state.observe_outbound_data_channels(false);
            let all_closed = matches!(
                close_pool.all(|tracked| {
                    tracked.channel.ready_state() == RTCDataChannelState::Closed
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

        channel_pool.push(TrackedChannel {
            channel,
            delivery,
            send_lock: Arc::new(Mutex::new(())),
        })?;
    }
    Ok(())
}

#[async_trait]
impl TransportInterface for WebrtcTransport {
    type Connection = WebrtcConnection;
    type Error = Error;

    fn inbound_frame_capacity(&self) -> &Arc<InboundFrameCapacity> {
        &self.inbound_frames
    }

    async fn new_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> Result<ConnectionRef<Self::Connection>> {
        self.pool.ensure_peer_slot_available(cid)?;

        let (webrtc_conn, sdp_extra_host_candidates) = create_peer_connection(self).await?;

        //
        // Set callbacks
        //
        let webrtc_data_channel_state_notifier = Notifier::default();
        let connection_state = ConnectionStateCell::new();
        let inner_cb = Arc::new(InnerTransportCallback::for_transport(
            self,
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
        wire_received_data_channels(&webrtc_conn, Arc::clone(&inner_cb));
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

    async fn close_connection_if_current(
        &self,
        connection: &ConnectionRef<Self::Connection>,
    ) -> Result<bool> {
        self.pool.safely_remove_if_current(connection).await
    }

    fn connection(&self, cid: &str) -> Result<ConnectionRef<Self::Connection>> {
        self.pool.connection(cid)
    }

    fn connection_ids(&self) -> Vec<String> {
        self.pool.connection_ids()
    }
}

impl From<IceServer> for RTCIceServer {
    fn from(s: IceServer) -> Self {
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
mod test_delivery_loopback;
#[cfg(test)]
mod test_native_webrtc;
