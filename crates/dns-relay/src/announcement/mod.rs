//! What an owner announces: the ports it serves and the TXT records it publishes.
//!
//! An [`Announcement`] is the relay's validated view of an owner's signed descriptor. Its
//! constructor is the only way to make one, and it enforces every bound below, so the DNS answer
//! and the ingress never meet an oversized or malformed announcement.
//!
//! ```text
//!   Announcement = (ports : Port ⇀ ProxyProtocol,  txt : SubName ⇀ [TxtValue]⁺)
//!   |ports| ≤ MAX_SERVED_PORTS,  |txt| ≤ MAX_TXT_NAMES,  ∀s. 1 ≤ |txt(s)| ≤ MAX_TXT_VALUES
//!   receives_mail(a)  ⇔  SMTP_PORT ∈ dom(ports)
//!   exists(a, s)      ⇔  s = ε  ∨  ∃t ∈ dom(txt). t = s ∨ t ends with "." ‖ s
//! ```
//!
//! A port names the service it reaches: which local service a port maps to is the owner's own
//! business, so the relay keeps nothing but the port and how to tell the service a connection's
//! origin.

use std::collections::BTreeMap;
use std::num::NonZeroU16;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

/// The port of SMTP: an owner that serves it receives mail, and its name gets an MX record.
pub const SMTP_PORT: u16 = 25;
/// Most ports one owner may serve through the relay.
pub const MAX_SERVED_PORTS: usize = 32;
/// Most names under its own one owner may publish TXT records at.
pub const MAX_TXT_NAMES: usize = 8;
/// Most TXT records one owner may publish at one name.
pub const MAX_TXT_VALUES: usize = 4;
/// Longest TXT value: one DNS character-string.
pub const MAX_TXT_VALUE_LEN: usize = 255;
/// Most labels a name under an owner may have.
pub const MAX_SUB_NAME_LABELS: usize = 4;
/// Longest DNS label.
pub const MAX_LABEL_LEN: usize = 63;

/// Why an announcement, or one of its parts, is malformed.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AnnouncementError {
    /// A TXT value is longer than one character-string.
    #[error("TXT value of {0} bytes exceeds {MAX_TXT_VALUE_LEN}")]
    TxtValue(usize),
    /// A name under the owner is not a sequence of lowercase labels.
    #[error("invalid name under the owner {0:?}")]
    SubName(String),
    /// Port 0 cannot be served.
    #[error("port 0 cannot be served")]
    PortZero,
    /// One port is mapped twice.
    #[error("port {0} is served twice")]
    DuplicatePort(u16),
    /// More ports than [`MAX_SERVED_PORTS`].
    #[error("{0} served ports exceed {MAX_SERVED_PORTS}")]
    TooManyPorts(usize),
    /// TXT records at more names than [`MAX_TXT_NAMES`].
    #[error("TXT records at {0} names exceed {MAX_TXT_NAMES}")]
    TooManyTxtNames(usize),
    /// A name has no TXT value, or more than [`MAX_TXT_VALUES`].
    #[error("{count} TXT values at {name:?}: between 1 and {MAX_TXT_VALUES} are allowed")]
    TxtValueCount {
        /// The name under the owner.
        name: String,
        /// How many values it was given.
        count: usize,
    },
    /// One name is given TXT records twice.
    #[error("TXT records at {0:?} are given twice")]
    DuplicateTxtName(String),
}

/// How the ingress tells a served port's service where a connection came from.
///
/// The relay carries a connection's bytes and nothing else, so without a header the service
/// sees every connection come from the owner's node itself. Written `off` or `v2`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyProtocol {
    /// Nothing precedes the client's bytes.
    #[default]
    Off,
    /// A PROXY protocol version 2 header naming the client's and the relayed address precedes
    /// the client's bytes ([`crate::proxy_v2_header`]).
    V2,
}

/// One TXT record's text: a single DNS character-string, at most [`MAX_TXT_VALUE_LEN`] bytes.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TxtValue(String);

impl TxtValue {
    /// Validate `value` as one TXT character-string.
    pub fn new(value: impl Into<String>) -> Result<Self, AnnouncementError> {
        let value = value.into();
        if value.len() > MAX_TXT_VALUE_LEN {
            return Err(AnnouncementError::TxtValue(value.len()));
        }
        Ok(Self(value))
    }

    /// The value as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A name under an owner's name, relative to it: `_acme-challenge` names
/// `_acme-challenge.<owner>.<zone>`, and the empty name names the owner itself.
///
/// Its labels are lowercase letters, digits, `-` and `_`, from 1 to [`MAX_LABEL_LEN`] bytes, at
/// most [`MAX_SUB_NAME_LABELS`] of them, joined by `.`.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct SubName(String);

impl SubName {
    /// The owner's own name.
    pub const fn owner() -> Self {
        Self(String::new())
    }

    /// Validate `name` as a name under an owner.
    pub fn new(name: impl Into<String>) -> Result<Self, AnnouncementError> {
        let name = name.into();
        if name.is_empty() {
            return Ok(Self(name));
        }
        let labels = name.split('.').collect::<Vec<_>>();
        if labels.len() > MAX_SUB_NAME_LABELS || !labels.iter().all(|label| is_sub_label(label)) {
            return Err(AnnouncementError::SubName(name));
        }
        Ok(Self(name))
    }

    /// The name from its labels, leftmost first, each validated.
    pub fn from_labels<'a>(
        labels: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, AnnouncementError> {
        Self::new(labels.into_iter().collect::<Vec<_>>().join("."))
    }

    /// Whether this is the owner's own name.
    pub fn is_owner(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `self` is `other` or lies below it: `a.b` lies below `b`, and every name lies
    /// below the owner's own.
    pub fn is_at_or_below(&self, other: &Self) -> bool {
        other.is_owner()
            || self.0 == other.0
            || self
                .0
                .strip_suffix(other.0.as_str())
                .is_some_and(|head| head.ends_with('.'))
    }

    /// The name as text: labels joined by `.`, empty for the owner's own name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether `label` is a label of a [`SubName`].
fn is_sub_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && label.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

/// What an owner announces through the relay: the ports it serves, each with how its service
/// learns a connection's origin, and the TXT records it publishes under its name.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Announcement {
    ports: BTreeMap<NonZeroU16, ProxyProtocol>,
    txt: BTreeMap<SubName, Vec<TxtValue>>,
}

impl Announcement {
    /// Validate an announcement from its served ports and its TXT records.
    ///
    /// Post: every bound in the module doc holds, no port is 0 or repeated, and no TXT name is
    /// repeated. A bound is checked as each entry arrives, so an oversized input is refused
    /// without being collected first.
    pub fn new(
        ports: impl IntoIterator<Item = (u16, ProxyProtocol)>,
        txt: impl IntoIterator<Item = (SubName, Vec<TxtValue>)>,
    ) -> Result<Self, AnnouncementError> {
        let mut served = BTreeMap::new();
        for (count, (port, proxy)) in ports.into_iter().enumerate() {
            if count >= MAX_SERVED_PORTS {
                return Err(AnnouncementError::TooManyPorts(count.saturating_add(1)));
            }
            let port = NonZeroU16::new(port).ok_or(AnnouncementError::PortZero)?;
            if served.insert(port, proxy).is_some() {
                return Err(AnnouncementError::DuplicatePort(port.get()));
            }
        }
        let mut published = BTreeMap::new();
        for (count, (name, values)) in txt.into_iter().enumerate() {
            if count >= MAX_TXT_NAMES {
                return Err(AnnouncementError::TooManyTxtNames(count.saturating_add(1)));
            }
            if values.is_empty() || values.len() > MAX_TXT_VALUES {
                return Err(AnnouncementError::TxtValueCount {
                    name: name.0,
                    count: values.len(),
                });
            }
            if published.contains_key(&name) {
                return Err(AnnouncementError::DuplicateTxtName(name.0));
            }
            published.insert(name, values);
        }
        Ok(Self {
            ports: served,
            txt: published,
        })
    }

    /// Whether this announcement announces nothing: no port and no record. An owner announcing
    /// nothing withdraws its name.
    pub fn is_empty(&self) -> bool {
        self.ports.is_empty() && self.txt.is_empty()
    }

    /// How this owner serves `port`, if it serves it.
    pub fn serves(&self, port: u16) -> Option<ProxyProtocol> {
        NonZeroU16::new(port).and_then(|port| self.ports.get(&port).copied())
    }

    /// The ports this owner serves, ascending, each with how its service learns origins.
    pub fn served_ports(&self) -> impl Iterator<Item = (u16, ProxyProtocol)> + '_ {
        self.ports.iter().map(|(port, proxy)| (port.get(), *proxy))
    }

    /// Whether this owner receives mail: it serves [`SMTP_PORT`].
    pub fn receives_mail(&self) -> bool {
        self.serves(SMTP_PORT).is_some()
    }

    /// The TXT records this owner publishes at `name`; empty when it publishes none there.
    pub fn txt_at(&self, name: &SubName) -> &[TxtValue] {
        self.txt.get(name).map_or(&[], Vec::as_slice)
    }

    /// The names this owner publishes TXT records at, with their values.
    pub fn txt_records(&self) -> impl Iterator<Item = (&SubName, &[TxtValue])> {
        self.txt
            .iter()
            .map(|(name, values)| (name, values.as_slice()))
    }

    /// Whether `name` exists in DNS under this owner: it is the owner's own name, or a name this
    /// owner publishes a record at or below (an empty non-terminal exists too).
    pub fn names(&self, name: &SubName) -> bool {
        name.is_owner()
            || self
                .txt
                .keys()
                .any(|published| published.is_at_or_below(name))
    }
}

#[cfg(test)]
mod test_announcement;
