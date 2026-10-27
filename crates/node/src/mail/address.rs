//! The DID a mail address names.
//!
//! ```text
//!   addressed_did(l@domain) = Some(d)   ⇔  lower(l) parses as d ∧ d.to_string() = lower(l)
//!                                          ∧ domain ≠ ε
//! ```
//!
//! The local part is the DID's canonical text, `0x` and 40 hex digits (42 octets, within the 64
//! of RFC 5321 §4.5.3.1.1), compared in lower case; the domain is any: every domain whose MX
//! names a gateway reaches the same mailbox, so `d@a` and `d@b` are one address.

use crate::prelude::rings_core::dht::Did;

/// The DID the mail address `address` names, or `None` when its local part is not a DID's
/// canonical text in some case, or it has no domain.
pub fn addressed_did(address: &str) -> Option<Did> {
    let (local, domain) = address.rsplit_once('@')?;
    if domain.is_empty() {
        return None;
    }
    let local = local.to_ascii_lowercase();
    let did = local.parse::<Did>().ok()?;
    (did.to_string() == local).then_some(did)
}
