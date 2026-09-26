//! Sessions of the world-facing symbols `tcp` and `https` over onion loops (#834 D2′, D8).
//!
//! A session `ς` is a duplex byte stream between the client and its world-facing hop `h`, carried
//! by loops: every forward loop brings one frame to `h` and leaves `h` one reply block, and every
//! reply spends one of them. The pieces, each pure:
//!
//! ```text
//! frame    OnionFrame          data(n, φ, w) | fin(n) | credit(υ…)          the carried value
//! ā        OnionSessionArguments  ς ‖ SHA-256(t) ‖ 0^16                      the layer arguments
//! Q_{h,ς}  OnionSurbPool       ≤ Q_max reply blocks, least x first          at h
//! order    OnionReorder        per-direction sequence, window Q_max        at both ends
//! h        OnionExitSession    open at the first matching T, reply per υ   the exit's machine
//! cl       OnionClientSession  T until the first reply, credit window      the client's machine
//! ```
//!
//! Laws (Invariant Credit of the paper, tested in `session::tests`):
//!
//! - **Credit.** Per session, `replies ≤ reply blocks received` and `|Q_{h,ς}| ≤ Q_max`; a
//!   session with `Q_{h,ς} = ∅` reads nothing from the world and resumes on new credit.
//! - **Binding.** `h` opens `ς` only for a target `t` with `SHA-256(t) = d`, the digest its layer
//!   carries, and never rebinds it.
//! - **Order.** Each direction releases `n = 0, 1, …` in order; a persisting gap fails the
//!   session closed, never skipping bytes.
//! - **Close.** `fin` in both directions, a gap, or `V` without a forward loop drops `ς` at `h`.

pub(crate) mod client;
pub(crate) mod dial;
pub(crate) mod exit;
pub(crate) mod frame;
pub(crate) mod order;
pub(crate) mod pool;
pub(crate) mod serve;
#[cfg(test)]
mod tests;

use sha2::Digest;
use sha2::Sha256;

use crate::onion::sphinx::layer::OnionArguments;
use crate::onion::sphinx::layer::ONION_ARGUMENT_BYTES;

/// Bytes of a session id `ς`.
const ONION_SESSION_ID_BYTES: usize = 16;

/// Bytes of a target digest `d = SHA-256(t)`.
const ONION_TARGET_DIGEST_BYTES: usize = 32;

// `ā = ς ‖ d` fills 48 of the `A = 64` argument bytes (D3).
const _: () = assert!(ONION_SESSION_ID_BYTES + ONION_TARGET_DIGEST_BYTES <= ONION_ARGUMENT_BYTES);

/// A session id `ς ∈ {0,1}^128`, drawn by the client per session (D2′).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OnionSessionId([u8; ONION_SESSION_ID_BYTES]);

impl OnionSessionId {
    /// A uniform session id.
    pub(crate) fn random() -> Self {
        Self(rand::random())
    }

    /// The session id with `bytes`.
    #[cfg(test)]
    pub(crate) const fn new(bytes: [u8; ONION_SESSION_ID_BYTES]) -> Self {
        Self(bytes)
    }
}

/// The digest `d = SHA-256(t)` of a session target, by which `ā` names the target whatever its
/// length; the target itself travels in `data` frames under `T`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OnionTargetDigest([u8; ONION_TARGET_DIGEST_BYTES]);

impl OnionTargetDigest {
    /// `SHA-256(t)` of the target's canonical authority bytes.
    pub(crate) fn of(target: &[u8]) -> Self {
        Self(Sha256::digest(target).into())
    }
}

/// The arguments `ā = (ς, d)` of a session symbol, encoded `ς ‖ d ‖ 0^16` (D2′, D3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionSessionArguments {
    /// `ς`.
    pub(crate) session: OnionSessionId,
    /// `d`.
    pub(crate) digest: OnionTargetDigest,
}

impl OnionSessionArguments {
    /// `ς ‖ d ‖ 0^16`.
    pub(crate) fn encode(self) -> OnionArguments {
        let mut bytes = [0; ONION_ARGUMENT_BYTES];
        bytes
            .iter_mut()
            .zip(self.session.0.iter().chain(self.digest.0.iter()))
            .for_each(|(slot, byte)| *slot = *byte);
        OnionArguments::new(bytes)
    }

    /// The left inverse of [`Self::encode`]: `None` unless the padding is `0^16`, so every
    /// session application has exactly one encoding.
    pub(crate) fn decode(arguments: &OnionArguments) -> Option<Self> {
        let bytes = arguments.as_bytes();
        let (session, rest) = bytes.split_first_chunk::<ONION_SESSION_ID_BYTES>()?;
        let (digest, padding) = rest.split_first_chunk::<ONION_TARGET_DIGEST_BYTES>()?;
        padding.iter().all(|byte| *byte == 0).then_some(Self {
            session: OnionSessionId(*session),
            digest: OnionTargetDigest(*digest),
        })
    }
}
