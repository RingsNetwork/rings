#![deny(missing_docs)]

//! The relay carrier of a payload: where it goes next, where it ends, and how many forwards it
//! has left.
//!
//! A carrier is the quadruple `(next_hop, destination, hop_budget, stage)`. Forwarding is the
//! partial map
//!
//! ```text
//! forward(current, (next', σ')) : (current, d, n, σ) ↦ (next', d, n − 1, σ')    defined iff n > 0
//! ```
//!
//! so the budget component walks the finite chain `MAX > … > 1 > 0` and never climbs it; a report
//! is a fresh carrier, not a continuation: its stage is derived from the signed request alone
//! (its `reply_via`), never from the request's carrier. The stage `σ` is the delivery automaton's
//! state (see [`RouteStage`]): the route's aim and whether it was handed past that aim. The
//! carrier records nothing about the hops already taken: each hop learns its predecessor from the
//! transport edge it received on and its successor from `next_hop`, and the destination learns
//! only the last hop. Delivery is bounded by its own progress law
//! ([`delivery`](crate::dht::delivery)): no route cycles, so a route outruns its budget only when
//! a correct route is longer than the budget (a ring larger than it whose finger table does not
//! span the identifier space), and budget exhaustion is the witness that replaces any
//! history-based loop detection.
//!
//! The carrier is outside every signature: it is rewritten by each hop under that hop's own
//! transport edge. A budget therefore bounds the work honest hops do for one payload; it is not a
//! promise a dishonest hop keeps.

use serde::Deserialize;
use serde::Serialize;

use crate::consts::MAX_RELAY_HOPS;
use crate::dht::delivery::NextHop;
use crate::dht::delivery::RouteStage;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

/// The number of forwards a payload may still take.
///
/// Invariant: `0 ≤ n ≤ MAX_RELAY_HOPS`, established by every constructor and by decoding, so a
/// carrier received from a peer can never claim more forwards than a fresh one.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(try_from = "u8", into = "u8")]
pub struct HopBudget(u8);

impl HopBudget {
    /// The top of the chain: what a fresh carrier holds, and the most any carrier can hold.
    pub const MAX: Self = Self(MAX_RELAY_HOPS);

    /// The bottom of the chain: a payload that can be delivered but not forwarded.
    pub const EXHAUSTED: Self = Self(0);

    /// The forwards remaining.
    pub const fn remaining(self) -> u8 {
        self.0
    }

    /// Spend one forward: the predecessor on the chain, undefined at the bottom.
    ///
    /// Law: `spend(n) = Some(n − 1)` iff `n > 0`; `spend` is strictly decreasing where defined.
    pub fn spend(self) -> Option<Self> {
        self.0.checked_sub(1).map(Self)
    }
}

impl TryFrom<u8> for HopBudget {
    type Error = Error;

    /// Admit a budget only inside the invariant, so decoding a carrier cannot mint forwards.
    fn try_from(remaining: u8) -> Result<Self> {
        if remaining > MAX_RELAY_HOPS {
            return Err(Error::RelayHopBudgetAboveMax(remaining));
        }
        Ok(Self(remaining))
    }
}

impl From<HopBudget> for u8 {
    fn from(budget: HopBudget) -> Self {
        budget.remaining()
    }
}

/// The relay carrier of a payload (see the module documentation).
///
/// Every payload is sent under a carrier. A handler picks the transport by `next_hop`; a
/// forwarding hop chooses the next carrier from `destination`.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct MessageRelay {
    /// The node that handles the payload next.
    pub next_hop: Did,

    /// The node the payload is routed toward.
    ///
    /// A sender that does not know the final destination names `next_hop` here, and a later hop
    /// re-aims the carrier with [`Self::reset_destination`].
    pub destination: Did,

    /// The forwards the payload may still take.
    pub hop_budget: HopBudget,

    /// The delivery stage: the route's aim, and whether it was handed past that aim.
    pub stage: RouteStage,
}

impl MessageRelay {
    /// A fresh carrier whose first hop is the delivery decision `hop`, stage included.
    pub fn new(hop: NextHop, destination: Did, hop_budget: HopBudget) -> Self {
        Self {
            next_hop: hop.peer,
            destination,
            hop_budget,
            stage: hop.stage,
        }
    }

    /// The carrier `current` sends on toward `self.destination` after the delivery decision
    /// `hop`.
    ///
    /// Pre: `self` was addressed to `current`.
    /// Post: `Ok` spends exactly one forward and sets `stage = hop.stage`;
    /// `Err(RelayHopBudgetExhausted)` is the drop of a payload that has taken every forward it
    /// was given.
    pub fn forward(&self, current: Did, hop: NextHop) -> Result<Self> {
        self.validate(current)?;
        let hop_budget = self
            .hop_budget
            .spend()
            .ok_or(Error::RelayHopBudgetExhausted)?;

        Ok(Self {
            next_hop: hop.peer,
            destination: self.destination,
            hop_budget,
            stage: hop.stage,
        })
    }

    /// The fresh carrier of a report `current` sends for the request carried by `self`: it
    /// holds [`HopBudget::MAX`], as every fresh carrier does, whatever `self` has left.
    ///
    /// Pre: `self` was addressed to `current`; `hop` was decided by the caller toward
    /// `destination`.
    pub fn report(&self, current: Did, destination: Did, hop: NextHop) -> Result<Self> {
        self.validate(current)?;

        Ok(Self::new(hop, destination, HopBudget::MAX))
    }

    /// The same carrier aimed at `destination`.
    ///
    /// A sender that does not know the final destination names its next hop as the destination;
    /// a hop that resolves a farther node re-aims the carrier here before forwarding it. A new
    /// destination starts a new route, so the stage returns to [`RouteStage::TOWARD`].
    pub fn reset_destination(&self, destination: Did) -> Self {
        Self {
            destination,
            stage: RouteStage::TOWARD,
            ..self.clone()
        }
    }

    /// Check that this carrier was addressed to `current`.
    pub fn validate(&self, current: Did) -> Result<()> {
        if self.next_hop != current {
            return Err(Error::InvalidNextHop);
        }

        Ok(())
    }
}

#[cfg(test)]
mod test_relay;
