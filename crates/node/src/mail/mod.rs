//! Mail to `did@domain` through a DHT data topic (#1117): what a mail gateway writes and what a
//! recipient, native or in a browser, reads.
//!
//! A gateway is a stateless SMTP receiver for every domain whose MX names it; its SMTP front is
//! an application (`examples/mailbox`). This module is what both ends share over the DHT:
//!
//! ```text
//!   recipient d:  μ(d) on descriptor_topic(d), every heartbeat          (registration)
//!   gateway:      RCPT l@any ─▶ d = addressed_did(l) ─▶ μ(d) live?       (gateway::recipient)
//!                 DATA m ─▶ seal(m, ek(μ)) ─▶ Extend(mail_topic(d), ttl = MAX_TTL_MS)
//!                        ─▶ 250 at the first owner's Stored report         (gateway::deliver)
//!   recipient d:  fetch mail_topic(d) at start and every I ≤ H / 2 ─▶ open ─▶ file, dedup
//! ```
//!
//! - [`address`]: the DID a mail address names.
//! - [`descriptor`]: the signed mail descriptor `μ(d)`, whose signature gives the key mail is
//!   sealed to.
//! - [`registration`]: the heartbeat that keeps `μ(d)` in the DHT under the registry lease.
//! - [`envelope`]: a mail as DHT elements: a manifest and its chunks (pure).
//! - [`open`]: the recipient's opening of those elements (pure).
//! - [`gateway`]: the gateway's DHT side: the RCPT lookup and the write.
//! - [`recipient`]: the recipient's poll and the records it keeps.
//!
//! Storage semantics: a mail is one `Extend` of every element with lifetime
//! [`MAX_TTL_MS`](rings_core::consts::MAX_TTL_MS), so it lives `H` (100 minutes) after the
//! write and is then lost if its recipient has not fetched it. A mail is lost too when it is
//! sealed to a delegatee key its recipient no longer holds: a browser node takes a new key at
//! every load (`recipient`'s one-key law). The gateway's `250` means that
//! an owner reported holding the mail (#866); a read-back would be the writer's own cache
//! (#948, #982), not an owner's statement. The recipient never removes: each element retires at
//! its horizon.
//!
//! Layering: application code over core's public storage API; core knows nothing of mail.

pub mod address;
pub mod descriptor;
pub mod envelope;
pub mod gateway;
pub mod open;
pub mod recipient;
pub mod registration;

use crate::prelude::rings_core::dht::Did;

/// Write bytes in lowercase hexadecimal. Law: each byte contributes exactly two digits in
/// input order, preserving leading zeroes in every mail digest and record-name key.
fn write_lowercase_hex(formatter: &mut std::fmt::Formatter<'_>, bytes: &[u8]) -> std::fmt::Result {
    bytes
        .iter()
        .try_for_each(|byte| write!(formatter, "{byte:02x}"))
}

/// The data topic a recipient's mail is written to: `"mail:" ‖ d`.
///
/// Law: injective in `did`, and disjoint from [`descriptor::descriptor_topic`] and from every
/// registry and DNS relay topic, none of which starts with `mail:`.
pub fn mail_topic(did: Did) -> String {
    format!("mail:{did}")
}

#[cfg(test)]
mod tests;
