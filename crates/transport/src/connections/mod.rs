//! Default using `WebrtcConnection` for native environment.
//! Plus a `WebSysWebrtcConnection` for wasm environment.
//! Also provide a `DummyConnection` for testing.

/// The future of a backend's `send_message_with_permit`, as `async_trait` boxes it for
/// [`ConnectionInterface`](crate::core::transport::ConnectionInterface): `Send` on native,
/// unconstrained in the browser.
#[cfg(rings_transport_backend)]
type LinkSendFuture<'a> = std::pin::Pin<
    Box<
        rings_runtime::maybe_send!(
            dyn std::future::Future<Output = crate::error::Result<crate::delivery::DeliveryFuture>>
                + 'a
        ),
    >,
>;

/// The [`ConnectionInterface`](crate::core::transport::ConnectionInterface) methods a
/// callback-backed backend delegates to its `callback`: the custom send and the lane's two
/// credit accessors. Expanded inside each backend's impl, so a credit accessor is added in one
/// place; every backend meets the credit law through the callback alone (see
/// [`crate::callback::link_send`]).
///
/// `send_message_with_permit` is written in the form `async_trait` gives an `async fn`, since
/// `async_trait` does not see into a macro expanded inside its impl.
#[cfg(rings_transport_backend)]
macro_rules! delegate_to_callback {
    () => {
        fn send_message_with_permit<'life0, 'async_trait>(
            &'life0 self,
            payload: bytes::Bytes,
            lane: $crate::core::pool::ChannelLane,
            permit: $crate::core::transport::SendPermit,
        ) -> $crate::connections::LinkSendFuture<'async_trait>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(self.callback.send_custom(self, payload, lane, permit))
        }

        fn reserve_send_credit(
            &self,
            lane: $crate::core::pool::ChannelLane,
        ) -> $crate::delivery::SendCreditWait<$crate::error::Error> {
            Box::pin(self.callback.reserve_credit(lane))
        }

        fn send_credit_view(
            &self,
            lane: $crate::core::pool::ChannelLane,
        ) -> $crate::core::transport::LaneCreditView {
            self.callback.send_credit_view(lane)
        }
    };
}

/// In-memory deterministic test transport backend.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod dummy;
/// Native `webrtc-rs` transport backend.
#[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
mod native_webrtc;
/// Browser `web-sys` WebRTC transport backend.
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
mod web_sys_webrtc;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub use crate::connections::dummy::controlled as dummy_controlled;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub use crate::connections::dummy::DummyConnection;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub use crate::connections::dummy::DummyTransport;
#[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
pub use crate::connections::native_webrtc::WebrtcConnection;
#[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
pub use crate::connections::native_webrtc::WebrtcTransport;
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
pub use crate::connections::web_sys_webrtc::WebSysWebrtcConnection;
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
pub use crate::connections::web_sys_webrtc::WebSysWebrtcTransport;

/// Stable monotone observation that a native or dummy connection generation's physical teardown
/// completed successfully.
#[cfg(all(
    any(feature = "native-webrtc", feature = "dummy"),
    not(target_family = "wasm")
))]
#[derive(Clone)]
pub struct PhysicalCloseWitness {
    /// Monotone watch channel set to `true` once physical teardown completes.
    completed: tokio::sync::watch::Receiver<bool>,
}

#[cfg(all(
    any(feature = "native-webrtc", feature = "dummy"),
    not(target_family = "wasm")
))]
impl PhysicalCloseWitness {
    /// Wrap a monotone `false -> true` watch receiver into a [`PhysicalCloseWitness`].
    pub(crate) fn new(completed: tokio::sync::watch::Receiver<bool>) -> Self {
        Self { completed }
    }

    /// Return `true` only after the underlying physical connection teardown has completed.
    pub fn is_complete(&self) -> bool {
        *self.completed.borrow()
    }

    /// Await the physical close outcome: `true` once the connection's close path has completed,
    /// `false` if every holder of the connection dropped without completing teardown.
    ///
    /// Law (no lost wake-up): `wait_for` inspects the stored state before suspending, and the
    /// value transitions `false -> true` at most once.
    pub async fn completed(mut self) -> bool {
        self.completed
            .wait_for(|completed| *completed)
            .await
            .is_ok()
    }
}

/// Alias retained for callers observing native connection teardown completion.
#[cfg(all(
    any(feature = "native-webrtc", feature = "dummy"),
    not(target_family = "wasm")
))]
pub type NativePhysicalCloseWitness = PhysicalCloseWitness;

/// Subscribe a [`PhysicalCloseWitness`] from a type-erased [`ConnectionRef`](crate::connection_ref::ConnectionRef)
/// by downcasting via [`ConnectionInterface::as_any`](crate::core::transport::ConnectionInterface::as_any)
/// instead of polluting [`ConnectionInterface`](crate::core::transport::ConnectionInterface) with
/// backend-specific witness methods.
#[cfg(all(
    any(feature = "native-webrtc", feature = "dummy"),
    not(target_family = "wasm")
))]
pub fn physical_close_witness(
    connection: &crate::connection_ref::ConnectionRef<crate::core::transport::DynConnection>,
) -> crate::error::Result<PhysicalCloseWitness> {
    let conn = connection.upgrade()?;
    let any = conn.as_any();
    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    if let Some(native) = any.downcast_ref::<WebrtcConnection>() {
        return Ok(native.physical_close_witness());
    }
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    if let Some(dummy) = any.downcast_ref::<DummyConnection>() {
        return Ok(dummy.physical_close_witness());
    }
    Err(crate::error::Error::UnsupportedOperation(
        "physical_close_witness".to_string(),
    ))
}

/// Read a [`DummyConnection`]'s generation identifier through [`ConnectionInterface::as_any`](crate::core::transport::ConnectionInterface::as_any).
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub fn dummy_generation_id(
    connection: &crate::connection_ref::ConnectionRef<crate::core::transport::DynConnection>,
) -> crate::error::Result<String> {
    let conn = connection.upgrade()?;
    let dummy = conn
        .as_any()
        .downcast_ref::<DummyConnection>()
        .ok_or_else(|| {
            crate::error::Error::UnsupportedOperation("dummy_generation_id".to_string())
        })?;
    Ok(dummy.generation_id().to_string())
}

/// Force a [`DummyConnection`]'s connection state without firing its callback, via
/// [`ConnectionInterface::as_any`](crate::core::transport::ConnectionInterface::as_any).
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub fn force_dummy_connection_state_without_callback(
    connection: &crate::connection_ref::ConnectionRef<crate::core::transport::DynConnection>,
    state: crate::core::transport::TransportConnectionState,
) -> crate::error::Result<()> {
    let conn = connection.upgrade()?;
    let dummy = conn
        .as_any()
        .downcast_ref::<DummyConnection>()
        .ok_or_else(|| {
            crate::error::Error::UnsupportedOperation(
                "force_dummy_connection_state_without_callback".to_string(),
            )
        })?;
    dummy.force_connection_state_without_callback(state);
    Ok(())
}

/// Force a [`DummyConnection`]'s channel-open override without firing its callback, via
/// [`ConnectionInterface::as_any`](crate::core::transport::ConnectionInterface::as_any).
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub fn force_dummy_channel_open_without_callback(
    connection: &crate::connection_ref::ConnectionRef<crate::core::transport::DynConnection>,
    open: Option<bool>,
) -> crate::error::Result<()> {
    let conn = connection.upgrade()?;
    let dummy = conn
        .as_any()
        .downcast_ref::<DummyConnection>()
        .ok_or_else(|| {
            crate::error::Error::UnsupportedOperation(
                "force_dummy_channel_open_without_callback".to_string(),
            )
        })?;
    dummy.force_data_channel_open_without_callback(open);
    Ok(())
}

/// Read the total payload frames dispatched by a [`DummyTransport`] behind a type-erased
/// [`SharedTransport`](crate::core::transport::SharedTransport), returning `0` for non-dummy
/// transports.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub fn dispatched_payload_frames(transport: &crate::core::transport::SharedTransport) -> usize {
    transport
        .as_any()
        .downcast_ref::<DummyTransport>()
        .map_or(0, DummyTransport::dispatched_payload_frames)
}
