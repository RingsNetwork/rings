//! Namespaces that delegate their admission (#888).
//!
//! A [`Protocol`](super::Protocol) that admits its own direct-edge traffic, per sending
//! neighbour, before any further processing declares it through
//! [`Protocol::delegates_admission`](super::Protocol::delegates_admission). The registry records
//! the declaration here when the protocol is installed, under the same write lock that installs
//! its handler, and core asks it about an inbound application payload through
//! [`Backend`](crate::extension::Backend):
//!
//! ```text
//!   install    : (namespace, 𝔹) → namespaces'   -- at register / replace
//!   delegates  : encoded Envelope → 𝔹           -- namespace prefix only
//! ```
//!
//! Core decides whether delegation applies at all (Application traffic from the authenticated
//! neighbour that originated it); this set only names the namespaces, never the eligibility.

use std::collections::HashSet;
use std::sync::RwLock;

use super::Envelope;
use crate::error::Error;
use crate::error::Result;

/// The set of namespaces whose registered protocol delegates its admission.
#[derive(Debug, Default)]
pub(crate) struct DelegatedNamespaces {
    /// The delegating namespaces.
    namespaces: RwLock<HashSet<String>>,
}

impl DelegatedNamespaces {
    /// Record the declarations of a batch of namespaces at once.
    ///
    /// Pre: the caller holds the handler table's write lock and inserts the batch's handlers
    /// right after, so a namespace delegates exactly while a delegating protocol owns it.
    pub(crate) fn install<'a>(
        &self,
        declarations: impl IntoIterator<Item = (&'a str, bool)>,
    ) -> Result<()> {
        let mut namespaces = self.namespaces.write().map_err(|_| Error::Lock)?;
        for (namespace, delegates) in declarations {
            if delegates {
                namespaces.insert(namespace.to_string());
            } else {
                namespaces.remove(namespace);
            }
        }
        Ok(())
    }

    /// Whether the namespace of an encoded [`Envelope`] delegates its admission, read from the
    /// namespace prefix alone. Undecodable bytes delegate nothing.
    pub(crate) fn delegates(&self, envelope: &[u8]) -> bool {
        let Ok(namespace) = Envelope::namespace_of(envelope) else {
            return false;
        };
        self.namespaces
            .read()
            .is_ok_and(|namespaces| namespaces.contains(namespace))
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::DelegatedNamespaces;
    use super::Envelope;
    use crate::error::Result;

    /// Encode an envelope of `namespace`.
    fn envelope(namespace: &str) -> Vec<u8> {
        Envelope::new(namespace, Bytes::from_static(b"payload"))
            .encode()
            .expect("envelope encodes")
    }

    #[test]
    fn test_declarations_resolve_by_namespace_and_follow_replacement() -> Result<()> {
        let namespaces = DelegatedNamespaces::default();
        namespaces.install([("delegating", true), ("plain", false)])?;
        assert!(namespaces.delegates(&envelope("delegating")));
        assert!(!namespaces.delegates(&envelope("plain")));
        assert!(!namespaces.delegates(b"not an envelope"));

        namespaces.install([("delegating", false)])?;
        assert!(!namespaces.delegates(&envelope("delegating")));
        namespaces.install([("delegating", true)])?;
        assert!(namespaces.delegates(&envelope("delegating")));
        Ok(())
    }
}
