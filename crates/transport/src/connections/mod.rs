//! Default using `WebrtcConnection` for native environment.
//! Plus a `WebSysWebrtcConnection` for wasm environment.
//! Also provide a `DummyConnection` for testing.

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
