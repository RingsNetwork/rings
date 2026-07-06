//! Encrypted onion circuit data plane.
//!
//! Security model: forward layers are wrapped from exit to entry with the selected hop session
//! public keys. Each relay decrypts exactly one ElGamal-AEAD layer and learns only the immediate
//! next hop plus an opaque inner layer. Backward frames carry a client-encrypted AEAD payload and
//! relays forward them with local return state.

mod codec;
mod crypto;
mod limiter;
mod protocol;
mod reducer;
mod shell;

#[cfg(test)]
mod tests;

use bytes::Bytes;
pub use codec::OnionCircuitEvent;
pub use crypto::encode_initial_forward;
pub use crypto::route_first_hop;
pub use crypto::send_backward;
pub use protocol::OnionCircuitCapabilities;
pub use protocol::OnionCircuitProtocol;
pub use reducer::OnionCircuitEffect;
pub use reducer::OnionCircuitState;
use rings_core::dht::Did;
use rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext;
use rings_core::ecc::PublicKey;
use rings_core::message::MessageVerification;
use serde::Deserialize;
use serde::Serialize;
pub use shell::OnionCircuitHandler;
pub use shell::OnionCircuitShell;

/// Namespace used by route-aware onion circuit messages.
pub const ONION_CIRCUIT_NAMESPACE: &str = "onion-circuit";

/// Security mode implemented by the current circuit wire format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionCircuitSecurity {
    /// Layered ElGamal-AEAD forward frames with client-encrypted backward payloads.
    LayeredAead,
}

/// Current circuit security mode.
pub const ONION_CIRCUIT_SECURITY: OnionCircuitSecurity = OnionCircuitSecurity::LayeredAead;

/// Maximum route length encoded by local clients and maximum relay hop-budget value accepted per
/// decrypted layer.
pub const MAX_ONION_CIRCUIT_HOPS: u8 = 8;

pub(super) const MAX_ONION_RELAY_CIRCUITS: usize = 1024;
pub(super) const ONION_RELAY_RETURN_TTL_MS: u128 = 120_000;
pub(super) const ONION_CRYPTO_LIMIT_WINDOW_MS: u128 = 60_000;
pub(super) const MAX_ONION_CRYPTO_OPS_PER_WINDOW: u32 = 4096;
pub(super) const ONION_AEAD_NAMESPACE: &str = "rings-node:onion-circuit:v1";

/// Opaque application payload carried over a route-aware onion circuit.
///
/// The circuit layer knows only the service label and authenticated bytes. TCP, HTTPS, or future
/// adapters own their own payload algebra outside the encrypted circuit core.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionCircuitPayload {
    /// Application service label selected from the onion-exit registry.
    pub service: String,
    /// Adapter-owned payload bytes.
    pub body: Bytes,
}

impl OnionCircuitPayload {
    /// Build an opaque circuit payload for one application service.
    pub fn new(service: impl Into<String>, body: impl Into<Bytes>) -> Self {
        Self {
            service: service.into(),
            body: body.into(),
        }
    }

    /// Return whether this payload belongs to `service`.
    pub fn is_service(&self, service: &str) -> bool {
        self.service == service
    }
}

/// Client-decrypted backward payload plus the exit session proof that authenticated it.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionAuthenticatedPayload {
    /// Exit session signature over the backward payload transcript.
    pub authentication: MessageVerification,
    /// Application payload signed by the exit and encrypted to the client.
    pub payload: OnionCircuitPayload,
}

/// Client return key encrypted into the exit layer.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionClientReturn {
    /// Client session public key used for backward AEAD payloads.
    pub session_public_key: PublicKey<33>,
}

impl OnionClientReturn {
    /// Build a client return descriptor.
    pub const fn new(session_public_key: PublicKey<33>) -> Self {
        Self { session_public_key }
    }
}

/// Public unlinkable circuit correlation id.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OnionCircuitId([u8; 16]);

impl OnionCircuitId {
    /// Build a circuit id from random bytes.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Generate a random circuit id.
    pub fn random() -> Self {
        Self(rand::random())
    }
}

/// Forward direction: client -> relays -> exit.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionForwardFrame {
    /// Random circuit correlation id.
    pub circuit_id: OnionCircuitId,
    /// AEAD-encrypted layer for the receiving hop.
    pub layer: AeadCiphertext,
}

/// Backward direction: exit -> relays -> client.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionBackwardFrame {
    /// Random circuit correlation id.
    pub circuit_id: OnionCircuitId,
    /// AEAD payload encrypted to the client session public key.
    pub payload: AeadCiphertext,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(super) enum OnionForwardLayer {
    Relay {
        next_hop: Did,
        remaining_hops: u8,
        inner: AeadCiphertext,
    },
    Exit {
        client: OnionClientReturn,
        payload: OnionCircuitPayload,
    },
}
