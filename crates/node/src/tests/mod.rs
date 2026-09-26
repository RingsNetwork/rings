/// Overlay every test fixture is published for and verified against.
pub(crate) const TEST_NETWORK_ID: u32 = 1;

/// ICE servers of every processor and provider fixture, native and browser: none, so peers
/// gather host candidates only.
///
/// All peers of these tests run in one process or one page, so host candidates connect them.
/// An external STUN server would only add a network dependency whose latency no test
/// controls. In the browser it would also sit inside `createOffer`/`answerOffer`, which wait
/// for ICE gathering to complete, ahead of every awaited event.
pub(crate) const TEST_ICE_SERVERS: &str = "";

pub(crate) mod activity;

#[cfg(feature = "node")]
pub mod native;
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub mod wasm;
