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
    /// Current lanes and the next unused lane identity.
    state: RwLock<PacedLaneTable>,
}

/// Lanes by namespace, and the identity the next newly paced namespace receives.
#[derive(Debug, Default)]
struct PacedLaneTable {
    /// The registered lane of each paced namespace.
    lanes: HashMap<String, PacedLane>,
    /// Identity of the next namespace that becomes paced.
    next_id: u32,
}

impl PacedLaneTable {
    /// Install `namespace`'s declared rate, or remove its lane when it declares none.
    ///
    /// A namespace keeps its lane identity across a replacement, so replacing a protocol never
    /// hands its neighbours a fresh allowance.
    fn install(&mut self, namespace: &str, rate: Option<PacedRate>) {
        match rate {
            Some(rate) => {
                let id = match self.lanes.get(namespace) {
                    Some(lane) => lane.id(),
                    None => {
                        let id = PacedLaneId::new(self.next_id);
                        self.next_id = self.next_id.saturating_add(1);
                        id
                    }
                };
                self.lanes
                    .insert(namespace.to_string(), PacedLane::new(id, rate));
            }
            None => {
                self.lanes.remove(namespace);
            }
        }
    }
}

impl PacedLanes {
    /// Install the declared rates of a batch of namespaces at once.
    ///
    /// Pre: the caller holds the handler table's write lock and has committed the batch, so a
    /// namespace is paced exactly while its protocol is the registered one.
    pub(crate) fn install<'a>(
        &self,
        declarations: impl IntoIterator<Item = (&'a str, Option<PacedRate>)>,
    ) -> Result<()> {
        let mut table = self.state.write().map_err(|_| Error::Lock)?;
        for (namespace, rate) in declarations {
            table.install(namespace, rate);
        }
        Ok(())
    }

    /// Resolve the lane of an encoded [`Envelope`] from its namespace prefix alone.
    ///
    /// Undecodable bytes and unpaced namespaces resolve to `None`, the default lane.
    pub(crate) fn resolve(&self, envelope: &[u8]) -> Option<PacedLane> {
        let namespace = Envelope::namespace_of(envelope).ok()?;
        self.state.read().ok()?.lanes.get(namespace).copied()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use bytes::Bytes;

    use super::*;

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
        lanes.install([("onion", Some(rate(100))), ("chat", None)])?;
        let onion = lanes.resolve(&envelope("onion")).expect("onion is paced");
        assert_eq!(onion.rate(), rate(100));
        assert_eq!(lanes.resolve(&envelope("chat")), None);
        assert_eq!(lanes.resolve(b"not an envelope"), None);

        lanes.install([("onion", Some(rate(50)))])?;
        let replaced = lanes.resolve(&envelope("onion")).expect("still paced");
        assert_eq!(replaced.id(), onion.id());
        assert_eq!(replaced.rate(), rate(50));

        lanes.install([("onion", None)])?;
        assert_eq!(lanes.resolve(&envelope("onion")), None);
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
