//! The paced direct-edge lane of final-destination origin quotas (#888).
//!
//! Every final-destination transaction is charged to an origin quota record keyed by
//! `(network, origin account, destination, lane)`. By default the lane is the message's
//! [`MessageCategory`], and every Application namespace shares the Application limits.
//!
//! Some application protocols emit direct-edge traffic at a constant, protocol-defined rate,
//! and bound each sending neighbour by their own admission before any decryption (for the onion
//! data plane, the per-link budget of `B` cells per `V` seconds). For such traffic the generic
//! Application quota is a second, tighter bound on the same traffic, and it refuses what the
//! protocol admits. The owning protocol therefore supplies the rate of a dedicated lane:
//!
//! ```text
//! lane(m, edge, paced) ≜ Paced(p)   if class(m) = Application ∧ edge = Neighbour ∧ paced = p
//!                        Class(c)   otherwise, with c = class(m)
//! ```
//!
//! Core takes only a [`PacedRate`] and an opaque [`PacedLaneId`]; it has no vocabulary of the
//! protocol that owns the lane.
//!
//! # Why only the authenticated neighbour may claim the lane
//!
//! The lane is granted only when [`EdgeRelation::Neighbour`] holds, that is, the frame arrived
//! on the handshake-authenticated connection of the transaction's own origin account. Its rate
//! is a per-link rate, and the owning protocol enforces it per sending neighbour:
//!
//! - A relayed transaction arrives from a neighbour that is not its origin. Its origin's
//!   traffic reaches us through any number of neighbours, so a per-link rate granted per origin
//!   would be multiplied by the number of paths, and no per-link admission bounds it.
//! - A neighbour could replay or fabricate a foreign origin's traffic into the lane. The replay
//!   window rejects exact replays, but a relay forwarding a foreign origin's fresh traffic at
//!   the paced rate would be admitted at a rate the foreign origin never negotiated with us.
//! - The relay carrier (`hop_budget`, next hop) is not signed, so "direct" cannot be read from
//!   the payload. The only unforgeable witness is the authenticated connection itself.
//!
//! Traffic that fails the neighbour predicate falls back to its class lane with the default
//! limits, so the paced lane never widens what a relayed or foreign origin can send.

use std::num::NonZeroU64;

use crate::dht::Did;
use crate::message::types::MessageCategory;

/// A per-origin admission rate that an application protocol grants its paced lane:
/// at most `budget` messages per `period_seconds`, with an instantaneous allowance of `budget`.
///
/// This is the bound a window admission of `budget` messages per `period_seconds` enforces,
/// expressed as a token bucket. For the onion data plane it is the per-link budget `B/V`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PacedRate {
    /// Messages admitted per period, and the instantaneous allowance.
    budget: NonZeroU64,
    /// Length of one period in seconds.
    period_seconds: NonZeroU64,
}

impl PacedRate {
    /// A rate of `budget` messages per `period_seconds`.
    pub const fn new(budget: NonZeroU64, period_seconds: NonZeroU64) -> Self {
        Self {
            budget,
            period_seconds,
        }
    }

    /// Messages admitted per period, and the instantaneous allowance.
    pub const fn budget(self) -> NonZeroU64 {
        self.budget
    }

    /// Length of one period in seconds.
    pub const fn period_seconds(self) -> NonZeroU64 {
        self.period_seconds
    }
}

/// Opaque identity of one paced lane, chosen by the application layer that registers it.
///
/// Distinct lanes keep distinct quota records, so two protocols with equal rates never share
/// an allowance.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PacedLaneId(u32);

impl PacedLaneId {
    /// Name a paced lane.
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The raw identity.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// One registered paced lane: its identity and the rate its owning protocol supplied.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PacedLane {
    /// Identity of the lane.
    id: PacedLaneId,
    /// Rate supplied by the owning protocol.
    rate: PacedRate,
}

impl PacedLane {
    /// A lane named `id` admitting `rate` per origin.
    pub const fn new(id: PacedLaneId, rate: PacedRate) -> Self {
        Self { id, rate }
    }

    /// Identity of the lane.
    pub const fn id(self) -> PacedLaneId {
        self.id
    }

    /// Rate supplied by the owning protocol.
    pub const fn rate(self) -> PacedRate {
        self.rate
    }
}

/// How the delivering connection relates to a transaction's origin account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EdgeRelation {
    /// The frame arrived on the handshake-authenticated connection of the origin itself.
    Neighbour,
    /// The origin is not the authenticated neighbour: relayed, foreign, local or unauthenticated.
    Remote,
}

impl EdgeRelation {
    /// Relate an authenticated connection peer to an origin account.
    ///
    /// `authenticated_peer` must be `Some` only when the connection's handshake authenticated
    /// that peer and its generation is still active; any other frame is [`Self::Remote`].
    pub fn of(authenticated_peer: Option<Did>, origin: Did) -> Self {
        if authenticated_peer == Some(origin) {
            Self::Neighbour
        } else {
            Self::Remote
        }
    }
}

/// The logical quota lane one admitted transaction is charged to, with the limits it implies.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OriginQuotaLane {
    /// The message class's lane under the configured limits.
    Class(MessageCategory),
    /// A paced direct-edge lane under the rate its owning protocol supplied.
    Paced(PacedLane),
}

impl OriginQuotaLane {
    /// Select the lane: see the module documentation for the law.
    ///
    /// `paced` resolves the lane the application layer registered for the message. It is
    /// consulted only for Application traffic from the authenticated neighbour that originated
    /// it, so no other traffic reaches the application layer's classifier.
    pub fn select(
        class: MessageCategory,
        edge: EdgeRelation,
        paced: impl FnOnce() -> Option<PacedLane>,
    ) -> Self {
        (class == MessageCategory::Application && edge == EdgeRelation::Neighbour)
            .then(paced)
            .flatten()
            .map_or(Self::Class(class), Self::Paced)
    }

    /// The identity a quota record of this lane is keyed by.
    pub const fn id(self) -> OriginQuotaLaneId {
        match self {
            Self::Class(class) => OriginQuotaLaneId::Class(class),
            Self::Paced(lane) => OriginQuotaLaneId::Paced(lane.id()),
        }
    }
}

/// Identity of a quota lane, as an [`OriginQuotaKey`](crate::message::OriginQuotaKey) stores
/// it: the lane's rate is an input to its limits, never part of a record's identity.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OriginQuotaLaneId {
    /// A message class's lane.
    Class(MessageCategory),
    /// A paced direct-edge lane.
    Paced(PacedLaneId),
}

impl From<MessageCategory> for OriginQuotaLane {
    fn from(class: MessageCategory) -> Self {
        Self::Class(class)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// A lane the application layer would register.
    fn registered() -> PacedLane {
        PacedLane::new(
            PacedLaneId::new(7),
            PacedRate::new(NonZeroU64::MIN, NonZeroU64::MIN),
        )
    }

    /// The selection law over every class and edge relation, with and without a registered
    /// lane; the classifier is consulted only where the law can grant the lane.
    #[test]
    fn test_only_neighbour_application_traffic_may_select_a_paced_lane() {
        let classes = [
            MessageCategory::DhtControl,
            MessageCategory::Storage,
            MessageCategory::E2e,
            MessageCategory::Application,
        ];
        for class in classes {
            for edge in [EdgeRelation::Neighbour, EdgeRelation::Remote] {
                for paced in [None, Some(registered())] {
                    let consulted = Cell::new(false);
                    let lane = OriginQuotaLane::select(class, edge, || {
                        consulted.set(true);
                        paced
                    });
                    let eligible =
                        class == MessageCategory::Application && edge == EdgeRelation::Neighbour;
                    assert_eq!(consulted.get(), eligible);
                    let expected = match (eligible, paced) {
                        (true, Some(lane)) => OriginQuotaLane::Paced(lane),
                        _ => OriginQuotaLane::Class(class),
                    };
                    assert_eq!(lane, expected);
                }
            }
        }
    }

    /// Only the authenticated peer that is the origin itself is a neighbour.
    #[test]
    fn test_edge_relation_requires_the_authenticated_origin() {
        let origin = Did::from(1_u32);
        assert_eq!(
            EdgeRelation::of(Some(origin), origin),
            EdgeRelation::Neighbour
        );
        assert_eq!(
            EdgeRelation::of(Some(Did::from(2_u32)), origin),
            EdgeRelation::Remote
        );
        assert_eq!(EdgeRelation::of(None, origin), EdgeRelation::Remote);
    }
}
