//! The DNS relay on the node (#1049): the services a node announces at `<did>.<zone>`, and the
//! relay a node may run for its zones.
//!
//! The relay itself, its DNS authority, address map and ingress, is the overlay-free crate
//! `rings_dns_relay`. This module is what ties it to Rings:
//!
//! - `descriptor`: the signed service descriptor a node publishes in the DHT, on the topic of its
//!   own DID, built with the node's signed-descriptor machinery.
//! - `registration`: the heartbeat that publishes it, one of the node's registration tasks.
//! - `directory`: the relay's [`Directory`](rings_dns_relay::Directory): an owner's latest valid
//!   descriptor, read from the DHT.
//! - `forward`: the relay's [`Forward`](rings_dns_relay::Forward): the TCP relay to the owner's
//!   node, which dials only the services it registered, a PROXY protocol header first where the
//!   owner asks for one.
//! - `config`: the node's `services` section.
//! - `open`: what `rings run` starts: the served ports, and the relay the node runs.
//!
//! ```text
//!   node d:   services ─▶ register tcp relay services "dns-relay/<p>" ─▶ publish σ(d) every I
//!   relay:    DNS  label(d).z ─▶ σ(d) from the DHT ─▶ AAAA addr(d), MX, TXT
//!             TCP  (addr(d), p) ─▶ tcp relay ─▶ d's "dns-relay/<p>" ─▶ d's local target
//! ```

mod config;
mod descriptor;
mod directory;
mod forward;
mod open;
mod registration;

use std::fmt;

pub use config::ServedPortConfig;
pub use config::ServedTxtConfig;
pub use config::ServicesConfig;
pub use descriptor::service_topic;
pub use descriptor::ServedPort;
pub use descriptor::ServedTxt;
pub use descriptor::ServiceDescriptor;
pub use descriptor::ServiceDescriptorBody;
pub use directory::DhtDirectory;
pub use directory::DirectoryError;
pub use forward::ForwardError;
pub use forward::RelayForward;
pub use open::open;
pub use open::NodeDnsRelay;
pub use open::OpenError;
pub use registration::ServiceOffer;
pub(crate) use registration::ServiceRegistration;
pub use rings_dns_relay::DnsRelay;
pub use rings_dns_relay::DnsRelayConfig;
use rings_dns_relay::Owner;

use crate::prelude::rings_core::dht::Did;

/// A DID as the owner of a relayed name: its label is the DID's canonical text, `0x` and 40
/// lowercase hex digits.
///
/// ```text
///   label(DidOwner(d)) = d.to_string()
///   from_label(l) = Some(DidOwner(d))  ⇔  l parses as d ∧ d.to_string() = l
/// ```
///
/// The second law makes the parse canonical: a label without `0x`, or with fewer digits, names
/// no owner, so a DID has one name per zone.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DidOwner(pub Did);

impl Owner for DidOwner {
    fn from_label(label: &str) -> Option<Self> {
        let did = label.parse::<Did>().ok()?;
        (did.to_string() == label).then_some(Self(did))
    }

    fn label(&self) -> String {
        self.0.to_string()
    }
}

impl fmt::Display for DidOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The TCP relay service a node serves `port` under: what the relay's ingress asks the node to
/// dial for a connection to that port of its address.
///
/// Law: injective in `port`, and disjoint from every service name an operator registers that
/// does not start with `dns-relay/`.
pub fn relay_service(port: u16) -> String {
    format!("dns-relay/{port}")
}

#[cfg(test)]
mod test_dns_relay;
