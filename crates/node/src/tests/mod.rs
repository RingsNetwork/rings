/// Overlay every test fixture is published for and verified against.
pub(crate) const TEST_NETWORK_ID: u32 = 1;

#[cfg(feature = "node")]
pub mod native;
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub mod wasm;
