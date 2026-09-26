//! The onion loop data plane (#834 Phase 2a, #843).
//!
//! A circuit is a client-sealed loop `client → g → r → h → r′ → g → client` of Sphinx cells
//! (`onion::sphinx`): every edge carries one cell `α‖β‖γ‖y` of the loop's class `b`, exactly `b`
//! bytes, and every hop, `relay` or symbol, runs the same step `Hop_i`:
//!
//! ```text
//! wire cell ──decode──▶ Hop(from, cell) ──shell──▶ hop (pure, over the admission state)
//!                                                   ├─ Relayed(next, cell)   ─▶ link sender
//!                                                   ├─ Consumed(f, ā, v, υ)  ─▶ ⟦f⟧ in the algebra
//!                                                   ├─ Returned(t_⋄, cell)   ─▶ the tag's session
//!                                                   └─ Refused | Dropped
//! link facts (core Admitted/Retired, reconcile tick) ─▶ admission link table
//! ```
//!
//! Relays are stateless: a relay keeps no return state, since the loop returns through fresh
//! positions the client sealed, and the only state of the data plane is the admission state
//! (ledgers and the replay store, `admission`), which the shell owns. There is no per-edge cell
//! AEAD: header integrity is `γ`, carry integrity is the consumer's AEZ authenticator under a key
//! only the client and the consumer hold, and the previous hop is the authenticated transport
//! link. A reply carries no signature of the exit (#834, Prop. Reply authentication): it opens
//! under `k_{c_n}`, which only the client and the exit's process hold, so replies are deniable.
//!
//! Every link emits at the constant rate of `send_outbox` (#880), real cells replacing cover.

mod admission;
mod cell;
mod codec;
mod expiry;
mod feed;
mod hop;
mod loops;
mod protocol;
mod reducer;
mod send_outbox;
mod shell;
mod tags;

#[cfg(all(test, rings_native))]
mod tests;

pub(crate) use admission::OnionAdmissionCharge;
pub(crate) use admission::OnionAdmissionLayer;
pub(crate) use admission::OnionAdmissionRejection;
pub(crate) use admission::OnionAdmissionState;
#[cfg(test)]
pub(crate) use admission::OnionReplayFilterKey;
pub(crate) use admission::ONION_ADMISSION_SENDER_UNITS;
pub use cell::OnionCellBucket;
pub(crate) use expiry::OnionExpiry;
pub(crate) use feed::OnionLinkFeed;
pub(crate) use loops::OnionLoopClient;
pub(crate) use protocol::OnionCircuitProtocol;
pub(crate) use reducer::OnionCircuitEffect;
use rings_core::dht::Did;
pub use send_outbox::OnionIdleFloor;
pub(crate) use send_outbox::OnionLinkSender;
use serde::Deserialize;
use serde::Serialize;
pub(crate) use shell::OnionAlgebra;
pub(crate) use shell::OnionApplicationInput;
pub(crate) use shell::OnionCircuitShell;
pub(crate) use shell::OnionInterpretation;
#[cfg(all(test, rings_native))]
pub(crate) use shell::OnionLinkWitness;
pub(crate) use tags::OnionClientTags;
pub(crate) use tags::OnionReply;
pub(crate) use tags::OnionReplySink;

/// Namespace of the onion data plane.
pub const ONION_CIRCUIT_NAMESPACE: &str = "onion-circuit";

/// `X₀ = V − Q`, the offset of a loop's expiry from its build quantum (#834 D6).
pub(crate) const ONION_FORWARD_PAYLOAD_TTL_MS: u128 = 120_000;
/// `Q = 30 s`, the expiry quantum.
pub(crate) const ONION_FORWARD_EXPIRY_QUANTUM_MS: u128 = 30_000;
/// `V = X₀ + Q = 150 s`, the admission window: a layer is admissible at `arr` iff
/// `arr < x ≤ arr + V`.
///
/// Law: the replay filter of `x` lives until `x`, so no admissible layer outlives the witness of
/// its admission (L9).
pub(crate) const ONION_FORWARD_MAX_VALIDITY_MS: u128 =
    ONION_FORWARD_PAYLOAD_TTL_MS + ONION_FORWARD_EXPIRY_QUANTUM_MS;

/// The next hop of one cell: the peer it is sent to over their direct link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionLink {
    /// The next hop's DID.
    peer: Did,
}

impl OnionLink {
    /// The link to `peer`.
    pub(crate) const fn new(peer: Did) -> Self {
        Self { peer }
    }
}

/// The replay nonce `ν` of one layer, admitted at most once per expiry (#834 L9, D6″).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OnionReplayNonce(pub(crate) [u8; 16]);

impl OnionReplayNonce {
    /// Build a nonce from its bytes.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Return the nonce bytes, the `ν` field of the uniform layer.
    pub(crate) const fn to_bytes(self) -> [u8; 16] {
        self.0
    }
}
