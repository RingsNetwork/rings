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
#[cfg(feature = "node")]
pub(crate) use crypto::OnionCircuitPath;
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
pub use shell::OnionCircuitExitFrame;
pub use shell::OnionCircuitHandler;
pub use shell::OnionCircuitShell;

use super::OnionServiceName;
use crate::error::Result;

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
pub(super) const ONION_FORWARD_PAYLOAD_TTL_MS: u128 = 120_000;
pub(super) const ONION_CRYPTO_LIMIT_WINDOW_MS: u128 = 60_000;
pub(super) const MAX_ONION_CRYPTO_OPS_PER_WINDOW: u32 = 4096;
pub(super) const ONION_AEAD_NAMESPACE: &str = "rings-node:onion-circuit:v1";

/// Opaque application payload carried over a route-aware onion circuit.
///
/// The circuit layer knows only the service label and authenticated bytes. TCP, HTTPS, or future
/// adapters own their own payload algebra outside the encrypted circuit core.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionCircuitPayload {
    /// Canonical application service selected from the onion-exit registry.
    pub service: OnionServiceName,
    /// Adapter-owned payload bytes.
    pub body: Bytes,
}

impl OnionCircuitPayload {
    /// Build an opaque circuit payload for one already-validated application service.
    pub fn new(service: OnionServiceName, body: impl Into<Bytes>) -> Self {
        Self {
            service,
            body: body.into(),
        }
    }

    /// Build an opaque circuit payload from an untrusted service string.
    pub fn try_new(service: impl AsRef<str>, body: impl Into<Bytes>) -> Result<Self> {
        Ok(Self::new(OnionServiceName::parse(service)?, body))
    }

    /// Return the canonical service selected by this payload.
    pub fn service(&self) -> &str {
        self.service.as_str()
    }

    /// Return the canonical service name selected by this payload.
    pub fn service_name(&self) -> &OnionServiceName {
        &self.service
    }

    /// Return whether this payload belongs to the already canonical `service`.
    pub fn is_service(&self, service: &OnionServiceName) -> bool {
        &self.service == service
    }

    /// Return whether this payload belongs to `service` after service-name canonicalization.
    pub fn matches_service(&self, service: &str) -> bool {
        self.service.matches(service)
    }
}

/// Client-decrypted backward payload plus the exit session proof that authenticated it.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionAuthenticatedPayload {
    /// Client/exit-only return id encrypted in the exit layer.
    pub return_id: OnionReturnId,
    /// Random per-frame nonce signed by the exit and consumed by the client adapter.
    pub nonce: OnionBackwardNonce,
    /// Exit session signature over the backward payload transcript.
    pub authentication: MessageVerification,
    /// Application payload signed by the exit and encrypted to the client.
    pub payload: OnionCircuitPayload,
}

/// Client/exit-only id used to authenticate backward payloads.
///
/// This id is encrypted inside the exit layer and never appears as a relay edge header. Relays may
/// rewrite [`OnionCircuitId`] while forwarding backward frames; the client adapter accepts a
/// backward payload only when this signed return id matches its pending request or stream.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OnionReturnId([u8; 16]);

impl OnionReturnId {
    /// Build a return id from random bytes.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Generate a random return id.
    pub fn random() -> Self {
        Self(rand::random())
    }
}

/// Random nonce for one backward payload on a circuit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OnionBackwardNonce([u8; 16]);

impl OnionBackwardNonce {
    /// Build a nonce from random bytes.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Generate a random backward-payload nonce.
    pub fn random() -> Self {
        Self(rand::random())
    }
}

/// Random nonce for one forward exit payload on a circuit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OnionForwardNonce([u8; 16]);

impl OnionForwardNonce {
    /// Build a nonce from random bytes.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Generate a random forward-payload nonce.
    pub fn random() -> Self {
        Self(rand::random())
    }
}

/// Backward payload that has passed exit identity, signature, and freshness checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionVerifiedPayload {
    /// Verified client/exit return id.
    pub return_id: OnionReturnId,
    /// Random per-frame nonce to be consumed exactly once by the client adapter.
    pub nonce: OnionBackwardNonce,
    /// Verified application payload.
    pub payload: OnionCircuitPayload,
}

/// Client return key encrypted into the exit layer.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionClientReturn {
    /// Client session public key used for backward AEAD payloads.
    pub session_public_key: PublicKey<33>,
    /// Client/exit-only id used to authenticate backward payloads.
    pub return_id: OnionReturnId,
}

impl OnionClientReturn {
    /// Build a client return descriptor with a fresh return id.
    pub fn new(session_public_key: PublicKey<33>) -> Self {
        Self {
            session_public_key,
            return_id: OnionReturnId::random(),
        }
    }

    /// Build a client return descriptor with an explicit return id.
    pub const fn with_return_id(
        session_public_key: PublicKey<33>,
        return_id: OnionReturnId,
    ) -> Self {
        Self {
            session_public_key,
            return_id,
        }
    }
}

/// Edge-local circuit id.
///
/// Invariant: an [`OnionCircuitId`] identifies exactly one directed edge of one route. Relay layers
/// carry the next edge id under AEAD; backward forwarding rewrites the header back to the previous
/// edge id.
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
    /// Edge-local circuit id for the receiving hop.
    pub circuit_id: OnionCircuitId,
    /// AEAD-encrypted layer for the receiving hop.
    pub layer: AeadCiphertext,
}

/// Backward direction: exit -> relays -> client.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionBackwardFrame {
    /// Edge-local circuit id for the receiving relay or client.
    pub circuit_id: OnionCircuitId,
    /// AEAD payload encrypted to the client session public key.
    pub payload: AeadCiphertext,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(super) enum OnionForwardLayer {
    Relay {
        next_hop: Did,
        next_circuit_id: OnionCircuitId,
        remaining_hops: u8,
        inner: AeadCiphertext,
    },
    Exit {
        client: OnionClientReturn,
        expires_at_ms: u128,
        forward_nonce: OnionForwardNonce,
        payload: OnionCircuitPayload,
    },
}
