//! Default using `WebrtcConnection` for native environment.
//! Plus a `WebSysWebrtcConnection` for wasm environment.
//! Also provide a `DummyConnection` for testing.

#[cfg(feature = "dummy")]
mod dummy;
#[cfg(feature = "native-webrtc")]
mod native_webrtc;
#[cfg(feature = "web-sys-webrtc")]
mod web_sys_webrtc;

#[cfg(feature = "dummy")]
pub use crate::connections::dummy::DummyConnection;
#[cfg(feature = "dummy")]
pub use crate::connections::dummy::DummyTransport;
#[cfg(feature = "native-webrtc")]
pub use crate::connections::native_webrtc::WebrtcConnection;
#[cfg(feature = "native-webrtc")]
pub use crate::connections::native_webrtc::WebrtcTransport;
#[cfg(feature = "web-sys-webrtc")]
pub use crate::connections::web_sys_webrtc::WebSysWebrtcConnection;
#[cfg(feature = "web-sys-webrtc")]
pub use crate::connections::web_sys_webrtc::WebSysWebrtcTransport;
