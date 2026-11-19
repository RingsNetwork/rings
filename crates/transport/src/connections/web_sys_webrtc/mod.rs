use std::cell::Cell;
use std::cell::RefCell;
use std::future::poll_fn;
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::future::select;
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
use web_sys::RtcPeerConnectionIceEvent;
use web_sys::RtcPeerConnectionState;
use web_sys::RtcSdpType;
use web_sys::RtcSessionDescription;
use web_sys::RtcSessionDescriptionInit;

use crate::callback::admit_inbound_data_channel;
use crate::callback::credit_pump::check_idle_lanes;
use crate::callback::credit_pump::pump_lane_credits;
use crate::callback::credit_pump::CREDIT_RESEND_PAUSE;
use crate::callback::data_channel_label;
use crate::callback::inbound_frame_exceeds_protocol_ceiling;
use crate::callback::link_credit::LANE_IDLE_PERIOD;
use crate::callback::link_send::FrameSink;
use crate::callback::InnerTransportCallback;
use crate::callback::NodeFramePool;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::credit::CreditIndex;
use crate::core::pool::ChannelLane;
use crate::core::pool::ChannelPool;
use crate::core::pool::LanePool;
use crate::core::pool::CHANNEL_POOL_SIZE;
use crate::core::sdp::parse_sdp_candidate_tier;
use crate::core::sdp::IceCandidateTier;
use crate::core::sdp::IceGatherState;
use crate::core::sdp::IceSettleWindow;
use crate::core::send::operation::send_sync;
use crate::core::transport::effective_max_message_size;
use crate::core::transport::stored_max_message_size;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateCell;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportConnectionState;
use crate::core::transport::TransportInterface;
use crate::core::transport::TransportMessage;
use crate::core::transport::MAX_CHANNEL_MESSAGE_SIZE;
use crate::delivery::tracker::BufferedChannel;
use crate::delivery::tracker::DeliveryTracker;
use crate::delivery::tracker::RoundLease;
use crate::delivery::DeliveryFuture;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::parse_ice_servers_or_warn;
use crate::ice_server::IceCredentialType;
use crate::ice_server::IceServer;
use crate::network_config::UdpPortRange;
use crate::notifier::wait_for_data_channel_open;
use crate::notifier::within;
use crate::notifier::Notifier;
use crate::pool::Pool;

mod send_lifecycle;
use send_lifecycle::BrowserLifecycle;

const WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT: u8 = 8; // seconds
const WEBRTC_GATHER_TIMEOUT: u8 = 60; // seconds
/// Quiet window after a server-reflexive or TURN-relay candidate arrives before ICE gathering is
/// treated as settled when the browser has not yet reported `iceGatheringState == "complete"`.
const WEBRTC_GATHER_REFLEXIVE_SETTLE: Duration = Duration::from_millis(300);
/// Quiet window after a host-only candidate arrives before ICE gathering is treated as settled
/// while waiting on STUN/TURN or mDNS interfaces that may never complete (e.g. WebKit on iOS).
const WEBRTC_GATHER_HOST_SETTLE: Duration = Duration::from_secs(3);

const fn settle_window_duration(window: IceSettleWindow) -> Duration {
    match window {
        IceSettleWindow::Fast => WEBRTC_GATHER_REFLEXIVE_SETTLE,
        IceSettleWindow::Fallback => WEBRTC_GATHER_HOST_SETTLE,
    }
}

/// Classify the settle window implied by an ICE candidate line or an SDP blob against the
/// peer connection's configured `target_tier`, delegating to [`IceGatherState::window_for_tier`].
#[cfg(test)]
pub(crate) fn gather_settle_delay(text: &str, target_tier: IceCandidateTier) -> Option<Duration> {
    let candidate_tier = parse_sdp_candidate_tier(text)?;
    IceGatherState::window_for_tier(candidate_tier, target_tier).map(settle_window_duration)
}

/// Shared progress observed across `onicecandidate` and initial `localDescription.sdp` inspection
/// during [`WebSysWebrtcConnection::webrtc_gather`], backed by the pure [`IceGatherState`]
/// transition algebra.
#[derive(Default)]
struct GatherCandidateProgress {
    /// Pure transition algebra state (`IceGatherState`).
    state: Cell<IceGatherState>,
    /// Wakers registered by [`wait_for_candidates_settled`] while polling for new candidates.
    wakers: RefCell<Vec<std::task::Waker>>,
}

impl GatherCandidateProgress {
    fn new(target_tier: IceCandidateTier) -> Self {
        Self {
            state: Cell::new(IceGatherState::new(target_tier)),
            wakers: RefCell::new(Vec::new()),
        }
    }

    fn observe_text(&self, text: &str) -> bool {
        let (next, armed_or_reset) = self.state.get().observe_text(text);
        self.state.set(next);
        if armed_or_reset {
            let wakers = std::mem::take(&mut *self.wakers.borrow_mut());
            for waker in wakers {
                waker.wake();
            }
        }
        armed_or_reset
    }

    fn register_waker(&self, waker: &std::task::Waker) {
        let mut wakers = self.wakers.borrow_mut();
        if !wakers.iter().any(|existing| existing.will_wake(waker)) {
            wakers.push(waker.clone());
        }
    }
}

/// Wait until gathered ICE candidates have settled for their quiet window (`300 ms` once a
/// reflexive/relay candidate is present, `3 s` for host-only candidates). Any new candidate arrival
/// resets the quiet window.
async fn wait_for_candidates_settled(
    progress: Rc<GatherCandidateProgress>,
) -> std::result::Result<(), rings_runtime::TimerError> {
    loop {
        let snapshot = progress.state.get();
        let observed_epoch = snapshot.epoch;
        if let Some(window) = snapshot.armed_window {
            let delay = settle_window_duration(window);
            let mut sleep_fut = pin!(rings_runtime::sleep(delay));
            let settled = poll_fn(|cx| {
                if progress.state.get().epoch != observed_epoch {
                    return Poll::Ready(Ok(false));
                }
                progress.register_waker(cx.waker());
                match sleep_fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(())) => Poll::Ready(Ok(true)),
                    Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                    Poll::Pending => Poll::Pending,
                }
            })
            .await?;
            if settled && progress.state.get().epoch == observed_epoch {
                return Ok(());
            }
        } else {
            poll_fn(|cx| {
                if progress.state.get().epoch != observed_epoch {
                    return Poll::Ready(());
                }
                progress.register_waker(cx.waker());
                Poll::Pending
            })
            .await;
        }
    }
}

/// A data channel paired with its delivery tracker, which owns the monotonic
/// counter of the total bytes ever enqueued onto it. See the native backend
/// for the rationale; the counter lets the delivery future tell, per message,
/// whether the bytes have left the local send buffer
/// (`enqueued_total - buffered_amount`).
type TrackedChannel = (RtcDataChannel, Arc<DeliveryTracker>);

impl BufferedChannel for RtcDataChannel {
    async fn arm_low_threshold(&self, threshold: u64) {
        // Never saturates: a round waits for the event only when τ < b, and the
        // browser's `bufferedAmount` is a u32. Saturating keeps the conversion
        // total.
        self.set_buffered_amount_low_threshold(u32::try_from(threshold).unwrap_or(u32::MAX));
    }

    async fn observe_buffered(&self) -> u64 {
        u64::from(self.buffered_amount())
    }
}

/// Run a delivery settle round as a browser task. Both channel effects are
/// synchronous, so one poll runs the whole round between two browser tasks.
fn spawn_delivery_round(lease: Option<RoundLease>, channel: RtcDataChannel) {
    if let Some(lease) = lease {
        spawn_local(async move { lease.run(&channel).await });
    }
}

/// Build the future that resolves once the message ending at `end_offset` on
/// this channel has been flushed to the wire, or errors if the channel closes
/// first. Registering it re-arms the channel's low-water threshold.
fn delivery_future(
    channel: RtcDataChannel,
    delivery: &Arc<DeliveryTracker>,
    end_offset: u64,
) -> DeliveryFuture {
    let (wait, lease) = delivery.track(end_offset);
    spawn_delivery_round(lease, channel);
    Box::pin(wait)
}

type VoidClosure = Closure<dyn FnMut()>;
type InboundChannelHandler = (RtcDataChannel, Closure<dyn FnMut(MessageEvent)>);

/// Drive a channel's delivery tracker from its `bufferedamountlow` and `error`
/// events. `close` is wired with the pool's close handler.
fn wire_delivery_events(
    channel: &RtcDataChannel,
    delivery: &Arc<DeliveryTracker>,
) -> (VoidClosure, VoidClosure) {
    let low_channel = channel.clone();
    let low_delivery = Arc::clone(delivery);
    let on_low = Closure::wrap(Box::new(move || {
        spawn_delivery_round(low_delivery.notify(), low_channel.clone());
    }) as Box<dyn FnMut()>);
    channel.set_onbufferedamountlow(Some(on_low.as_ref().unchecked_ref()));

    let error_delivery = Arc::clone(delivery);
    let on_error = Closure::wrap(Box::new(move || {
        tracing::warn!("data channel failed; pending deliveries are lost");
        error_delivery.close();
    }) as Box<dyn FnMut()>);
    channel.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    (on_low, on_error)
}

/// Owned JavaScript event-listener closures for one outbound data channel.
struct OutboundChannelHandlers {
    channel: RtcDataChannel,
    onopen: Option<VoidClosure>,
    onclose: Option<VoidClosure>,
    onbufferedamountlow: Option<VoidClosure>,
    onerror: Option<VoidClosure>,
}

/// Owned JavaScript event-listener closures for one [`WebSysWebrtcConnection`], detached and
/// freed when the connection closes or drops instead of leaking via [`Closure::forget`].
#[derive(Default)]
struct ConnectionHandlers {
    ondatachannel: Option<Closure<dyn FnMut(RtcDataChannelEvent)>>,
    onconnectionstatechange: Option<Closure<dyn FnMut(web_sys::Event)>>,
    outbound: Vec<OutboundChannelHandlers>,
    inbound: Vec<InboundChannelHandler>,
}

impl ConnectionHandlers {
    fn detach(&mut self, conn: &RtcPeerConnection) {
        conn.set_ondatachannel(None);
        self.ondatachannel = None;
        conn.set_onconnectionstatechange(None);
        self.onconnectionstatechange = None;
        for mut outbound in self.outbound.drain(..) {
            outbound.channel.set_onopen(None);
            outbound.channel.set_onclose(None);
            outbound.channel.set_onbufferedamountlow(None);
            outbound.channel.set_onerror(None);
            outbound.onopen = None;
            outbound.onclose = None;
            outbound.onbufferedamountlow = None;
            outbound.onerror = None;
        }
        for (channel, _on_message) in self.inbound.drain(..) {
            channel.set_onmessage(None);
        }
    }
}

/// Guard that detaches `onicegatheringstatechange` and `onicecandidate` before freeing their
/// [`Closure`]s on any exit path of [`WebSysWebrtcConnection::webrtc_gather`].
struct GatherListenerGuard<'a> {
    conn: &'a RtcPeerConnection,
    _gathering_closure: Closure<dyn FnMut()>,
    _candidate_closure: Closure<dyn FnMut(RtcPeerConnectionIceEvent)>,
}

impl Drop for GatherListenerGuard<'_> {
    fn drop(&mut self) {
        self.conn.set_onicegatheringstatechange(None);
        self.conn.set_onicecandidate(None);
    }
}

/// Cloneable handle performing full browser connection teardown on explicit close or failed send.
#[derive(Clone)]
struct BrowserCloseHandle {
    webrtc_conn: RtcPeerConnection,
    webrtc_data_channel: Rc<ChannelPool<TrackedChannel>>,
    webrtc_data_channel_state_notifier: Notifier,
    connection_state: ConnectionStateCell,
    callback: Rc<InnerTransportCallback>,
    local_close: Notifier,
    handlers: Rc<RefCell<ConnectionHandlers>>,
}

impl BrowserCloseHandle {
    fn close(&self) {
        self.connection_state.close();
        let _ = self
            .webrtc_data_channel
            .for_each(|(_, delivery)| delivery.close());
        self.webrtc_conn.close();
        self.handlers.borrow_mut().detach(&self.webrtc_conn);
        self.local_close.wake();
        self.webrtc_data_channel_state_notifier.wake();
        self.callback.close_link_credit();
    }
}

impl WebSysWebrtcConnection {
    /// Use the same permit, accounting and destruction boundary as the native backend.
    /// The send primitive is synchronous; an irrevocable error awaits actor completion.
    async fn send_after_permit(
        &self,
        permit: SendPermit,
        enqueued: Arc<AtomicU64>,
        bytes: u64,
        send: impl FnOnce() -> Result<()>,
    ) -> Result<u64> {
        let close_handle = self.close_handle();
        // The actor owns the connection-close capture before final send admission.
        let lifecycle = BrowserLifecycle::new(
            permit.acceptance(),
            self.connection_state.clone(),
            move || close_handle.close(),
        );
        // Unit is the browser's channel lease: no await occurs inside synchronous JS send.
        let result = send_sync(send, permit, enqueued, bytes, Rc::clone(&lifecycle)).await;
        lifecycle.finish(result).await
    }

    fn close_handle(&self) -> BrowserCloseHandle {
        BrowserCloseHandle {
            webrtc_conn: self.webrtc_conn.clone(),
            webrtc_data_channel: Rc::clone(&self.webrtc_data_channel),
            webrtc_data_channel_state_notifier: self.webrtc_data_channel_state_notifier.clone(),
            connection_state: self.connection_state.clone(),
            callback: Rc::clone(&self.callback),
            local_close: self.local_close.clone(),
            handlers: Rc::clone(&self.handlers),
        }
    }
}

impl ChannelPool<TrackedChannel> {
    /// Return whether every channel in this backend pool is open.
    fn all_ready(&self) -> Result<bool> {
        self.all(|(c, _)| c.ready_state() == RtcDataChannelState::Open)
    }
}

/// A connection that implemented by web_sys library.
/// Used for browser environment.
pub struct WebSysWebrtcConnection {
    webrtc_conn: RtcPeerConnection,
    // `Rc`, not `Arc`: the browser backend is single-threaded (the `ConnectionInterface` impl is
    // `?Send`), so the channel pool is never shared across threads.
    webrtc_data_channel: Rc<ChannelPool<TrackedChannel>>,
    webrtc_data_channel_state_notifier: Notifier,
    connection_state: ConnectionStateCell,
    /// Highest candidate tier the configured ICE servers can produce (`Host < Reflexive < Relay`).
    target_tier: Cell<IceCandidateTier>,
    /// Negotiated SCTP `max_message_size` (RFC 8841), parsed from the remote SDP at handshake.
    /// `0` means not yet negotiated. Parsed identically to native for consistent behaviour.
    remote_max_message_size: Arc<AtomicUsize>,
    /// The connection's callback: its credit gates every custom send.
    callback: Rc<InnerTransportCallback>,
    /// Woken by [`ConnectionInterface::close`]: `RTCPeerConnection.close()` fires no event, so a
    /// wait on the peer connection's own events learns of a local close only through it.
    local_close: Notifier,
    /// Owned JavaScript event closures detached when this connection closes or drops.
    handlers: Rc<RefCell<ConnectionHandlers>>,
}

impl Drop for WebSysWebrtcConnection {
    fn drop(&mut self) {
        let _ = self
            .webrtc_data_channel
            .for_each(|(_, delivery)| delivery.close());
        self.handlers.borrow_mut().detach(&self.webrtc_conn);
        self.local_close.wake();
        self.webrtc_data_channel_state_notifier.wake();
        self.callback.close_link_credit();
    }
}

/// [WebSysWebrtcTransport] manages all the [WebSysWebrtcConnection] and
/// provides methods to create, get and close connections.
pub struct WebSysWebrtcTransport {
    session_credentials: RefCell<String>,
    ice_servers: RefCell<Vec<IceServer>>,
    pool: Pool<WebSysWebrtcConnection>,
    /// The frames this node lends its connections' lanes, shared by every connection.
    frame_pool: NodeFramePool,
}

impl WebSysWebrtcConnection {
    #[cfg(test)]
    fn new(
        webrtc_conn: RtcPeerConnection,
        webrtc_data_channel: Rc<ChannelPool<TrackedChannel>>,
        webrtc_data_channel_state_notifier: Notifier,
        connection_state: ConnectionStateCell,
        callback: Rc<InnerTransportCallback>,
    ) -> Self {
        Self::new_with_handlers(
            webrtc_conn,
            webrtc_data_channel,
            webrtc_data_channel_state_notifier,
            connection_state,
            IceCandidateTier::Reflexive,
            callback,
            Rc::new(RefCell::new(ConnectionHandlers::default())),
        )
    }

    fn new_with_handlers(
        webrtc_conn: RtcPeerConnection,
        webrtc_data_channel: Rc<ChannelPool<TrackedChannel>>,
        webrtc_data_channel_state_notifier: Notifier,
        connection_state: ConnectionStateCell,
        target_tier: IceCandidateTier,
        callback: Rc<InnerTransportCallback>,
        handlers: Rc<RefCell<ConnectionHandlers>>,
    ) -> Self {
        Self {
            webrtc_conn,
            webrtc_data_channel,
            webrtc_data_channel_state_notifier,
            connection_state,
            target_tier: Cell::new(target_tier),
            remote_max_message_size: Arc::new(AtomicUsize::new(0)),
            callback,
            local_close: Notifier::default(),
            handlers,
        }
    }

    /// Wait for ICE gathering to complete (or gathered candidates to settle) and return the local
    /// description.
    ///
    /// Law (Gathering ends): the wait ends when gathering completes, when gathered candidates have
    /// settled for their quiet window (`300 ms` once a candidate reaches `self.target_tier`, `3 s`
    /// otherwise), when this connection is closed locally, or at `WEBRTC_GATHER_TIMEOUT`,
    /// whichever comes first. In WebKit (Safari and every browser on iOS), `RTCPeerConnection`
    /// gathers its STUN `srflx` candidate in a few hundred milliseconds yet `iceGatheringState`
    /// can remain `"gathering"` past the 60 s timeout while waiting on mDNS host-candidate
    /// registration or secondary interfaces that never finish. A gathering of a locally closed
    /// connection fails as `Local connection closed`, the error native WebRTC reports, so core
    /// classifies it by slot ownership (#945); waiting on gathering events alone would outlive the
    /// close, which fires none (#960).
    async fn webrtc_gather(&self) -> Result<String> {
        let notifier = Notifier::default();
        let target_tier = self.target_tier.get();
        let progress = Rc::new(GatherCandidateProgress::new(target_tier));

        let notifier_clone = notifier.clone();
        let conn_clone = self.webrtc_conn.clone();
        let onicegatheringstatechange = Box::new(move || match conn_clone.ice_gathering_state() {
            RtcIceGatheringState::Complete => notifier_clone.wake(),
            x => {
                tracing::trace!("gather status: {:?}", x)
            }
        });

        let notifier_for_null_candidate = notifier.clone();
        let progress_clone = Rc::clone(&progress);
        let onicecandidate = Box::new(move |event: RtcPeerConnectionIceEvent| {
            match event.candidate() {
                None => {
                    // End-of-candidates event (`event.candidate === null`).
                    notifier_for_null_candidate.wake();
                }
                Some(candidate) => {
                    let cand_str = candidate.candidate();
                    if cand_str.is_empty() {
                        // In some browsers, an empty candidate string signals end-of-candidates.
                        notifier_for_null_candidate.wake();
                    } else {
                        progress_clone.observe_text(&cand_str);
                    }
                }
            }
        });

        let gathering_closure = Closure::wrap(onicegatheringstatechange as Box<dyn FnMut()>);
        let candidate_closure =
            Closure::wrap(onicecandidate as Box<dyn FnMut(RtcPeerConnectionIceEvent)>);
        self.webrtc_conn
            .set_onicegatheringstatechange(Some(gathering_closure.as_ref().unchecked_ref()));
        self.webrtc_conn
            .set_onicecandidate(Some(candidate_closure.as_ref().unchecked_ref()));
        let _gather_listener = GatherListenerGuard {
            conn: &self.webrtc_conn,
            _gathering_closure: gathering_closure,
            _candidate_closure: candidate_closure,
        };
        // Subscribe, then read: the browser runtime is single-threaded, so no gathering or
        // candidate event runs between this read and the wait. Any candidate already materialized
        // in `localDescription.sdp` during `setLocalDescription` arms the settle window.
        if let Some(desc) = self.webrtc_conn.local_description() {
            progress.observe_text(&desc.sdp());
        }
        if self.webrtc_conn.ice_gathering_state() == RtcIceGatheringState::Complete {
            notifier.wake();
        }

        let settled = pin!(wait_for_candidates_settled(Rc::clone(&progress)));
        let gather_or_settled = select(notifier, settled);
        within(
            select(gather_or_settled, self.local_close.clone()),
            Duration::from_secs(WEBRTC_GATHER_TIMEOUT.into()),
        )
        .await?;
        if self.connection_state.snapshot().state() == TransportConnectionState::Closed {
            return Err(Error::LocalHandshakeGenerationError(
                "Local connection closed".to_string(),
            ));
        }

        let local_sdp = self
            .webrtc_conn
            .local_description()
            .ok_or(Error::LocalHandshakeGenerationError(
                "local_description is None".to_string(),
            ))
            .map(|x| x.sdp())?;
        let has_gathered_candidates = parse_sdp_candidate_tier(&local_sdp).is_some();
        if self.webrtc_conn.ice_gathering_state() != RtcIceGatheringState::Complete
            && !has_gathered_candidates
        {
            return Err(Error::LocalHandshakeGenerationError(format!(
                "Webrtc gathering is not completed in {WEBRTC_GATHER_TIMEOUT} seconds"
            )));
        }

        Ok(local_sdp)
    }
}

impl WebSysWebrtcTransport {
    /// Create a new [WebSysWebrtcTransport] instance.
    pub fn new(
        ice_servers: &str,
        _external_address: Option<String>,
        _udp_port_range: Option<UdpPortRange>,
    ) -> Self {
        let parsed_ice_servers = parse_ice_servers_or_warn(ice_servers, "web-sys-webrtc");

        Self {
            session_credentials: RefCell::new(ice_servers.to_string()),
            ice_servers: RefCell::new(parsed_ice_servers),
            pool: Pool::new(),
            frame_pool: NodeFramePool::new(),
        }
    }

    /// Replace the ICE server list used for subsequent peer connections and update active
    /// browser peer connections in place via `RTCPeerConnection.setConfiguration`.
    pub fn set_ice_servers(&self, ice_servers: &str) {
        *self.session_credentials.borrow_mut() = ice_servers.to_string();
        let parsed = parse_ice_servers_or_warn(ice_servers, "web-sys-webrtc");
        let target_tier = IceServer::target_candidate_tier(&parsed);
        *self.ice_servers.borrow_mut() = parsed.clone();

        let config = RtcConfiguration::new();
        let js_ice_servers: js_sys::Array =
            js_sys::Array::from_iter(parsed.into_iter().map(RtcIceServer::from));
        config.set_ice_servers(&js_ice_servers.into());

        for conn in self.pool.connections() {
            if conn.webrtc_conn.connection_state() != RtcPeerConnectionState::Closed {
                if let Err(error) = conn
                    .webrtc_conn
                    .set_configuration_with_configuration(&config)
                {
                    tracing::debug!(
                        ?error,
                        "Skipped setConfiguration on active browser peer connection"
                    );
                } else {
                    conn.target_tier.set(target_tier);
                }
            }
        }
    }
}

#[async_trait(?Send)]
impl FrameSink for WebSysWebrtcConnection {
    /// Encode and account one browser send using the shared checked queue owner.
    async fn send_frame(
        &self,
        frame: TransportMessage,
        lane: ChannelLane,
        permit: SendPermit,
    ) -> Result<DeliveryFuture> {
        let (channel, delivery) = self.webrtc_data_channel.select(lane)?;
        let data = rings_codec::serialize(&frame)?;
        let bytes = u64::try_from(data.len()).map_err(|_| Error::SendByteCountOverflow)?;
        // The primitive runs only after shared queue admission; failures do not advance offsets.
        let end_offset = self
            .send_after_permit(permit, Arc::clone(delivery.enqueued()), bytes, || {
                channel
                    .send_with_u8_array(&data)
                    .map_err(Error::WebSysWebrtc)
            })
            .await
            .inspect_err(|error| tracing::error!(error = ?error, bytes, "browser send failed"))?;
        Ok(delivery_future(channel, &delivery, end_offset))
    }
}

#[async_trait(?Send)]
impl ConnectionInterface for WebSysWebrtcConnection {
    type Handshake = String;
    type Error = Error;

    delegate_to_callback!();

    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
        self.connection_state.snapshot()
    }

    fn max_message_size(&self) -> usize {
        stored_max_message_size(self.remote_max_message_size.load(Ordering::SeqCst))
    }

    async fn create_offer(&self) -> Result<Self::Handshake> {
        let promise = self.webrtc_conn.create_offer();
        let offer_js_value = JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;
        let offer = RtcSessionDescription::from(offer_js_value);
        let sdp = offer.sdp();

        let set_local_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        set_local_init.set_sdp(&sdp);

        let promise = self.webrtc_conn.set_local_description(&set_local_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;

        self.webrtc_gather().await
    }

    async fn answer_offer(&self, offer: Self::Handshake) -> Result<Self::Handshake> {
        tracing::debug!("answer_offer, offer: {offer:?}");
        // Read the negotiated limit, but record it only after the *whole* answer path
        // (setRemoteDescription + createAnswer + setLocalDescription + gather) has succeeded, so a
        // failure midway does not leave a partially-updated connection carrying a stale size.
        let negotiated_max_message_size = effective_max_message_size(&offer)?;

        let set_remote_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        set_remote_init.set_sdp(&offer);

        let promise = self.webrtc_conn.set_remote_description(&set_remote_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;

        let promise = self.webrtc_conn.create_answer();
        let answer_js_value = JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;
        let answer = RtcSessionDescription::from(answer_js_value);
        let sdp = answer.sdp();

        let set_local_init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        set_local_init.set_sdp(&sdp);

        let promise = self.webrtc_conn.set_local_description(&set_local_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;

        let local_sdp = self.webrtc_gather().await?;
        self.remote_max_message_size
            .store(negotiated_max_message_size, Ordering::SeqCst);
        Ok(local_sdp)
    }

    async fn accept_answer(&self, answer: Self::Handshake) -> Result<()> {
        tracing::debug!("accept_answer, answer: {answer:?}");
        let negotiated_max_message_size = effective_max_message_size(&answer)?;

        let set_remote_init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        set_remote_init.set_sdp(&answer);

        let promise = self.webrtc_conn.set_remote_description(&set_remote_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;
        // Applied: now it is correct to record the negotiated limit.
        self.remote_max_message_size
            .store(negotiated_max_message_size, Ordering::SeqCst);

        Ok(())
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
        self.close_handle().close();
        Ok(())
    }
}

fn decode_data_channel_message(data: JsValue) -> Result<Vec<u8>> {
    let buffer = data.dyn_into::<js_sys::ArrayBuffer>().map_err(|_| {
        Error::ChannelMessage(
            "received a non-ArrayBuffer value after configuring binaryType".to_string(),
        )
    })?;
    let bytes = buffer.byte_length() as usize;
    if inbound_frame_exceeds_protocol_ceiling(bytes) {
        return Err(Error::ChannelMessage(format!(
            "inbound frame of {bytes} bytes exceeds the {MAX_CHANNEL_MESSAGE_SIZE}-byte protocol ceiling"
        )));
    }
    let message = js_sys::Uint8Array::new(&buffer).to_vec();
    Ok(message)
}

/// Receive one browser message on `lane`: a credit frame is applied and a custom frame queued on
/// its lane at once; `onmessage` never waits on the protocol.
fn dispatch_data_channel_message(
    callback: Rc<InnerTransportCallback>,
    data: JsValue,
    lane: ChannelLane,
) {
    match decode_data_channel_message(data) {
        Ok(message) => {
            tracing::debug!(
                peer = %callback.cid(),
                bytes = message.len(),
                "received data-channel message"
            );
            callback.receive_inbound_frame(Bytes::from(message), lane);
        }
        Err(error) => {
            tracing::warn!(peer = %callback.cid(), %error, "rejected data-channel message");
            callback.report_invalid_inbound_frame();
        }
    }
}

fn wire_data_channel_messages(
    channel: &RtcDataChannel,
    callback: Rc<InnerTransportCallback>,
    lane: ChannelLane,
) -> Closure<dyn FnMut(MessageEvent)> {
    let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
        dispatch_data_channel_message(callback.clone(), event.data(), lane);
    }) as Box<dyn FnMut(MessageEvent)>);
    channel.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message
}

fn wire_received_data_channels(
    webrtc_conn: &RtcPeerConnection,
    inner_cb: Rc<InnerTransportCallback>,
    handlers: &Rc<RefCell<ConnectionHandlers>>,
) {
    // Inbound channels carry messages only. One remote-created channel closing
    // does not prove the SCTP association is gone; outbound-pool state owns
    // readiness and emits the terminal data-channel callback when all close.
    let admitted_channels = AtomicU8::new(0);
    let inbound_handlers = Rc::clone(handlers);
    let on_data_channel = Box::new(move |event: RtcDataChannelEvent| {
        let channel = event.channel();
        let Some(lane) = admit_inbound_data_channel(&admitted_channels, &channel.label()) else {
            tracing::warn!(
                peer = %inner_cb.cid(),
                label = channel.label(),
                "rejected an inbound data channel that names no lane, or a lane already open"
            );
            channel.close();
            return;
        };
        channel.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);
        tracing::debug!(label = channel.label(), "new received data channel");

        let on_message = wire_data_channel_messages(&channel, inner_cb.clone(), lane);
        inbound_handlers
            .borrow_mut()
            .inbound
            .push((channel, on_message));
    });

    let callback = Closure::wrap(on_data_channel as Box<dyn FnMut(RtcDataChannelEvent)>);
    webrtc_conn.set_ondatachannel(Some(callback.as_ref().unchecked_ref()));
    handlers.borrow_mut().ondatachannel = Some(callback);
}

fn wire_peer_connection_state(
    webrtc_conn: &RtcPeerConnection,
    inner_cb: Rc<InnerTransportCallback>,
    connection_state: ConnectionStateCell,
    handlers: &Rc<RefCell<ConnectionHandlers>>,
) {
    let state_source = webrtc_conn.clone();
    let on_state_change = Closure::wrap(Box::new(move |_| {
        let state_change: TransportConnectionState = state_source.connection_state().into();
        tracing::debug!("peer connection state changed: {state_change:?}");
        connection_state.observe_webrtc(state_change);
        inner_cb.dispatch_peer_connection_state(state_change);
    }) as Box<dyn FnMut(web_sys::Event)>);
    webrtc_conn.set_onconnectionstatechange(Some(on_state_change.as_ref().unchecked_ref()));
    handlers.borrow_mut().onconnectionstatechange = Some(on_state_change);
}

fn create_outbound_data_channels(
    webrtc_conn: &RtcPeerConnection,
    channel_pool: &Rc<ChannelPool<TrackedChannel>>,
    inner_cb: &Rc<InnerTransportCallback>,
    connection_state: &ConnectionStateCell,
    handlers: &Rc<RefCell<ConnectionHandlers>>,
) -> Result<()> {
    for index in 0..CHANNEL_POOL_SIZE {
        let channel = webrtc_conn.create_data_channel(&data_channel_label(ChannelLane::new(index)));
        let open_pool = channel_pool.clone();
        let open_cb = inner_cb.clone();
        let open_state = connection_state.clone();
        let on_open = Closure::wrap(Box::new(move || {
            let all_ready = matches!(open_pool.all_ready(), Ok(true));
            if all_ready {
                open_state.observe_outbound_data_channels(true);
            }
            let cb = open_cb.clone();
            spawn_local(async move {
                if all_ready {
                    cb.on_channel_open().await;
                }
            });
        }) as Box<dyn FnMut()>);
        channel.set_onopen(Some(on_open.as_ref().unchecked_ref()));

        let delivery = Arc::new(DeliveryTracker::default());
        let (on_low, on_error) = wire_delivery_events(&channel, &delivery);

        let close_pool = channel_pool.clone();
        let close_cb = inner_cb.clone();
        let close_state = connection_state.clone();
        let close_delivery = Arc::clone(&delivery);
        let on_close = Closure::wrap(Box::new(move || {
            close_delivery.close();
            close_state.observe_outbound_data_channels(false);
            let all_closed = matches!(
                close_pool.all(|(candidate, _)| {
                    candidate.ready_state() == RtcDataChannelState::Closed
                }),
                Ok(true)
            );
            let cb = close_cb.clone();
            spawn_local(async move {
                if all_closed {
                    cb.on_channel_close().await;
                }
            });
        }) as Box<dyn FnMut()>);
        channel.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        handlers
            .borrow_mut()
            .outbound
            .push(OutboundChannelHandlers {
                channel: channel.clone(),
                onopen: Some(on_open),
                onclose: Some(on_close),
                onbufferedamountlow: Some(on_low),
                onerror: Some(on_error),
            });
        channel_pool.push((channel, delivery))?;
    }
    Ok(())
}

#[async_trait(?Send)]
impl TransportInterface for WebSysWebrtcTransport {
    type Connection = WebSysWebrtcConnection;
    type Error = Error;

    async fn new_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> Result<ConnectionRef<Self::Connection>> {
        self.pool.ensure_peer_slot_available(cid)?;

        //
        // Setup webrtc connection env
        //
        let config = RtcConfiguration::new();
        let (target_tier, ice_servers) = {
            let configured_ice = self.ice_servers.borrow();
            let target_tier = IceServer::target_candidate_tier(&configured_ice);
            let js_servers: js_sys::Array =
                js_sys::Array::from_iter(configured_ice.iter().cloned().map(RtcIceServer::from));
            (target_tier, js_servers)
        };
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
        let connection_state = ConnectionStateCell::new();
        let inner_cb = Rc::new(InnerTransportCallback::new(
            cid,
            callback,
            webrtc_data_channel_state_notifier.clone(),
            self.frame_pool.clone(),
        ));

        let channel_pool = Rc::new(ChannelPool::default());
        let handlers = Rc::new(RefCell::new(ConnectionHandlers::default()));
        // Wire open/close on the channels this side creates (the pool), not only
        // on received channels: a received channel's `onopen` can be missed if
        // it opens before the handler is registered, so `on_data_channel_open`
        // (and thus `join_dht`) would never fire. Created channels are wired
        // before they can open, so this is reliable.
        wire_received_data_channels(&webrtc_conn, inner_cb.clone(), &handlers);
        wire_peer_connection_state(
            &webrtc_conn,
            inner_cb.clone(),
            connection_state.clone(),
            &handlers,
        );
        create_outbound_data_channels(
            &webrtc_conn,
            &channel_pool,
            &inner_cb,
            &connection_state,
            &handlers,
        )?;

        //
        // Construct the Connection
        //
        let conn = WebSysWebrtcConnection::new_with_handlers(
            webrtc_conn,
            channel_pool,
            webrtc_data_channel_state_notifier,
            connection_state,
            target_tier,
            Rc::clone(&inner_cb),
            handlers,
        );

        let connection = self.pool.safely_insert(cid, conn).await?;
        for index in CreditIndex::ALL {
            spawn_local(pump_lane_credits(
                Arc::clone(inner_cb.link_credit()),
                connection.clone(),
                index,
                CREDIT_RESEND_PAUSE,
            ));
        }
        spawn_local(check_idle_lanes(
            Arc::downgrade(inner_cb.link_credit()),
            LANE_IDLE_PERIOD,
        ));
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

    fn update_session_credentials(&self, credentials: &str) {
        self.set_ice_servers(credentials);
    }

    fn session_credentials(&self) -> Option<String> {
        Some(self.session_credentials.borrow().clone())
    }
}

// set default to password
impl From<IceCredentialType> for RtcIceCredentialType {
    fn from(s: IceCredentialType) -> Self {
        match s {
            IceCredentialType::Password => Self::Password,
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

impl From<RtcPeerConnectionState> for TransportConnectionState {
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

#[cfg(test)]
mod test_gather;

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use bytes::Bytes;
    use wasm_bindgen_test::wasm_bindgen_test;
    use wasm_bindgen_test::wasm_bindgen_test_configure;

    use super::*;
    use crate::core::callback::AdmittedInboundMessage;
    use crate::core::callback::TransportCallback;

    wasm_bindgen_test_configure!(run_in_browser);

    struct InvalidRecordingCallback {
        invalid_frames: Rc<Cell<usize>>,
    }

    #[async_trait(?Send)]
    impl TransportCallback for InvalidRecordingCallback {
        async fn on_admitted_message(
            &self,
            _message: AdmittedInboundMessage<'_>,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }

        async fn on_invalid_inbound_frame(
            &self,
            _cid: &str,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            self.invalid_frames.set(self.invalid_frames.get() + 1);
            Ok(())
        }
    }

    /// A transport callback that ignores every event.
    struct IgnoredCallback;

    #[async_trait(?Send)]
    impl TransportCallback for IgnoredCallback {}

    /// A callback for a backend under test, which these tests never dispatch to.
    fn ignored_callback() -> Rc<InnerTransportCallback> {
        Rc::new(InnerTransportCallback::new(
            "peer",
            Box::new(IgnoredCallback),
            Notifier::default(),
            NodeFramePool::new(),
        ))
    }

    fn test_backend() -> (
        RtcPeerConnection,
        ConnectionStateCell,
        WebSysWebrtcConnection,
    ) {
        let peer_connection =
            RtcPeerConnection::new().expect("browser peer connection must construct");
        let connection_state = ConnectionStateCell::new();
        let connection = WebSysWebrtcConnection::new(
            peer_connection.clone(),
            Rc::new(ChannelPool::from_vec(Vec::new())),
            Notifier::default(),
            connection_state.clone(),
            ignored_callback(),
        );
        (peer_connection, connection_state, connection)
    }

    #[wasm_bindgen_test]
    async fn test_rejected_permit_does_not_call_browser_send_primitive() {
        let (_peer_connection, connection_state, connection) = test_backend();
        let called = Rc::new(Cell::new(false));
        let observed = called.clone();
        let result = connection
            .send_after_permit(
                SendPermit::new(|| false),
                Arc::new(AtomicU64::new(0)),
                1,
                move || {
                    observed.set(true);
                    Ok(())
                },
            )
            .await;

        assert!(matches!(result, Err(Error::SendPermitRevoked)));
        assert!(!called.get());
        assert_ne!(
            connection_state.snapshot().state(),
            TransportConnectionState::Closed
        );
    }

    #[wasm_bindgen_test]
    async fn test_browser_send_failure_retires_connection_and_rejects_later_send() {
        let (peer_connection, connection_state, connection) = test_backend();
        connection_state.observe_webrtc(TransportConnectionState::Connected);
        connection_state.observe_outbound_data_channels(true);
        let permit = SendPermit::always();
        let acceptance = permit.acceptance();
        let result = connection
            .send_after_permit(permit, Arc::new(AtomicU64::new(0)), 1, || {
                Err::<(), _>(Error::ChannelMessage(
                    "injected browser send failure".to_string(),
                ))
            })
            .await;

        assert!(matches!(result, Err(Error::ChannelMessage(_))));
        assert!(!acceptance.is_accepted());
        // The public error return must follow the actual browser close call, not just fencing.
        assert_eq!(
            peer_connection.connection_state(),
            RtcPeerConnectionState::Closed
        );
        assert_eq!(
            connection_state.snapshot().state(),
            TransportConnectionState::Closed
        );
        assert!(matches!(
            connection
                .send_message_with_permit(
                    Bytes::from_static(&[1]),
                    ChannelLane::default(),
                    SendPermit::always(),
                )
                .await,
            Err(Error::ChannelOpen(_))
        ));
    }

    #[wasm_bindgen_test]
    fn test_oversized_browser_frame_is_rejected_before_copy() {
        let length = u32::try_from(MAX_CHANNEL_MESSAGE_SIZE + 1)
            .expect("protocol ceiling must fit in a JavaScript array length");
        let array = js_sys::Uint8Array::new_with_length(length);

        let result = decode_data_channel_message(array.buffer().into());

        assert!(matches!(result, Err(Error::ChannelMessage(_))));
    }

    #[wasm_bindgen_test]
    async fn test_registered_browser_onmessage_coalesces_invalid_frame_accounting() {
        let invalid_frames = Rc::new(Cell::new(0));
        let callback = Rc::new(InnerTransportCallback::new(
            "peer",
            Box::new(InvalidRecordingCallback {
                invalid_frames: Rc::clone(&invalid_frames),
            }),
            Notifier::default(),
            NodeFramePool::new(),
        ));
        let connection = RtcPeerConnection::new().expect("browser peer connection must construct");
        let channel = connection.create_data_channel("invalid-frame-accounting");
        let _on_message = wire_data_channel_messages(&channel, callback, ChannelLane::default());
        let event = MessageEvent::new("message").expect("browser message event must construct");

        for _ in 0..32 {
            assert!(channel
                .dispatch_event(&event)
                .expect("browser message event must dispatch"));
        }
        let _ = JsFuture::from(js_sys::Promise::resolve(&JsValue::UNDEFINED)).await;

        channel.set_onmessage(None);
        connection.close();
        assert_eq!(invalid_frames.get(), 32);
    }

    #[wasm_bindgen_test]
    async fn test_round_robin_backend_checks_permit_before_real_browser_send() {
        let connection = RtcPeerConnection::new().expect("browser peer connection must construct");
        let channel = connection.create_data_channel("permit-boundary-test");
        let delivery = Arc::new(DeliveryTracker::default());
        let pool = Rc::new(ChannelPool::from_vec(vec![(
            channel,
            Arc::clone(&delivery),
        )]));
        let backend = WebSysWebrtcConnection::new(
            connection.clone(),
            pool,
            Notifier::default(),
            ConnectionStateCell::new(),
            ignored_callback(),
        );

        let result = backend
            .send_frame(
                TransportMessage::Custom(Bytes::from_static(&[1, 2, 3])),
                ChannelLane::default(),
                SendPermit::new(|| false),
            )
            .await;

        connection.close();
        assert!(matches!(result, Err(Error::SendPermitRevoked)));
        assert_eq!(delivery.enqueued().load(Ordering::SeqCst), 0);
    }
    /// Offset exhaustion must reject before browser IO and before irrevocable admission.
    #[wasm_bindgen_test]
    async fn test_browser_offset_exhaustion_preserves_generation_and_counter() {
        let (_peer, state, connection) = test_backend();
        let enqueued = Arc::new(AtomicU64::new(u64::MAX));
        let permit = SendPermit::always();
        let acceptance = permit.acceptance();
        let called = Rc::new(Cell::new(false));
        let observed = Rc::clone(&called);
        let result = connection
            .send_after_permit(permit, Arc::clone(&enqueued), 1, move || {
                observed.set(true);
                Ok(())
            })
            .await;
        assert!(matches!(result, Err(Error::SendByteCountOverflow)));
        assert!(!called.get());
        assert!(!acceptance.is_irrevocable());
        assert_ne!(state.snapshot().state(), TransportConnectionState::Closed);
        assert_eq!(enqueued.load(Ordering::SeqCst), u64::MAX);
    }
}
