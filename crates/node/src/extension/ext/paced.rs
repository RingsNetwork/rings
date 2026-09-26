//! Paced direct-edge lanes registered by namespace (#888).
//!
//! A [`Protocol`](super::Protocol) that paces its direct-edge traffic and bounds each sending
//! neighbour itself declares its per-origin rate through
//! [`Protocol::paced_direct_rate`](super::Protocol::paced_direct_rate). The registry records
//! that rate here when the protocol is installed, under the same write lock that installs its
//! handler, and core resolves an inbound application payload to the lane through
//! [`Backend`](crate::extension::Backend):
//!
//! ```text
//!   install : (namespace, Option rate) → lanes'        -- at register / replace
//!   resolve : encoded Envelope ⇀ PacedLane              -- namespace prefix only
//! ```
//!
//! Core decides whether the lane applies at all (Application traffic from the authenticated
//! neighbour that originated it); this table only names the rate, never the eligibility.

use std::collections::HashMap;
use std::sync::RwLock;

use rings_core::message::PacedLane;
use rings_core::message::PacedLaneId;
use rings_core::message::PacedRate;

use super::Envelope;
use crate::error::Error;
use crate::error::Result;

/// The table of paced lanes: one per namespace whose protocol declared a rate.
#[derive(Debug, Default)]
pub(crate) struct PacedLanes {
    /// Lane identities, current rates, and the next unused identity.
    state: RwLock<PacedLaneTable>,
}

/// Lane identities and rates by namespace.
///
/// Invariant: a namespace receives its lane identity the first time it declares a rate and keeps
/// it for the registry's lifetime, even while no rate is declared. Withdrawing and re-declaring
/// a rate therefore never hands a neighbour a fresh allowance, and never orphans the quota
/// records of an abandoned identity. Identities are bounded by the number of distinct paced
/// namespaces.
#[derive(Debug, Default)]
struct PacedLaneTable {
    /// The permanent lane identity of every namespace that ever declared a rate.
    ids: HashMap<String, PacedLaneId>,
    /// The currently declared rate of each paced namespace.
    rates: HashMap<String, PacedRate>,
    /// Identity of the next namespace that declares a rate for the first time.
    next_id: u32,
}

impl PacedLaneTable {
    /// Give `namespace` its permanent identity if it has none yet.
    fn identify(&mut self, namespace: &str) -> Result<()> {
        if self.ids.contains_key(namespace) {
            return Ok(());
        }
        let next = self.next_id.checked_add(1).ok_or_else(|| {
            Error::ExtensionError("paced lane identities are exhausted".to_string())
        })?;
        self.ids
            .insert(namespace.to_string(), PacedLaneId::new(self.next_id));
        self.next_id = next;
        Ok(())
    }

    /// Set or withdraw `namespace`'s declared rate. Its identity is untouched.
    fn declare(&mut self, namespace: &str, rate: Option<PacedRate>) {
        match rate {
            Some(rate) => {
                self.rates.insert(namespace.to_string(), rate);
            }
            None => {
                self.rates.remove(namespace);
            }
        }
    }

    /// The lane of `namespace`, if it currently declares a rate.
    fn lane(&self, namespace: &str) -> Option<PacedLane> {
        let rate = self.rates.get(namespace).copied()?;
        let id = self.ids.get(namespace).copied()?;
        Some(PacedLane::new(id, rate))
    }
}

impl PacedLanes {
    /// Install the declared rates of a batch of namespaces at once.
    ///
    /// Either every declaration applies or none does: identities, the only fallible part, are
    /// assigned first, and an identity assigned before a failure is permanent anyway.
    ///
    /// Pre: the caller holds the handler table's write lock and inserts the batch's handlers
    /// right after, so a namespace is paced exactly while its protocol is the registered one.
    pub(crate) fn install<'a>(
        &self,
        declarations: impl IntoIterator<Item = (&'a str, Option<PacedRate>)> + Clone,
    ) -> Result<()> {
        let mut table = self.state.write().map_err(|_| Error::Lock)?;
        for (namespace, rate) in declarations.clone() {
            if rate.is_some() {
                table.identify(namespace)?;
            }
        }
        for (namespace, rate) in declarations {
            table.declare(namespace, rate);
        }
        Ok(())
    }

    /// Resolve the lane of an encoded [`Envelope`] from its namespace prefix alone.
    ///
    /// Undecodable bytes and unpaced namespaces resolve to `None`, the default lane.
    pub(crate) fn resolve(&self, envelope: &[u8]) -> Option<PacedLane> {
        let namespace = Envelope::namespace_of(envelope).ok()?;
        self.state.read().ok()?.lane(namespace)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use bytes::Bytes;
    use rings_core::message::PacedRate;

    use super::Envelope;
    use super::PacedLanes;
    use crate::error::Result;

    /// A rate of `budget` messages per second.
    fn rate(budget: u64) -> PacedRate {
        PacedRate::new(
            NonZeroU64::new(budget).expect("non-zero budget"),
            NonZeroU64::MIN,
        )
    }

    /// Encode an envelope of `namespace`.
    fn envelope(namespace: &str) -> Vec<u8> {
        Envelope::new(namespace, Bytes::from_static(b"payload"))
            .encode()
            .expect("envelope encodes")
    }

    #[test]
    fn test_declared_rates_resolve_by_namespace_and_keep_identity_on_replace() -> Result<()> {
        let lanes = PacedLanes::default();
        lanes.install([("paced-a", Some(rate(100))), ("chat", None)])?;
        let first = lanes
            .resolve(&envelope("paced-a"))
            .expect("paced-a is paced");
        assert_eq!(first.rate(), rate(100));
        assert_eq!(lanes.resolve(&envelope("chat")), None);
        assert_eq!(lanes.resolve(b"not an envelope"), None);

        lanes.install([("paced-a", Some(rate(50)))])?;
        let replaced = lanes.resolve(&envelope("paced-a")).expect("still paced");
        assert_eq!(replaced.id(), first.id());
        assert_eq!(replaced.rate(), rate(50));

        lanes.install([("paced-a", None)])?;
        assert_eq!(lanes.resolve(&envelope("paced-a")), None);

        // Withdrawn and re-declared: the same identity, so the same quota records.
        lanes.install([("paced-a", Some(rate(100)))])?;
        let redeclared = lanes.resolve(&envelope("paced-a")).expect("paced again");
        assert_eq!(redeclared.id(), first.id());
        Ok(())
    }

    #[test]
    fn test_distinct_namespaces_receive_distinct_lanes() -> Result<()> {
        let lanes = PacedLanes::default();
        lanes.install([("a", Some(rate(1))), ("b", Some(rate(1)))])?;
        let a = lanes.resolve(&envelope("a")).expect("a is paced");
        let b = lanes.resolve(&envelope("b")).expect("b is paced");
        assert_ne!(a.id(), b.id());
        Ok(())
    }
}
