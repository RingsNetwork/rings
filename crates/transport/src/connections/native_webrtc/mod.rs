use std::any::Any;
use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
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
use crate::callback::credit_pump::check_idle_lanes;
use crate::callback::credit_pump::pump_lane_credits;
use crate::callback::credit_pump::CREDIT_RESEND_PAUSE;
use crate::callback::data_channel_label;
use crate::callback::link_credit::LANE_IDLE_PERIOD;
use crate::callback::link_send::FrameSink;
use crate::callback::InnerTransportCallback;
use crate::callback::NodeFramePool;
use crate::connection_pool::Pool;
use crate::connection_ref::ConnectionRef;
use crate::connections::NativePhysicalCloseWitness;
use crate::core::callback::BoxedTransportCallback;
use crate::core::credit::CreditIndex;
use crate::core::pool::ChannelLane;
use crate::core::pool::ChannelPool;
use crate::core::pool::LanePool;
use crate::core::pool::CHANNEL_POOL_SIZE;
use crate::core::transport::stored_max_message_size;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateCell;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::HandshakeStep;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportConnectionState;
use crate::core::transport::TransportInterface;
use crate::core::transport::TransportMessage;
use crate::core::transport::CONNECTION_RETIRE_TIMEOUT as NATIVE_CONNECTION_RETIRE_TIMEOUT;
use crate::core::transport::IRREVOCABLE_SEND_COMPLETION_TIMEOUT as NATIVE_SEND_COMPLETION_TIMEOUT;
use crate::delivery::tracker::BufferedChannel;
use crate::delivery::tracker::DeliveryTracker;
use crate::delivery::tracker::RoundLease;
use crate::delivery::DeliveryFuture;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::parse_ice_servers_or_warn;
use crate::ice_server::parse_ice_servers_strict;
use crate::ice_server::IceServer;
use crate::network_config::UdpPortRange;
use crate::notifier::wait_for_data_channel_open;
use crate::notifier::Notifier;
use crate::sdp::effective_max_message_size;
use crate::sdp::split_sdp_remote_candidates;
use crate::sync_utils::lock_recover;

mod close_actor;
mod host_candidates;
mod send_lifecycle;
#[cfg(test)]
use crate::core::send::model as send_model;
use crate::core::send::operation as send_operation;
mod send_runtime;
#[cfg(test)]
mod test_close_actor;
#[cfg(test)]
mod test_send_lifecycle;
mod turn_tcp;

use host_candidates::append_sdp_extra_host_candidates;
use host_candidates::external_address_candidates;
use host_candidates::is_unrouted_container_bridge_interface;
use host_candidates::nat_1to1_host_candidates;
use host_candidates::sdp_candidate_count;
use host_candidates::sdp_extra_host_candidates;
#[cfg(test)]
use send_lifecycle::OwnedSend;
use send_lifecycle::SendLifecycle;
use send_operation::prepare_native;
use send_runtime::native_close_task_stopped;
use send_runtime::native_send_runtime;
use send_runtime::run_irrevocable_send;
#[cfg(test)]
use send_runtime::run_irrevocable_send_with_timeout;
use send_runtime::run_send_with_retirement;
use send_runtime::NativeRetirementFence;
use turn_tcp::prepare_native_ice_config;
use turn_tcp::TurnTcpBridgeGuard;

const WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT: u8 = 8; // seconds
const WEBRTC_GATHER_TIMEOUT: u8 = 60; // seconds
/// Delay before the responder starts outbound ICE connectivity checks in [`WebrtcConnection::answer_offer`]
/// after local candidate gathering finishes, giving the out-of-band HTTP `answerOffer` response
/// (~300–350 ms RTT) time to reach the browser and apply `setRemoteDescription(answer)` before the
/// seed's UDP STUN Binding Requests hit the client router's NAT port (#1290).
const WEBRTC_ANSWER_CHECK_START_DELAY: std::time::Duration = std::time::Duration::from_millis(450);

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
        // Never saturates: a round arms τ = E ⊖ e_min ≤ E, and the bytes of the
        // pending sends are counted in a usize `bufferedAmount`, so τ < b ≤
        // usize::MAX whenever a round waits for the event. Saturating keeps the
        // conversion total.
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
        // Unreachable from the send path, which requires the executor. Dropping
        // the lease returns the round to idle, and only a later registration
        // is sure to start the next one: no event may follow the stale arm.
        Err(error) => tracing::error!(%error, "no executor for a delivery settle round"),
    }
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

impl ChannelPool<TrackedChannel> {
    /// Serialize one channel operation and hand its resource owner to the bounded executor.
    async fn send_with_retirement_fence(
        &self,
        frame: TransportMessage,
        lane: ChannelLane,
        permit: SendPermit,
        retirement_fence: NativeRetirementFence,
        lifecycle: Arc<SendLifecycle>,
    ) -> Result<DeliveryFuture> {
        let TrackedChannel {
            channel,
            delivery,
            send_lock,
        } = self.select(lane)?;
        let data = rings_codec::serialize(&frame).map(Bytes::from)?;
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

impl ChannelPool<TrackedChannel> {
    /// Return whether every channel in this backend pool is open.
    fn all_ready(&self) -> Result<bool> {
        self.all(|tracked| tracked.channel.ready_state() == RTCDataChannelState::Open)
    }
}

#[cfg(test)]
mod test_send_cancellation;

const HANDSHAKE_ROLE_FRESH: u8 = 0;
const HANDSHAKE_ROLE_INITIATOR_AWAITING_ANSWER: u8 = 1;
const HANDSHAKE_ROLE_ESTABLISHED: u8 = 2;
const HANDSHAKE_ROLE_IN_PROGRESS: u8 = 3;
const HANDSHAKE_ROLE_FAILED: u8 = 4;

struct HandshakeRoleGuard<'a> {
    role: &'a AtomicU8,
    committed: bool,
}

impl<'a> HandshakeRoleGuard<'a> {
    fn new(role: &'a AtomicU8) -> Self {
        Self {
            role,
            committed: false,
        }
    }

    fn commit(mut self, next_role: u8) {
        self.role.store(next_role, Ordering::Release);
        self.committed = true;
    }
}

impl Drop for HandshakeRoleGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.role.store(HANDSHAKE_ROLE_FAILED, Ordering::Release);
        }
    }
}

/// A connection that implemented by webrtc-rs library.
/// Used for native environment.
pub struct WebrtcConnection {
    webrtc_conn: Arc<std::sync::RwLock<Arc<RTCPeerConnection>>>,
    configured_ice_servers: std::sync::RwLock<Vec<IceServer>>,
    transport_ice_state: Arc<std::sync::RwLock<(String, Vec<IceServer>)>>,
    external_address: Option<String>,
    udp_port_range: Option<UdpPortRange>,
    negotiation_started: AtomicBool,
    handshake_role: AtomicU8,
    reconfigure_lock: Arc<Mutex<()>>,
    webrtc_data_channel: Arc<ChannelPool<TrackedChannel>>,
    webrtc_data_channel_state_notifier: Notifier,
    connection_state: ConnectionStateCell,
    cancel_token: CancellationToken,
    retirement_fence: NativeRetirementFence,
    sdp_extra_host_candidates: std::sync::RwLock<Vec<String>>,
    turn_tcp_bridge: Arc<std::sync::Mutex<Option<TurnTcpBridgeGuard>>>,
    /// Negotiated SCTP `max_message_size` (RFC 8841), parsed from the remote SDP at handshake.
    /// `0` means not yet negotiated. webrtc-rs exposes no getter, so we track it ourselves.
    remote_max_message_size: Arc<AtomicUsize>,
    physical_close_completed: PhysicalCloseCompletion,
    /// The connection's callback: its credit gates every custom send.
    callback: Arc<InnerTransportCallback>,
}

/// Shared error representation for coalesced native physical-close waiters.
///
/// The first waiter takes the original [`Error`] directly when available; any concurrent or
/// later waiter reconstructs an equivalent typed [`Error`] without requiring [`Error`] to be
/// globally `Clone`.
#[derive(Clone)]
struct SharedCloseFailure {
    primary: Arc<std::sync::Mutex<Option<Error>>>,
    webrtc_error: Option<String>,
    timeout_ms: Option<u128>,
    fallback_message: String,
}

impl SharedCloseFailure {
    fn from_error(error: Error) -> Self {
        let webrtc_error = match &error {
            Error::Webrtc(err) => Some(err.to_string()),
            _ => None,
        };
        let timeout_ms = match &error {
            Error::NativeConnectionRetirementTimeout { timeout_ms } => Some(*timeout_ms),
            _ => None,
        };
        let fallback_message = error.to_string();
        Self {
            primary: Arc::new(std::sync::Mutex::new(Some(error))),
            webrtc_error,
            timeout_ms,
            fallback_message,
        }
    }

    fn into_error(self) -> Error {
        if let Some(error) = lock_recover(&self.primary).take() {
            return error;
        }
        if let Some(error) = self.webrtc_error {
            return Error::Webrtc(webrtc::error::Error::new(error));
        }
        if let Some(timeout_ms) = self.timeout_ms {
            return Error::NativeConnectionRetirementTimeout { timeout_ms };
        }
        Error::MessageNotDelivered(self.fallback_message)
    }
}

struct SharedCloseTask {
    outcome_rx: watch::Receiver<Option<std::result::Result<(), SharedCloseFailure>>>,
    join_handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

struct PhysicalCloseState {
    completed: Arc<watch::Sender<bool>>,
    task: std::sync::Mutex<Option<SharedCloseTask>>,
}

/// Write side of [`NativePhysicalCloseWitness`] and single-flight coordinator for one native
/// connection generation's physical close.
///
/// The cell owns both:
/// - a watch channel storing the monotone fact "`RTCPeerConnection::close()` succeeded", so a
///   witness subscribed before or after publication observes it alike and the channel closes
///   when the last holder (the connection or the detached close task) drops without publishing;
/// - at most one detached physical-close task per connection generation, shared by explicit
///   `close()` and failed-send retirement callers so concurrent closes never invoke
///   `RTCPeerConnection::close()` a second time or publish completion before the original
///   cleanup finishes.
#[derive(Clone)]
struct PhysicalCloseCompletion(Arc<PhysicalCloseState>);

impl PhysicalCloseCompletion {
    /// A cell that has not observed or started a physical close.
    fn new() -> Self {
        Self(Arc::new(PhysicalCloseState {
            completed: Arc::new(watch::channel(false).0),
            task: std::sync::Mutex::new(None),
        }))
    }

    /// Subscribe a read-only witness to this cell.
    fn witness(&self) -> NativePhysicalCloseWitness {
        NativePhysicalCloseWitness::new(self.0.completed.subscribe())
    }

    /// Run `close` at most once for this connection generation and await the shared outcome.
    ///
    /// Law (single physical close): the first caller spawns one detached close task on `runtime`;
    /// concurrent or later callers drop their candidate close future without polling it and join
    /// the original task's outcome. [`NativePhysicalCloseWitness`] is published only by that
    /// task after the original close future returns `Ok(())`, before waking waiters.
    async fn run_close(
        self,
        runtime: &tokio::runtime::Handle,
        close: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Result<()> {
        let mut unused_close = None;
        let (mut outcome_rx, join_handle) = {
            let mut task_slot = lock_recover(&self.0.task);
            if let Some(existing) = task_slot.as_ref() {
                unused_close = Some(close);
                (
                    existing.outcome_rx.clone(),
                    Arc::clone(&existing.join_handle),
                )
            } else {
                let (outcome_tx, outcome_rx) = watch::channel(None);
                let completed = Arc::clone(&self.0.completed);
                let join_handle = runtime.spawn(async move {
                    let outcome = match close.await {
                        Ok(()) => {
                            completed.send_replace(true);
                            Ok(())
                        }
                        Err(error) => Err(SharedCloseFailure::from_error(error)),
                    };
                    outcome_tx.send_replace(Some(outcome));
                });
                let join_handle = Arc::new(std::sync::Mutex::new(Some(join_handle)));
                *task_slot = Some(SharedCloseTask {
                    outcome_rx: outcome_rx.clone(),
                    join_handle: Arc::clone(&join_handle),
                });
                (outcome_rx, join_handle)
            }
        };
        drop(unused_close);
        drop(self);

        let _ = outcome_rx.wait_for(Option::is_some).await;
        let maybe_outcome = outcome_rx.borrow().clone();
        match maybe_outcome {
            Some(Ok(())) => Ok(()),
            Some(Err(failure)) => Err(failure.into_error()),
            None => {
                let handle = lock_recover(&join_handle).take();
                Err(native_close_task_stopped(runtime, handle).await)
            }
        }
    }
}

/// [WebrtcTransport] manages all the [WebrtcConnection] and
/// provides methods to create, get and close connections.
///
/// Both the raw credential string and its parsed [`IceServer`] list live inside a single
/// [`std::sync::RwLock`] so concurrent readers never observe torn `(raw, parsed)` state across a
/// credential rotation.
pub struct WebrtcTransport {
    ice_state: Arc<std::sync::RwLock<(String, Vec<IceServer>)>>,
    external_address: Option<String>,
    udp_port_range: Option<UdpPortRange>,
    pool: Pool<WebrtcConnection>,
    /// The frames this node lends its connections' lanes, shared by every connection.
    frame_pool: NodeFramePool,
    /// Captured Tokio runtime handle used when [`Self::commit_prepared_ice_servers`] is called
    /// from an external non-Tokio thread (such as a synchronous FFI caller).
    runtime_handle: std::sync::RwLock<Option<tokio::runtime::Handle>>,
}

impl WebrtcConnection {
    #[allow(clippy::too_many_arguments)]
    fn new(
        webrtc_conn: RTCPeerConnection,
        configured_ice_servers: Vec<IceServer>,
        transport_ice_state: Arc<std::sync::RwLock<(String, Vec<IceServer>)>>,
        external_address: Option<String>,
        udp_port_range: Option<UdpPortRange>,
        webrtc_data_channel: Arc<ChannelPool<TrackedChannel>>,
        webrtc_data_channel_state_notifier: Notifier,
        connection_state: ConnectionStateCell,
        sdp_extra_host_candidates: Vec<String>,
        turn_tcp_bridge: Option<TurnTcpBridgeGuard>,
        callback: Arc<InnerTransportCallback>,
    ) -> Self {
        let cancel_token = CancellationToken::new();
        let retirement_fence =
            NativeRetirementFence::new(connection_state.clone(), cancel_token.clone());
        Self {
            webrtc_conn: Arc::new(std::sync::RwLock::new(Arc::new(webrtc_conn))),
            configured_ice_servers: std::sync::RwLock::new(configured_ice_servers),
            transport_ice_state,
            external_address,
            udp_port_range,
            negotiation_started: AtomicBool::new(false),
            handshake_role: AtomicU8::new(HANDSHAKE_ROLE_FRESH),
            reconfigure_lock: Arc::new(Mutex::new(())),
            webrtc_data_channel,
            webrtc_data_channel_state_notifier,
            connection_state,
            cancel_token,
            retirement_fence,
            sdp_extra_host_candidates: std::sync::RwLock::new(sdp_extra_host_candidates),
            turn_tcp_bridge: Arc::new(std::sync::Mutex::new(turn_tcp_bridge)),
            remote_max_message_size: Arc::new(AtomicUsize::new(0)),
            physical_close_completed: PhysicalCloseCompletion::new(),
            callback,
        }
    }

    fn webrtc_conn(&self) -> Arc<RTCPeerConnection> {
        Arc::clone(
            &self
                .webrtc_conn
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    async fn refresh_ice_servers(&self) -> Result<()> {
        let _guard = self.reconfigure_lock.lock().await;
        self.refresh_ice_servers_locked().await
    }

    async fn refresh_ice_servers_locked(&self) -> Result<()> {
        if self.cancel_token.is_cancelled()
            || self.connection_state.snapshot().state() == TransportConnectionState::Closed
        {
            return Ok(());
        }

        let desired = self
            .transport_ice_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1
            .clone();
        let already_configured = *self
            .configured_ice_servers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            == desired;
        if already_configured {
            return Ok(());
        }

        let current_conn = self.webrtc_conn();
        let is_unnegotiated = !self.negotiation_started.load(Ordering::Acquire)
            && current_conn.local_description().await.is_none()
            && current_conn.remote_description().await.is_none()
            && self.connection_state.snapshot().state() == TransportConnectionState::New;

        if is_unnegotiated {
            let (new_webrtc_conn, new_extra_host_candidates, new_turn_tcp_bridge) =
                create_peer_connection_with_config(
                    &desired,
                    self.external_address.as_deref(),
                    self.udp_port_range,
                )
                .await?;
            wire_received_data_channels(&new_webrtc_conn, Arc::clone(&self.callback));
            wire_peer_connection_state(
                &new_webrtc_conn,
                Arc::clone(&self.callback),
                self.connection_state.clone(),
            );
            let new_channels = build_outbound_data_channels(
                &new_webrtc_conn,
                &self.webrtc_data_channel,
                &self.callback,
                &self.connection_state,
            )
            .await?;
            if self.cancel_token.is_cancelled()
                || self.connection_state.snapshot().state() == TransportConnectionState::Closed
            {
                drop(new_turn_tcp_bridge);
                new_webrtc_conn.on_peer_connection_state_change(Box::new(|_| Box::pin(async {})));
                new_webrtc_conn.on_data_channel(Box::new(|_| Box::pin(async {})));
                for tracked in new_channels {
                    tracked.delivery.close();
                    tracked.channel.on_open(Box::new(|| Box::pin(async {})));
                    tracked.channel.on_close(Box::new(|| Box::pin(async {})));
                    tracked.channel.on_error(Box::new(|_| Box::pin(async {})));
                }
                let _ = new_webrtc_conn.close().await;
                return Ok(());
            }
            let old_channels = self.webrtc_data_channel.replace_all(new_channels)?;
            *self
                .webrtc_conn
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(new_webrtc_conn);
            *self
                .configured_ice_servers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = desired;
            *self
                .sdp_extra_host_candidates
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = new_extra_host_candidates;
            let old_bridge = std::mem::replace(
                &mut *self
                    .turn_tcp_bridge
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                new_turn_tcp_bridge,
            );
            drop(old_bridge);

            current_conn.on_peer_connection_state_change(Box::new(|_| Box::pin(async {})));
            current_conn.on_data_channel(Box::new(|_| Box::pin(async {})));
            for tracked in old_channels {
                tracked.channel.on_open(Box::new(|| Box::pin(async {})));
                tracked.channel.on_close(Box::new(|| Box::pin(async {})));
                tracked.channel.on_error(Box::new(|_| Box::pin(async {})));
            }
            let _ = current_conn.close().await;
            return Ok(());
        }

        let selected = turn_tcp::select_ice_servers_for_connection(&desired);
        let rtc_ice_servers: Vec<RTCIceServer> =
            selected.into_iter().map(RTCIceServer::from).collect();
        let mut cfg = current_conn.get_configuration().await;
        cfg.ice_servers = rtc_ice_servers;
        if let Err(error) = current_conn.set_configuration(cfg).await {
            tracing::debug!(
                %error,
                "Skipped set_configuration on native peer connection"
            );
        } else {
            *self
                .configured_ice_servers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = desired;
        }
        Ok(())
    }

    /// Subscribe a monotone [`NativePhysicalCloseWitness`] that resolves once the underlying
    /// `RTCPeerConnection::close()` future succeeds.
    pub fn physical_close_witness(&self) -> NativePhysicalCloseWitness {
        self.physical_close_completed.witness()
    }

    fn request_close(&self) {
        self.retirement_fence.request();
        if let Some(guard) = self
            .turn_tcp_bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            guard.quiesce_inbound();
        }
        let _ = self
            .webrtc_data_channel
            .for_each(|tracked| tracked.delivery.close());
        self.webrtc_data_channel_state_notifier.wake();
        self.callback.close_link_credit();
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
                return Err(Error::LocalHandshakeGenerationError("Local connection closed".to_string()))
            }
            result = gathering_complete_promise_with_timeout => {
                if result.is_err() {
                    return Err(Error::LocalHandshakeGenerationError(format!(
                        "Webrtc gathering is not completed in {WEBRTC_GATHER_TIMEOUT} seconds"
                    )));
                }
            }
        }

        let sdp = self
            .webrtc_conn()
            .local_description()
            .await
            .ok_or(Error::LocalHandshakeGenerationError(
                "Failed to get local description".to_string(),
            ))?
            .sdp;
        let extra_host_candidates = self
            .sdp_extra_host_candidates
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let sdp = append_sdp_extra_host_candidates(sdp, &extra_host_candidates);

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
    /// Create a new [WebrtcTransport] instance, logging and ignoring invalid ICE configuration.
    pub fn new(
        ice_servers: &str,
        external_address: Option<String>,
        udp_port_range: Option<UdpPortRange>,
    ) -> Self {
        let parsed_ice_servers = parse_ice_servers_or_warn(ice_servers, "native-webrtc");

        Self {
            ice_state: Arc::new(std::sync::RwLock::new((
                ice_servers.to_string(),
                parsed_ice_servers,
            ))),
            external_address,
            udp_port_range,
            pool: Pool::new(),
            frame_pool: NodeFramePool::new(),
            runtime_handle: std::sync::RwLock::new(tokio::runtime::Handle::try_current().ok()),
        }
    }

    /// Create a new [WebrtcTransport] instance, failing closed when `ice_servers` is malformed.
    pub fn new_strict(
        ice_servers: &str,
        external_address: Option<String>,
        udp_port_range: Option<UdpPortRange>,
    ) -> Result<Self> {
        let parsed_ice_servers = parse_ice_servers_strict(ice_servers)?;
        Ok(Self {
            ice_state: Arc::new(std::sync::RwLock::new((
                ice_servers.to_string(),
                parsed_ice_servers,
            ))),
            external_address,
            udp_port_range,
            pool: Pool::new(),
            frame_pool: NodeFramePool::new(),
            runtime_handle: std::sync::RwLock::new(tokio::runtime::Handle::try_current().ok()),
        })
    }

    /// Replace the ICE server list used for subsequent peer connections and update active
    /// native peer connections (rebuilding pre-gather peer connections so their ICE gatherers
    /// pick up renewed credentials, or updating negotiated connections via `set_configuration`).
    pub fn set_ice_servers(&self, ice_servers: &str) {
        let parsed = parse_ice_servers_or_warn(ice_servers, "native-webrtc");
        self.commit_prepared_ice_servers(ice_servers.to_string(), parsed);
    }

    /// Strictly validate and replace the ICE server list, returning [`Error::IceServer`] without
    /// mutating state if `ice_servers` is malformed.
    pub fn try_set_ice_servers(&self, ice_servers: &str) -> Result<()> {
        let parsed = parse_ice_servers_strict(ice_servers)?;
        self.commit_prepared_ice_servers(ice_servers.to_string(), parsed);
        Ok(())
    }

    /// Atomically commit a pre-validated `(raw_credentials, parsed_ice_servers)` pair under a
    /// single write lock and trigger asynchronous refresh on active connections.
    pub fn commit_prepared_ice_servers(&self, raw: String, parsed: Vec<IceServer>) {
        *self
            .ice_state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (raw, parsed);

        let active_connections = self.pool.connections();
        if !active_connections.is_empty() {
            let maybe_runtime = tokio::runtime::Handle::try_current().ok().or_else(|| {
                self.runtime_handle
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            });
            if let Some(runtime) = maybe_runtime {
                for conn in active_connections {
                    runtime.spawn(async move {
                        if let Err(error) = conn.refresh_ice_servers().await {
                            tracing::debug!(
                                %error,
                                "Skipped ICE server refresh on native peer connection"
                            );
                        }
                    });
                }
            }
        }
    }

    /// Return the currently committed raw session credential / ICE server string.
    pub fn session_credentials(&self) -> String {
        self.ice_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .clone()
    }
}

fn ephemeral_udp_for_range(range: UdpPortRange) -> Result<EphemeralUDP> {
    EphemeralUDP::new(range.min(), range.max()).map_err(|e| {
        Error::UdpPortRange(format!(
            "min={}, max={}, reason={e}",
            range.min(),
            range.max()
        ))
    })
}

fn set_udp_network_range(
    setting: &mut webrtc::api::setting_engine::SettingEngine,
    range: Option<UdpPortRange>,
) -> Result<()> {
    if let Some(range) = range {
        setting.set_udp_network(UDPNetwork::Ephemeral(ephemeral_udp_for_range(range)?));
    }
    Ok(())
}

async fn create_peer_connection(
    transport: &WebrtcTransport,
) -> Result<(
    RTCPeerConnection,
    Vec<String>,
    Vec<IceServer>,
    Option<TurnTcpBridgeGuard>,
)> {
    let ice_servers = transport
        .ice_state
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .1
        .clone();
    let (connection, sdp_extra_host_candidates, turn_tcp_bridge) =
        create_peer_connection_with_config(
            &ice_servers,
            transport.external_address.as_deref(),
            transport.udp_port_range,
        )
        .await?;
    Ok((
        connection,
        sdp_extra_host_candidates,
        ice_servers,
        turn_tcp_bridge,
    ))
}

async fn create_peer_connection_with_config(
    ice_servers: &[IceServer],
    external_address: Option<&str>,
    udp_port_range: Option<UdpPortRange>,
) -> Result<(RTCPeerConnection, Vec<String>, Option<TurnTcpBridgeGuard>)> {
    let prepared = prepare_native_ice_config(ice_servers).await?;
    let is_relay_only = prepared.vnet.is_some();
    let configuration = RTCConfiguration {
        ice_servers: prepared.ice_servers,
        ice_transport_policy: prepared.ice_transport_policy,
        ..Default::default()
    };

    let mut setting = webrtc::api::setting_engine::SettingEngine::default();
    set_udp_network_range(&mut setting, udp_port_range)?;
    if let Some(vnet) = prepared.vnet {
        setting.set_vnet(Some(vnet));
    }
    setting.set_ice_multicast_dns_mode(if prepared.disable_mdns {
        MulticastDnsMode::Disabled
    } else {
        MulticastDnsMode::QueryOnly
    });

    let sdp_extra_host_candidates = if is_relay_only {
        Vec::new()
    } else {
        let external_addresses = external_address_candidates(external_address);
        let nat_host_candidates = nat_1to1_host_candidates(&external_addresses);
        let extra = sdp_extra_host_candidates(&external_addresses);
        if !nat_host_candidates.is_empty() {
            tracing::debug!(
                external_addresses = ?nat_host_candidates,
                "setting external WebRTC host candidates"
            );
            setting.set_interface_filter(Box::new(|name: &str| {
                !is_unrouted_container_bridge_interface(name)
            }));
            setting.set_nat_1to1_ips(nat_host_candidates, RTCIceCandidateType::Host);
        }
        if !extra.is_empty() {
            tracing::debug!(
                extra_host_candidates = ?extra,
                "will append SDP-only WebRTC host candidates"
            );
        }
        extra
    };

    let api = webrtc::api::APIBuilder::new()
        .with_setting_engine(setting)
        .build();
    let connection = api.new_peer_connection(configuration).await?;
    Ok((connection, sdp_extra_host_candidates, prepared.bridge_guard))
}

#[async_trait]
impl FrameSink for WebrtcConnection {
    async fn send_frame(
        &self,
        frame: TransportMessage,
        lane: ChannelLane,
        permit: SendPermit,
    ) -> Result<DeliveryFuture> {
        let runtime = native_send_runtime()?;
        let acceptance = permit.acceptance();
        let pool = self.webrtc_data_channel.clone();
        let connection = self.webrtc_conn();
        let physical_close_completed = self.physical_close_completed.clone();
        let retirement_fence = self.retirement_fence.clone();
        run_send_with_retirement(
            &runtime,
            acceptance,
            retirement_fence.clone(),
            move |lifecycle| async move {
                pool.send_with_retirement_fence(frame, lane, permit, retirement_fence, lifecycle)
                    .await
            },
            close_failed_native_send(
                connection,
                self.webrtc_data_channel.clone(),
                self.webrtc_data_channel_state_notifier.clone(),
                self.callback.clone(),
                Arc::clone(&self.turn_tcp_bridge),
                physical_close_completed,
            ),
        )
        .await
    }
}

impl WebrtcConnection {
    async fn create_offer_sdp(&self) -> Result<String> {
        let gathering_complete_promise = {
            let _guard = self.reconfigure_lock.lock().await;
            self.refresh_ice_servers_locked().await?;
            self.negotiation_started.store(true, Ordering::Release);
            let webrtc_conn = self.webrtc_conn();
            let setting_offer = webrtc_conn.create_offer(None).await?;
            let gathering_complete_promise = webrtc_conn.gathering_complete_promise().await;
            webrtc_conn
                .set_local_description(setting_offer.clone())
                .await?;
            gathering_complete_promise
        };

        self.webrtc_gather(gathering_complete_promise).await
    }

    async fn answer_offer_sdp(&self, offer: String) -> Result<String> {
        tracing::debug!("answer_offer, offer: {offer:?}");
        // Read the negotiated limit from the SDP text, but record it only after the *whole* answer
        // path (create_answer + set_local_description + gather) has succeeded, so a failure midway
        // does not leave a partially-updated connection carrying a stale negotiated size.
        let negotiated_max_message_size = effective_max_message_size(&offer)?;
        let (stripped_offer_sdp, deferred_candidates) = split_sdp_remote_candidates(&offer);
        let offer = RTCSessionDescription::offer(stripped_offer_sdp)?;
        let (webrtc_conn, gathering_complete_promise) = {
            let _guard = self.reconfigure_lock.lock().await;
            self.refresh_ice_servers_locked().await?;
            self.negotiation_started.store(true, Ordering::Release);
            let webrtc_conn = self.webrtc_conn();
            webrtc_conn.set_remote_description(offer).await?;

            let answer = webrtc_conn.create_answer(None).await?;
            let gathering_complete_promise = webrtc_conn.gathering_complete_promise().await;
            webrtc_conn.set_local_description(answer.clone()).await?;
            (webrtc_conn, gathering_complete_promise)
        };
        let local_sdp = self.webrtc_gather(gathering_complete_promise).await?;

        self.remote_max_message_size
            .store(negotiated_max_message_size, Ordering::SeqCst);
        schedule_deferred_remote_candidates(
            webrtc_conn,
            deferred_candidates,
            self.cancel_token.clone(),
        );

        Ok(local_sdp)
    }

    async fn accept_answer_sdp(&self, answer: String) -> Result<()> {
        tracing::debug!("accept_answer, answer: {answer:?}");
        let negotiated_max_message_size = effective_max_message_size(&answer)?;
        let answer = RTCSessionDescription::answer(answer)?;
        {
            let _guard = self.reconfigure_lock.lock().await;
            self.negotiation_started.store(true, Ordering::Release);
            self.webrtc_conn().set_remote_description(answer).await?;
        }
        self.remote_max_message_size
            .store(negotiated_max_message_size, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl ConnectionInterface for WebrtcConnection {
    type Error = Error;

    delegate_to_callback!();

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
        self.connection_state.snapshot()
    }

    fn max_message_size(&self) -> usize {
        stored_max_message_size(self.remote_max_message_size.load(Ordering::SeqCst))
    }

    async fn handshake_step(&self, inbound: Option<Bytes>) -> Result<HandshakeStep> {
        match inbound {
            None => {
                self.handshake_role
                    .compare_exchange(
                        HANDSHAKE_ROLE_FRESH,
                        HANDSHAKE_ROLE_IN_PROGRESS,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .map_err(|_| {
                        Error::LocalHandshakeGenerationError(
                            "unexpected initial handshake step on active WebRTC connection"
                                .to_string(),
                        )
                    })?;
                let role_guard = HandshakeRoleGuard::new(&self.handshake_role);
                let sdp = self.create_offer_sdp().await?;
                role_guard.commit(HANDSHAKE_ROLE_INITIATOR_AWAITING_ANSWER);
                Ok(HandshakeStep::Send(Bytes::from(sdp.into_bytes())))
            }
            Some(inbound_bytes) => {
                if self
                    .handshake_role
                    .compare_exchange(
                        HANDSHAKE_ROLE_FRESH,
                        HANDSHAKE_ROLE_IN_PROGRESS,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    let role_guard = HandshakeRoleGuard::new(&self.handshake_role);
                    let offer = std::str::from_utf8(&inbound_bytes)
                        .map_err(|err| {
                            Error::RemoteHandshakeApplyError(format!(
                                "invalid UTF-8 SDP offer payload: {err}"
                            ))
                        })?
                        .to_owned();
                    let local_sdp = self.answer_offer_sdp(offer).await?;
                    role_guard.commit(HANDSHAKE_ROLE_ESTABLISHED);
                    return Ok(HandshakeStep::SendFinal(Bytes::from(
                        local_sdp.into_bytes(),
                    )));
                }

                if self
                    .handshake_role
                    .compare_exchange(
                        HANDSHAKE_ROLE_INITIATOR_AWAITING_ANSWER,
                        HANDSHAKE_ROLE_IN_PROGRESS,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    let role_guard = HandshakeRoleGuard::new(&self.handshake_role);
                    let answer = std::str::from_utf8(&inbound_bytes)
                        .map_err(|err| {
                            Error::RemoteHandshakeApplyError(format!(
                                "invalid UTF-8 SDP answer payload: {err}"
                            ))
                        })?
                        .to_owned();
                    self.accept_answer_sdp(answer).await?;
                    role_guard.commit(HANDSHAKE_ROLE_ESTABLISHED);
                    return Ok(HandshakeStep::Done);
                }

                Err(Error::RemoteHandshakeApplyError(
                    "unexpected inbound handshake step on established WebRTC connection"
                        .to_string(),
                ))
            }
        }
    }

    async fn wait_for_channel_open(&self) -> Result<()> {
        wait_for_data_channel_open(
            || self.connection_state.snapshot(),
            &self.webrtc_data_channel_state_notifier,
            WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT,
        )
        .await
    }

    async fn close(&self) -> Result<()> {
        self.request_close();
        close_reconfigurable_native_connection(
            Arc::clone(&self.reconfigure_lock),
            Arc::clone(&self.webrtc_conn),
            Arc::clone(&self.turn_tcp_bridge),
            self.physical_close_completed.clone(),
        )
        .await
    }
}

fn schedule_deferred_remote_candidates(
    webrtc_conn: Arc<RTCPeerConnection>,
    deferred_candidates: Vec<String>,
    cancel_token: CancellationToken,
) {
    if deferred_candidates.is_empty() {
        return;
    }
    let _ = rings_runtime::spawn_detached(async move {
        tokio::select! {
            _ = cancel_token.cancelled() => return,
            _ = tokio::time::sleep(WEBRTC_ANSWER_CHECK_START_DELAY) => {}
        }
        for candidate in deferred_candidates {
            if cancel_token.is_cancelled() {
                break;
            }
            let _ = webrtc_conn
                .add_ice_candidate(webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
                    candidate,
                    ..Default::default()
                })
                .await;
        }
    });
}

async fn close_reconfigurable_native_connection(
    reconfigure_lock: Arc<Mutex<()>>,
    webrtc_conn: Arc<std::sync::RwLock<Arc<RTCPeerConnection>>>,
    turn_tcp_bridge: Arc<std::sync::Mutex<Option<TurnTcpBridgeGuard>>>,
    physical_close_completed: PhysicalCloseCompletion,
) -> Result<()> {
    let runtime = native_send_runtime()?;
    run_native_close_with_witness(
        &runtime,
        async move {
            let connection = {
                let _guard = reconfigure_lock.lock().await;
                if let Some(guard) = turn_tcp_bridge
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                {
                    guard.quiesce_inbound();
                }
                Arc::clone(
                    &webrtc_conn
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                )
            };
            let result = connection.close().await.map_err(Into::into);
            let maybe_guard = turn_tcp_bridge
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(guard) = maybe_guard {
                guard.shutdown().await;
            }
            result
        },
        physical_close_completed,
    )
    .await
}

#[cfg(test)]
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
    physical_close_completed.run_close(runtime, close).await
}

/// Close a generation already fenced by its send lifecycle, with bounded waiting.
/// The detached close task retains the separate physical-completion witness.
async fn close_failed_native_send(
    connection: Arc<RTCPeerConnection>,
    channel_pool: Arc<ChannelPool<TrackedChannel>>,
    data_channel_notifier: Notifier,
    callback: Arc<InnerTransportCallback>,
    turn_tcp_bridge: Arc<std::sync::Mutex<Option<TurnTcpBridgeGuard>>>,
    physical_close_completed: PhysicalCloseCompletion,
) -> Result<()> {
    let _ = channel_pool.for_each(|tracked| tracked.delivery.close());
    data_channel_notifier.wake();
    callback.close_link_credit();
    let runtime = native_send_runtime()?;
    close_failed_native_send_with_witness(
        &runtime,
        async move {
            if let Some(guard) = turn_tcp_bridge
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                guard.quiesce_inbound();
            }
            let result = connection.close().await.map_err(Into::into);
            let maybe_guard = turn_tcp_bridge
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(guard) = maybe_guard {
                guard.shutdown().await;
            }
            result
        },
        physical_close_completed,
    )
    .await
}

async fn close_failed_native_send_with_witness(
    runtime: &tokio::runtime::Handle,
    close: impl Future<Output = Result<()>> + Send + 'static,
    physical_close_completed: PhysicalCloseCompletion,
) -> Result<()> {
    tokio::time::timeout(
        NATIVE_CONNECTION_RETIRE_TIMEOUT,
        run_native_close_with_witness(runtime, close, physical_close_completed),
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
    let admitted_channels = AtomicU8::new(0);
    webrtc_conn.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
        let Some(lane) = admit_inbound_data_channel(&admitted_channels, channel.label()) else {
            tracing::warn!(
                peer = %inner_cb.cid(),
                label = channel.label(),
                "rejected an inbound data channel that names no lane, or a lane already open"
            );
            return Box::pin(async move {
                if let Err(error) = channel.close().await {
                    tracing::debug!(error = ?error, "failed to close rejected inbound data channel");
                }
            });
        };
        tracing::debug!(
            label = channel.label(),
            id = channel.id(),
            "new received data channel"
        );
        let message_cb = Arc::clone(&inner_cb);
        // The read loop awaits this handler, so it returns at once: a credit frame is applied
        // and a custom frame is queued on its lane, never waiting on the protocol.
        channel.on_message(Box::new(move |msg: DataChannelMessage| {
            let bytes = msg.data.len();
            if msg.is_string {
                tracing::warn!(
                    peer = %message_cb.cid(),
                    bytes,
                    "rejected text data-channel message on binary channel"
                );
                message_cb.report_invalid_inbound_frame();
                return Box::pin(async {});
            }
            tracing::debug!(
                peer = %message_cb.cid(),
                bytes,
                "received data-channel message"
            );
            message_cb.receive_inbound_frame(msg.data, lane);
            Box::pin(async {})
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
        let state_change: TransportConnectionState = state.into();
        connection_state.observe_phase(state_change);
        inner_cb.dispatch_peer_connection_state(state_change);
        Box::pin(async {})
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

async fn build_outbound_data_channels(
    webrtc_conn: &RTCPeerConnection,
    channel_pool: &Arc<ChannelPool<TrackedChannel>>,
    inner_cb: &Arc<InnerTransportCallback>,
    connection_state: &ConnectionStateCell,
) -> Result<Vec<TrackedChannel>> {
    let mut channels = Vec::with_capacity(usize::from(CHANNEL_POOL_SIZE));
    for index in 0..CHANNEL_POOL_SIZE {
        let channel = webrtc_conn
            .create_data_channel(&data_channel_label(ChannelLane::new(index)), None)
            .await?;
        let open_pool = Arc::clone(channel_pool);
        let open_cb = Arc::clone(inner_cb);
        let open_state = connection_state.clone();
        channel.on_open(Box::new(move || {
            let all_ready = matches!(open_pool.all_ready(), Ok(true));
            if all_ready {
                open_state.observe_channels(true);
            }
            let cb = Arc::clone(&open_cb);
            Box::pin(async move {
                if all_ready {
                    cb.on_channel_open().await;
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
            close_state.observe_channels(false);
            let all_closed = matches!(
                close_pool.all(|tracked| {
                    tracked.channel.ready_state() == RTCDataChannelState::Closed
                }),
                Ok(true)
            );
            let cb = Arc::clone(&close_cb);
            Box::pin(async move {
                if all_closed {
                    cb.on_channel_close().await;
                }
            })
        }));

        channels.push(TrackedChannel {
            channel,
            delivery,
            send_lock: Arc::new(Mutex::new(())),
        });
    }
    Ok(channels)
}

async fn create_outbound_data_channels(
    webrtc_conn: &RTCPeerConnection,
    channel_pool: &Arc<ChannelPool<TrackedChannel>>,
    inner_cb: &Arc<InnerTransportCallback>,
    connection_state: &ConnectionStateCell,
) -> Result<()> {
    let channels =
        build_outbound_data_channels(webrtc_conn, channel_pool, inner_cb, connection_state).await?;
    let _ = channel_pool.replace_all(channels)?;
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
        if let Ok(current) = tokio::runtime::Handle::try_current() {
            let mut slot = self
                .runtime_handle
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(current);
            }
        }
        self.pool.ensure_peer_slot_available(cid)?;

        let (webrtc_conn, sdp_extra_host_candidates, configured_ice_servers, turn_tcp_bridge) =
            create_peer_connection(self).await?;

        //
        // Set callbacks
        //
        let webrtc_data_channel_state_notifier = Notifier::default();
        let connection_state = ConnectionStateCell::new();
        let inner_cb = Arc::new(InnerTransportCallback::new(
            cid,
            callback,
            webrtc_data_channel_state_notifier.clone(),
            self.frame_pool.clone(),
        ));

        // Wire open/close on the channels *this* side creates (the pool), not
        // only on received channels: a received channel's `on_open` can be
        // missed if it opens before the handler is registered, which would mean
        // `on_data_channel_open` (and thus `join_dht`) never fires. The created
        // channels are registered before they can open, so this is reliable.
        let channel_pool = Arc::new(ChannelPool::default());
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
            configured_ice_servers,
            Arc::clone(&self.ice_state),
            self.external_address.clone(),
            self.udp_port_range,
            channel_pool,
            webrtc_data_channel_state_notifier,
            connection_state,
            sdp_extra_host_candidates,
            turn_tcp_bridge,
            Arc::clone(&inner_cb),
        );

        let connection = self.pool.safely_insert(cid, conn).await?;
        let spawned = CreditIndex::ALL.into_iter().all(|index| {
            let pump = pump_lane_credits(
                Arc::clone(inner_cb.link_credit()),
                connection.clone(),
                index,
                CREDIT_RESEND_PAUSE,
            );
            rings_runtime::spawn_detached(pump).is_ok()
        }) && rings_runtime::spawn_detached(check_idle_lanes(
            Arc::downgrade(inner_cb.link_credit()),
            LANE_IDLE_PERIOD,
        ))
        .is_ok();
        if !spawned {
            // Without its pump the connection would advertise no credit after the first window,
            // and without its idle check a lane would keep its share of the pool for good.
            self.close_connection_if_current(&connection).await?;
            return Err(Error::CreditPumpUnavailable(cid.to_string()));
        }
        Ok(connection)
    }

    async fn close_connection_if_current(
        &self,
        connection: &ConnectionRef<Self::Connection>,
    ) -> Result<bool> {
        self.pool.safely_remove_if_current(connection).await
    }

    async fn close_dyn_connection_if_current(
        &self,
        connection: &ConnectionRef<crate::core::transport::DynConnection>,
    ) -> Result<bool> {
        self.pool.safely_remove_if_current(connection).await
    }

    fn connection(&self, cid: &str) -> Result<ConnectionRef<Self::Connection>> {
        self.pool.connection(cid)
    }

    fn connection_ids(&self) -> Vec<String> {
        self.pool.connection_ids()
    }

    fn validate_inbound_handshake(&self, _step_index: u8, payload: &[u8]) -> Result<()> {
        let text = std::str::from_utf8(payload).map_err(|err| {
            Error::RemoteHandshakeApplyError(format!("invalid UTF-8 SDP handshake payload: {err}"))
        })?;
        effective_max_message_size(text).map(|_| ())
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

impl From<RTCPeerConnectionState> for TransportConnectionState {
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
#[cfg(test)]
mod test_turn_tcp_model;
