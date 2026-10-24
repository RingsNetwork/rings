//! The zones a relay serves, and where a queried name lies among them.
//!
//! A zone is delegated to the relay by NS records at its parent. Below its apex, the label next
//! to the apex names an owner, and any labels further left name a [`SubName`] under that owner:
//!
//! ```text
//!   place : Name → Placement
//!   place(n) = Foreign                       if no zone z has n ∈ z
//!            = Apex(z)                       if n = z
//!            = Owned(z, o, s)                if n = s.l.z  ∧  from_label(l) = Some(o)  ∧  s valid
//!            = Absent(z)                     otherwise
//! ```
//!
//! Zones are pairwise disjoint (no origin lies in another zone), so at most one zone contains a
//! name and `place` is a function. Nameservers lie outside every zone, so the relay never needs
//! glue for them: their addresses come from their own zones.

use hickory_proto::rr::LowerName;
use hickory_proto::rr::Name;
use thiserror::Error;

use crate::announcement::SubName;
use crate::owner::Owner;

/// Why a set of zones cannot be served.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ZoneError {
    /// No zone is configured.
    #[error("no zone is configured")]
    Empty,
    /// One zone's origin lies in another zone.
    #[error("zone {inner} lies in zone {outer}")]
    Overlap {
        /// The zone lying inside.
        inner: String,
        /// The zone containing it.
        outer: String,
    },
    /// A zone has no nameserver.
    #[error("zone {0} has no nameserver")]
    NoNameserver(String),
    /// A nameserver lies in a relayed zone, where the relay would have to answer for it.
    #[error("nameserver {nameserver} lies in relayed zone {zone}")]
    NameserverInZone {
        /// The nameserver's name.
        nameserver: String,
        /// The zone it lies in.
        zone: String,
    },
}

/// One zone delegated to the relay: its origin, its nameservers and the contact its SOA names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Zone {
    origin: LowerName,
    nameservers: Vec<Name>,
    contact: Name,
}

impl Zone {
    /// A zone with origin `origin`, served by `nameservers`, whose SOA names `contact` (the
    /// mailbox `hostmaster@example.com` is written `hostmaster.example.com.`).
    pub fn new(origin: Name, nameservers: Vec<Name>, contact: Name) -> Self {
        Self {
            origin: LowerName::new(&origin.to_lowercase()),
            nameservers,
            contact,
        }
    }

    /// The zone's origin.
    pub fn origin(&self) -> &LowerName {
        &self.origin
    }

    /// The zone's nameservers.
    pub fn nameservers(&self) -> &[Name] {
        &self.nameservers
    }

    /// The mailbox the zone's SOA names.
    pub fn contact(&self) -> &Name {
        &self.contact
    }

    /// Whether `name` lies in this zone: it is the origin or below it.
    fn contains(&self, name: &LowerName) -> bool {
        self.origin.zone_of(name)
    }
}

/// Where a queried name lies among the relayed zones.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Placement<'z, O> {
    /// The name lies in no relayed zone.
    Foreign,
    /// The name is the apex of `zone`.
    Apex(&'z Zone),
    /// The name is `sub` under `owner`'s name in `zone`.
    Owned {
        /// The zone the name lies in.
        zone: &'z Zone,
        /// The owner the label next to the apex names.
        owner: O,
        /// The name under the owner's name; empty for the owner's own name.
        sub: SubName,
    },
    /// The name lies in `zone` but names no owner.
    Absent(&'z Zone),
}

/// The zones a relay serves: non-empty and pairwise disjoint, each with a nameserver outside
/// every zone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Zones(Vec<Zone>);

impl Zones {
    /// Validate `zones` as a set the relay can serve.
    pub fn new(zones: Vec<Zone>) -> Result<Self, ZoneError> {
        if zones.is_empty() {
            return Err(ZoneError::Empty);
        }
        for (index, outer) in zones.iter().enumerate() {
            if outer.nameservers.is_empty() {
                return Err(ZoneError::NoNameserver(outer.origin.to_string()));
            }
            for inner in zones.iter().skip(index.saturating_add(1)) {
                let (inside, around) = if outer.contains(&inner.origin) {
                    (inner, outer)
                } else if inner.contains(&outer.origin) {
                    (outer, inner)
                } else {
                    continue;
                };
                return Err(ZoneError::Overlap {
                    inner: inside.origin.to_string(),
                    outer: around.origin.to_string(),
                });
            }
        }
        let zones = Self(zones);
        for zone in &zones.0 {
            for nameserver in &zone.nameservers {
                if let Some(around) = zones.containing(&LowerName::new(nameserver)) {
                    return Err(ZoneError::NameserverInZone {
                        nameserver: nameserver.to_string(),
                        zone: around.origin.to_string(),
                    });
                }
            }
        }
        Ok(zones)
    }

    /// The zones, in configuration order.
    pub fn iter(&self) -> impl Iterator<Item = &Zone> {
        self.0.iter()
    }

    /// The zone containing `name`, if any; unique because the zones are disjoint.
    fn containing(&self, name: &LowerName) -> Option<&Zone> {
        self.0.iter().find(|zone| zone.contains(name))
    }

    /// Where `name` lies among these zones (the module doc's `place`).
    pub fn place<O>(&self, name: &LowerName) -> Placement<'_, O>
    where O: Owner {
        let Some(zone) = self.containing(name) else {
            return Placement::Foreign;
        };
        let depth =
            usize::from(name.num_labels()).saturating_sub(usize::from(zone.origin.num_labels()));
        if depth == 0 {
            return Placement::Apex(zone);
        }
        let name = Name::from(name);
        // Labels leftmost first: `depth - 1` labels under the owner, then the owner's label.
        let labels = name
            .iter()
            .take(depth)
            .map(std::str::from_utf8)
            .collect::<Result<Vec<_>, _>>();
        let Ok(labels) = labels else {
            return Placement::Absent(zone);
        };
        let Some((owner_label, sub_labels)) = labels.split_last() else {
            return Placement::Absent(zone);
        };
        match (
            O::from_label(owner_label),
            SubName::from_labels(sub_labels.iter().copied()),
        ) {
            (Some(owner), Ok(sub)) => Placement::Owned { zone, owner, sub },
            _ => Placement::Absent(zone),
        }
    }
}

#[cfg(test)]
mod test_zone;
