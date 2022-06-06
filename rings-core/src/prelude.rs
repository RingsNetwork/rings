//! prelude

#[cfg(feature = "wasm")]
pub use web_sys::RtcSdpType as RTCSdpType;
#[cfg(not(feature = "wasm"))]
pub use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

pub use dashmap;
pub use futures;
pub use uuid;
pub use web3;

pub use url;
#[cfg(feature = "wasm")]
pub use web_sys;
#[cfg(feature = "default")]
pub use webrtc;

#[cfg(feature = "wasm")]
pub use js_sys;
#[cfg(feature = "wasm")]
pub use wasm_bindgen;
pub use web3::types::Address;
