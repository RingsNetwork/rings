//! Namespaces that delegate their admission (#888).
//!
//! A [`Protocol`](super::Protocol) that admits its own direct-edge traffic, per sending
//! neighbour, before its own processing declares it through
//! [`Protocol::delegates_admission`](super::Protocol::delegates_admission). The registry
//! publishes the declaration into core when the protocol is installed, under the same write lock
//! that installs its handler, as the encoded prefix that names the namespace in every
//! [`Envelope`]. Core matches the prefix on its own admission path; no node code runs there
//! (#935):
//!
//! ```text
//!   publish   : (namespace, 𝔹) → Swarm::set_admission_delegation(prefix(namespace), 𝔹)
//!   prefix    : namespace → encoded namespace   -- a prefix of every envelope of it
//! ```
//!
//! The encoding is length-prefixed, so it is prefix-free: an envelope starts with the prefix of
//! exactly its own namespace, and core's match is exact.
//!
//! Core decides whether delegation applies at all (Application traffic from the authenticated
//! neighbour that originated it); the published set only names the namespaces, never the
//! eligibility.
//!
//! The verdict is taken at admission and not carried with the frame. A frame admitted as
//! delegated that is still queued when its namespace's protocol is replaced by one that does not
//! delegate reaches the replacement having skipped the message limit, and the reverse holds too.
//! The exposure is bounded by the inbound mailbox depth, and every delegated frame was charged
//! at least `DELEGATED_MIN_CHARGE` bytes.

use rings_core::swarm::Swarm;

use super::Envelope;
use crate::error::Result;

/// Publish a batch of declarations into `swarm`.
///
/// Pre: the caller holds the handler table's write lock and inserts the batch's handlers right
/// after, so at admission time a namespace delegates exactly while a delegating protocol owns
/// it.
pub(crate) fn publish<'a>(
    swarm: &Swarm,
    declarations: impl IntoIterator<Item = (&'a str, bool)>,
) -> Result<()> {
    for (namespace, delegates) in declarations {
        swarm.set_admission_delegation(Envelope::namespace_prefix(namespace)?, delegates);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::Envelope;
    use crate::error::Result;

    /// Encode an envelope of `namespace`.
    fn envelope(namespace: &str) -> Vec<u8> {
        Envelope::new(namespace, Bytes::from_static(b"payload"))
            .encode()
            .expect("envelope encodes")
    }

    #[test]
    fn test_an_envelope_starts_with_exactly_its_own_namespace_prefix() -> Result<()> {
        let prefix = Envelope::namespace_prefix("delegating")?;
        assert!(envelope("delegating").starts_with(&prefix));
        assert!(!envelope("delegating-not").starts_with(&prefix));
        assert!(!envelope("delegate").starts_with(&prefix));
        Ok(())
    }

    /// Only the namespace prefix is read: a delegating namespace followed by an undecodable
    /// tail still carries the prefix. Core then charges it at least `DELEGATED_MIN_CHARGE`
    /// bytes, which is what bounds such frames.
    #[test]
    fn test_a_malformed_tail_after_a_namespace_keeps_its_prefix() -> Result<()> {
        let mut malformed = Envelope::namespace_prefix("delegating")?;
        malformed.extend_from_slice(&[0xff; 9]);
        assert!(Envelope::decode(&malformed).is_err());
        assert!(malformed.starts_with(&Envelope::namespace_prefix("delegating")?));
        Ok(())
    }
}
