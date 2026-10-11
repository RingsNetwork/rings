//! Namespaces whose admission the application delegated, as data core reads.
//!
//! ```text
//!   app:  Swarm::set_admission_delegation(prefix, 𝔹)  -- at protocol install / replace
//!   core: delegates(payload) = ∃ p ∈ prefixes. payload starts with p   -- on admission
//! ```
//!
//! Core consults no application code on its admission path: the application registers in
//! advance the encoded prefix that names a namespace, and core matches it. The match is exact
//! when the application's namespace encoding is prefix-free (a length-prefixed name is), which
//! is the application's to guarantee, as delegation is trusted local configuration.

use std::collections::BTreeSet;
use std::sync::PoisonError;
use std::sync::RwLock;

/// The encoded namespace prefixes whose application payloads delegate their admission.
#[derive(Default)]
pub(crate) struct DelegatedNamespaces {
    prefixes: RwLock<BTreeSet<Vec<u8>>>,
}

impl DelegatedNamespaces {
    /// Record whether payloads starting with `prefix` delegate their admission.
    pub(crate) fn set(&self, prefix: Vec<u8>, delegates: bool) {
        let mut prefixes = self
            .prefixes
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if delegates {
            prefixes.insert(prefix);
        } else {
            prefixes.remove(&prefix);
        }
    }

    /// Whether `application_payload` belongs to a delegating namespace.
    pub(crate) fn delegates(&self, application_payload: &[u8]) -> bool {
        self.prefixes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|prefix| application_payload.starts_with(prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::DelegatedNamespaces;

    #[test]
    fn test_a_payload_delegates_exactly_while_its_prefix_is_registered() {
        let namespaces = DelegatedNamespaces::default();
        let prefix = b"\x03abc".to_vec();
        assert!(!namespaces.delegates(b"\x03abcpayload"));
        namespaces.set(prefix.clone(), true);
        assert!(namespaces.delegates(b"\x03abcpayload"));
        assert!(!namespaces.delegates(b"\x03abd"));
        namespaces.set(prefix, false);
        assert!(!namespaces.delegates(b"\x03abcpayload"));
    }
}
