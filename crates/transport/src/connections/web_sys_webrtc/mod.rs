use std::rc::Rc;
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
const DELIVERY_POLL_INTERVAL_MS: u64 = 300;

/// A data channel paired with a monotonic counter of the total bytes ever
/// enqueued onto it. See the native backend for the rationale; the counter
/// lets the delivery future tell, per message, whether the bytes have left the
/// local send buffer (`enqueued_total - buffered_amount`).
type TrackedChannel = (RtcDataChannel, Arc<AtomicU64>);

fn send_after_permit<T>(permit: SendPermit, send: impl FnOnce() -> Result<T>) -> Result<T> {
    if !permit.allows() {
        return Err(Error::SendPermitRevoked);
    }
    let value = send()?;
    permit.mark_accepted();
    Ok(value)
}

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
    async fn send_with_permit(
        &self,
        msg: TransportMessage,
        permit: SendPermit,
    ) -> Result<DeliveryFuture> {
        let (channel, enqueued) = self.select()?;
        let data = rings_codec::serialize(&msg)?;
        // `send_with_u8_array` is synchronous, so there's no interleaving to
        // guard; just advance `enqueued` ONLY after a successful send. Advancing
        // first would, on a rejected send, leave the counter ahead of the bytes
        // actually buffered, making earlier messages' delivery futures resolve
        // early on phantom bytes (`enqueued_total - buffered_amount`).
        if let Err(e) = send_after_permit(permit, || {
            channel
                .send_with_u8_array(&data)
                .map_err(Error::WebSysWebrtc)
        }) {
            tracing::error!("{:?}, Data size: {:?}", e, data.len());
            return Err(e);
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
    // `Rc`, not `Arc`: the browser backend is single-threaded (the `ConnectionInterface` impl is
    // `?Send`), so the channel pool is never shared across threads.
    webrtc_data_channel: Rc<RoundRobinPool<TrackedChannel>>,
    webrtc_data_channel_state_notifier: Notifier,
    connection_state: ConnectionStateCell,
    /// Negotiated SCTP `max_message_size` (RFC 8841), parsed from the remote SDP at handshake.
    /// `0` means not yet negotiated. Parsed identically to native for consistent behaviour.
    remote_max_message_size: Arc<AtomicUsize>,
}

/// [WebSysWebrtcTransport] manages all the [WebSysWebrtcConnection] and
/// provides methods to create, get and close connections.
pub struct WebSysWebrtcTransport {
    ice_servers: Vec<IceServer>,
    pool: Pool<WebSysWebrtcConnection>,
    inbound_frames: Arc<InboundFrameCapacity>,
}

impl WebSysWebrtcConnection {
    fn new(
        webrtc_conn: RtcPeerConnection,
        webrtc_data_channel: Rc<RoundRobinPool<TrackedChannel>>,
        webrtc_data_channel_state_notifier: Notifier,
        connection_state: ConnectionStateCell,
    ) -> Self {
        Self {
            webrtc_conn,
            webrtc_data_channel,
            webrtc_data_channel_state_notifier,
            connection_state,
            remote_max_message_size: Arc::new(AtomicUsize::new(0)),
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
        _udp_port_range: Option<WebrtcUdpPortRange>,
    ) -> Self {
        let ice_servers = parse_ice_servers_or_warn(ice_servers, "web-sys-webrtc");

        Self {
            ice_servers,
            pool: Pool::new(),
            inbound_frames: Arc::new(InboundFrameCapacity::new()),
        }
    }
}

#[async_trait(?Send)]
impl ConnectionInterface for WebSysWebrtcConnection {
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
        // fall back to the interop default. Same parsing as native (consistent behaviour).
        match self.remote_max_message_size.load(Ordering::SeqCst) {
            0 => MAX_DATA_CHANNEL_MESSAGE_SIZE,
            n => n,
        }
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

        let set_local_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        set_local_init.set_sdp(&sdp);

        let promise = self.webrtc_conn.set_local_description(&set_local_init);
        JsFuture::from(promise).await.map_err(Error::WebSysWebrtc)?;

        self.webrtc_gather().await
    }

    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp> {
        tracing::debug!("webrtc_answer_offer, offer: {offer:?}");
        // Read the negotiated limit, but record it only after the *whole* answer path
        // (setRemoteDescription + createAnswer + setLocalDescription + gather) has succeeded, so a
        // failure midway does not leave a partially-updated connection carrying a stale size.
        let negotiated_max_message_size = effective_max_message_size(&offer);

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
        self.webrtc_conn.close();
        Ok(())
    }
}

fn decode_data_channel_message(
    data: JsValue,
    capacity: &Arc<InboundFrameCapacity>,
) -> Result<Option<(Vec<u8>, crate::callback::InboundFramePermit)>> {
    let buffer = data.dyn_into::<js_sys::ArrayBuffer>().map_err(|_| {
        Error::DataChannelMessage(
            "received a non-ArrayBuffer value after configuring binaryType".to_string(),
        )
    })?;
    let bytes = buffer.byte_length() as usize;
    if bytes == 0 {
        return Ok(None);
    }
    if bytes > MAX_DATA_CHANNEL_MESSAGE_SIZE {
        return Err(Error::DataChannelMessage(format!(
            "inbound frame of {bytes} bytes exceeds the {MAX_DATA_CHANNEL_MESSAGE_SIZE}-byte protocol ceiling"
        )));
    }
    let permit = capacity.try_acquire(bytes).ok_or_else(|| {
        Error::DataChannelMessage(format!("inbound frame capacity exceeded for {bytes} bytes"))
    })?;
    let message = js_sys::Uint8Array::new(&buffer).to_vec();
    Ok(Some((message, permit)))
}

fn wire_received_data_channels(
    webrtc_conn: &RtcPeerConnection,
    inner_cb: Rc<InnerTransportCallback>,
    inbound_frames: Arc<InboundFrameCapacity>,
) {
    // Inbound channels carry messages only. One remote-created channel closing
    // does not prove the SCTP association is gone; outbound-pool state owns
    // readiness and emits the terminal data-channel callback when all close.
    let admitted_channels = AtomicUsize::new(0);
    let on_data_channel = Box::new(move |event: RtcDataChannelEvent| {
        let channel = event.channel();
        if !admit_inbound_data_channel(&admitted_channels) {
            tracing::warn!(
                peer = %inner_cb.cid,
                label = channel.label(),
                "rejected excess inbound data channel"
            );
            channel.close();
            return;
        }
        channel.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);
        tracing::debug!(label = channel.label(), "new received data channel");

        let message_cb = inner_cb.clone();
        let frame_capacity = inbound_frames.clone();
        let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
            let cb = message_cb.clone();
            match decode_data_channel_message(event.data(), &frame_capacity) {
                Ok(Some((message, permit))) => {
                    spawn_local(async move {
                        tracing::debug!(
                            peer = %cb.cid,
                            bytes = message.len(),
                            "received data-channel message"
                        );
                        cb.on_message(&message.into()).await;
                        drop(permit);
                    });
                }
                Ok(None) => {
                    tracing::debug!(peer = %cb.cid, "received empty data-channel message");
                }
                Err(error) => {
                    tracing::warn!(peer = %cb.cid, %error, "rejected data-channel message");
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        channel.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
    });

    let callback = Closure::wrap(on_data_channel as Box<dyn FnMut(RtcDataChannelEvent)>);
    webrtc_conn.set_ondatachannel(Some(callback.as_ref().unchecked_ref()));
    callback.forget();
}

fn wire_peer_connection_state(
    webrtc_conn: &RtcPeerConnection,
    inner_cb: Rc<InnerTransportCallback>,
    connection_state: ConnectionStateCell,
) {
    let state_source = webrtc_conn.clone();
    let on_state_change = Closure::wrap(Box::new(move |_| {
        let state_change = state_source.connection_state().into();
        tracing::debug!("peer connection state changed: {state_change:?}");
        connection_state.observe_webrtc(state_change);
        let cb = inner_cb.clone();
        spawn_local(async move { cb.on_peer_connection_state_change(state_change).await });
    }) as Box<dyn FnMut(web_sys::Event)>);
    webrtc_conn.set_onconnectionstatechange(Some(on_state_change.as_ref().unchecked_ref()));
    on_state_change.forget();
}

fn create_outbound_data_channels(
    webrtc_conn: &RtcPeerConnection,
    channel_pool: &Rc<RoundRobinPool<TrackedChannel>>,
    inner_cb: &Rc<InnerTransportCallback>,
    connection_state: &ConnectionStateCell,
) -> Result<()> {
    for index in 0..DATA_CHANNEL_POOL_SIZE {
        let channel = webrtc_conn.create_data_channel(&format!("rings_data_channel_{index}"));
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
                    cb.on_data_channel_open().await;
                }
            });
        }) as Box<dyn FnMut()>);
        channel.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();

        let close_pool = channel_pool.clone();
        let close_cb = inner_cb.clone();
        let close_state = connection_state.clone();
        let on_close = Closure::wrap(Box::new(move || {
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
                    cb.on_data_channel_close().await;
                }
            });
        }) as Box<dyn FnMut()>);
        channel.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        on_close.forget();

        channel_pool.push((channel, Arc::new(AtomicU64::new(0))))?;
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
        if let Ok(existed_conn) = self.pool.connection(cid) {
            if existed_conn.webrtc_connection_state().occupies_peer_slot() {
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
        let connection_state = ConnectionStateCell::new();
        let inner_cb = Rc::new(InnerTransportCallback::new(
            cid,
            callback,
            webrtc_data_channel_state_notifier.clone(),
        ));

        let channel_pool = Rc::new(RoundRobinPool::default());
        // Wire open/close on the channels this side creates (the pool), not only
        // on received channels: a received channel's `onopen` can be missed if
        // it opens before the handler is registered, so `on_data_channel_open`
        // (and thus `join_dht`) would never fire. Created channels are wired
        // before they can open, so this is reliable.
        wire_received_data_channels(&webrtc_conn, inner_cb.clone(), self.inbound_frames.clone());
        wire_peer_connection_state(&webrtc_conn, inner_cb.clone(), connection_state.clone());
        create_outbound_data_channels(&webrtc_conn, &channel_pool, &inner_cb, &connection_state)?;

        //
        // Construct the Connection
        //
        let conn = WebSysWebrtcConnection::new(
            webrtc_conn,
            channel_pool,
            webrtc_data_channel_state_notifier,
            connection_state,
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use bytes::Bytes;
    use wasm_bindgen_test::wasm_bindgen_test;
    use wasm_bindgen_test::wasm_bindgen_test_configure;

    use super::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    fn rejected_permit_does_not_call_browser_send_primitive() {
        let called = Rc::new(Cell::new(false));
        let observed = called.clone();
        let result = send_after_permit(SendPermit::new(|| false), move || {
            observed.set(true);
            Ok(())
        });

        assert!(matches!(result, Err(Error::SendPermitRevoked)));
        assert!(!called.get());
    }

    #[wasm_bindgen_test]
    fn browser_send_failure_does_not_mark_permit_accepted() {
        let permit = SendPermit::always();
        let acceptance = permit.acceptance();
        let result = send_after_permit(permit, || {
            Err::<(), _>(Error::DataChannelMessage(
                "injected browser send failure".to_string(),
            ))
        });

        assert!(matches!(result, Err(Error::DataChannelMessage(_))));
        assert!(!acceptance.is_accepted());
    }

    #[wasm_bindgen_test]
    fn oversized_browser_frame_is_rejected_before_capacity_or_copy() {
        let capacity = Arc::new(InboundFrameCapacity::new());
        let length = u32::try_from(MAX_DATA_CHANNEL_MESSAGE_SIZE + 1)
            .expect("protocol ceiling must fit in a JavaScript array length");
        let array = js_sys::Uint8Array::new_with_length(length);

        let result = decode_data_channel_message(array.buffer().into(), &capacity);

        assert!(matches!(result, Err(Error::DataChannelMessage(_))));
        assert!(capacity.try_acquire(1).is_some());
    }

    #[wasm_bindgen_test]
    async fn round_robin_backend_checks_permit_before_real_browser_send() {
        let connection = RtcPeerConnection::new().expect("browser peer connection must construct");
        let channel = connection.create_data_channel("permit-boundary-test");
        let enqueued = Arc::new(AtomicU64::new(0));
        let pool = RoundRobinPool::from_vec(vec![(channel, enqueued.clone())]);

        let result = pool
            .send_with_permit(
                TransportMessage::Custom(Bytes::from_static(&[1, 2, 3])),
                SendPermit::new(|| false),
            )
            .await;

        connection.close();
        assert!(matches!(result, Err(Error::SendPermitRevoked)));
        assert_eq!(enqueued.load(Ordering::SeqCst), 0);
    }
}
