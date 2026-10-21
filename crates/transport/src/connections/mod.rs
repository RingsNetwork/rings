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

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod dummy;
#[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
mod native_webrtc;
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
mod web_sys_webrtc;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub use crate::connections::dummy::controlled as dummy_controlled;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub use crate::connections::dummy::DummyConnection;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub use crate::connections::dummy::DummyTransport;
#[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
pub use crate::connections::native_webrtc::NativePhysicalCloseWitness;
#[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
pub use crate::connections::native_webrtc::WebrtcConnection;
#[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
pub use crate::connections::native_webrtc::WebrtcTransport;
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
pub use crate::connections::web_sys_webrtc::WebSysWebrtcConnection;
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
pub use crate::connections::web_sys_webrtc::WebSysWebrtcTransport;
