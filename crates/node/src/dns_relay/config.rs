//! The node's `services` section: what it serves at `<did>.<zone>` through DNS relays.
//!
//! ```yaml
//! services:
//!   ports:
//!     - port: 25
//!       target: 127.0.0.1:2525
//!     - port: 22
//!       target: 127.0.0.1:22
//!   txt:
//!     - name: _acme-challenge
//!       values: ["<token>"]
//! ```
//!
//! Each port maps to the local address the node dials for a connection to that port of its
//! relayed address. An empty section (no port, no record) publishes a withdrawal.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

use super::descriptor::ServedTxt;
use super::registration::ServiceOffer;
use crate::error::Error;
use crate::error::Result;
use crate::registration::validate_registration_timing;

/// Default heartbeat interval of the service descriptor, in seconds.
const DEFAULT_SERVICES_HEARTBEAT_INTERVAL_SECS: u64 = 30;
/// Default time-to-live of each service descriptor, in seconds.
const DEFAULT_SERVICES_TTL_SECS: u64 = 90;

/// One served port and the local address behind it.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServedPortConfig {
    /// The port at the node's relayed address.
    pub port: u16,
    /// The local address the node dials for a connection to it.
    pub target: SocketAddr,
}

/// TXT records published at one name under the node's.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServedTxtConfig {
    /// The name under the node's name; empty for the node's own.
    #[serde(default)]
    pub name: String,
    /// The records' texts.
    pub values: Vec<String>,
}

/// The node's `services` section.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServicesConfig {
    /// The served ports.
    #[serde(default)]
    pub ports: Vec<ServedPortConfig>,
    /// The published TXT records.
    #[serde(default)]
    pub txt: Vec<ServedTxtConfig>,
    /// Service descriptor heartbeat interval, in seconds.
    #[serde(default = "default_services_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
    /// Service descriptor time-to-live, in seconds.
    #[serde(default = "default_services_ttl_secs")]
    pub ttl_secs: u64,
}

impl ServicesConfig {
    /// The validated offer this section publishes.
    ///
    /// Post: no port repeats, every bound of an announcement holds, and the heartbeat refreshes
    /// the descriptor before it expires.
    pub fn offer(&self) -> Result<ServiceOffer> {
        let heartbeat_interval = Duration::from_secs(self.heartbeat_interval_secs);
        let ttl = Duration::from_secs(self.ttl_secs);
        validate_registration_timing("services", "services", true, heartbeat_interval, ttl)?;
        let mut ports = BTreeSet::new();
        for served in &self.ports {
            if !ports.insert(served.port) {
                return Err(Error::InvalidConfig(format!(
                    "services: port {} is served twice",
                    served.port
                )));
            }
        }
        let txt = self
            .txt
            .iter()
            .map(|served| ServedTxt {
                name: served.name.clone(),
                values: served.values.clone(),
            })
            .collect();
        ServiceOffer::new(ports, txt, heartbeat_interval, ttl)
            .map_err(|error| Error::InvalidConfig(format!("services: {error}")))
    }

    /// The local address behind each served port.
    pub fn targets(&self) -> impl Iterator<Item = (u16, SocketAddr)> + '_ {
        self.ports.iter().map(|served| (served.port, served.target))
    }
}

/// [`DEFAULT_SERVICES_HEARTBEAT_INTERVAL_SECS`].
const fn default_services_heartbeat_interval_secs() -> u64 {
    DEFAULT_SERVICES_HEARTBEAT_INTERVAL_SECS
}

/// [`DEFAULT_SERVICES_TTL_SECS`].
const fn default_services_ttl_secs() -> u64 {
    DEFAULT_SERVICES_TTL_SECS
}
