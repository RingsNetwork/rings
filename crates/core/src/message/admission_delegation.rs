//! Delegated admission of direct-edge application traffic (#888).
//!
//! Every final-destination transaction is charged to the origin quota record keyed by
//! `(network, origin account, destination, class)`, with a message bucket and a byte bucket.
//!
//! Some application protocols admit each sending neighbour themselves, through their own
//! per-neighbour admission, before any further processing. For their direct-edge traffic the
//! generic per-origin message limit is a second, tighter bound on the same traffic, and it refuses
//! what the protocol admits. Such a namespace may declare *delegated admission*, a flag with no
//! rate, and core then skips only the message-count limit for it:
//!
//! ```text
//! limit(m, edge, delegated) ≜ Delegated   if class(m) = Application ∧ edge = Neighbour ∧ delegated
//!                             Enforced    otherwise
//! ```
//!
//! A delegated transaction neither needs nor consumes a message token. It is still charged to
//! the same record's byte bucket, and the record bound still applies, so the byte bound on the
//! neighbour's application traffic is unchanged. Core takes a flag, not a rate, and has no
//! vocabulary of the protocol that declares it.
//!
//! # Why only the authenticated neighbour qualifies
//!
//! Delegation applies only when [`EdgeRelation::Neighbour`] holds, that is, the frame arrived
//! on the handshake-authenticated connection of the transaction's own origin account. The
//! declaring protocol's admission is per sending neighbour:
//!
//! - A relayed transaction arrives from a neighbour that is not its origin. Its origin's
//!   traffic reaches us through any number of neighbours, and no per-neighbour admission bounds
//!   it by origin, so skipping the per-origin limit would leave it unbounded in messages.
//! - A relay forwarding a foreign origin's fresh traffic must not escape that origin's limit
//!   just because the namespace delegates admission for direct neighbours.
//! - The relay carrier (`hop_budget`, next hop) is not signed, so "direct" cannot be read from
//!   the payload. The only unforgeable witness is the authenticated connection itself.
//!
//! Traffic that fails the predicate keeps the enforced class limits, so delegation never widens
//! what a relayed or foreign origin can send.
//!
//! # Trust
//!
//! Delegation is trusted local configuration. It enters through `SwarmCallback` (and, in the
//! node, a registered `Protocol`), with the authority of the operator who configures the quota.
//! A namespace that declares it must enforce its own per-neighbour admission.

use crate::dht::Did;
use crate::message::types::MessageCategory;

/// How the delivering connection relates to a transaction's origin account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EdgeRelation {
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
    pub(crate) fn of(authenticated_peer: Option<Did>, origin: Did) -> Self {
        if authenticated_peer == Some(origin) {
            Self::Neighbour
        } else {
            Self::Remote
        }
    }
}

/// Whether an admission checks and consumes the per-origin message limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageLimit {
    /// The message bucket must hold a token, and the admission consumes it.
    Enforced,
    /// The declaring namespace admits this traffic itself: the message bucket is skipped.
    Delegated,
}

/// How one final-destination transaction is charged: its class record, and whether that
/// record's message limit applies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OriginQuotaCharge {
    /// The class whose origin record is charged.
    pub(crate) lane: MessageCategory,
    /// Whether the record's message limit applies.
    pub(crate) message_limit: MessageLimit,
}

impl OriginQuotaCharge {
    /// Select the charge: see the module documentation for the law.
    ///
    /// `delegated` asks the application layer whether the message's namespace declared delegated
    /// admission. It is consulted only for Application traffic from the authenticated neighbour
    /// that originated it, so no other traffic reaches the application layer's classifier.
    pub(crate) fn select(
        lane: MessageCategory,
        edge: EdgeRelation,
        delegated: impl FnOnce() -> bool,
    ) -> Self {
        let eligible = lane == MessageCategory::Application && edge == EdgeRelation::Neighbour;
        let message_limit = if eligible && delegated() {
            MessageLimit::Delegated
        } else {
            MessageLimit::Enforced
        };
        Self {
            lane,
            message_limit,
        }
    }
}

impl From<MessageCategory> for OriginQuotaCharge {
    fn from(lane: MessageCategory) -> Self {
        Self {
            lane,
            message_limit: MessageLimit::Enforced,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::EdgeRelation;
    use super::MessageLimit;
    use super::OriginQuotaCharge;
    use crate::dht::Did;
    use crate::message::types::MessageCategory;

    /// The selection law over every class and edge relation, with and without a declaration;
    /// the classifier is consulted only where the law can delegate.
    #[test]
    fn test_only_neighbour_application_traffic_may_be_delegated() {
        let classes = [
            MessageCategory::DhtControl,
            MessageCategory::Storage,
            MessageCategory::E2e,
            MessageCategory::Application,
        ];
        for lane in classes {
            for edge in [EdgeRelation::Neighbour, EdgeRelation::Remote] {
                for declared in [false, true] {
                    let consulted = Cell::new(false);
                    let charge = OriginQuotaCharge::select(lane, edge, || {
                        consulted.set(true);
                        declared
                    });
                    let eligible =
                        lane == MessageCategory::Application && edge == EdgeRelation::Neighbour;
                    assert_eq!(consulted.get(), eligible);
                    assert_eq!(charge.lane, lane);
                    let expected = if eligible && declared {
                        MessageLimit::Delegated
                    } else {
                        MessageLimit::Enforced
                    };
                    assert_eq!(charge.message_limit, expected);
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
