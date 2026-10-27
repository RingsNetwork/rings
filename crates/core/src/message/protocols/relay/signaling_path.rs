#![deny(missing_docs)]

//! The recorded path of connect signaling (#947).
//!
//! Every carrier is pathless (#736) except connect signaling: the offer `ConnectNodeSend`, its
//! answer `ConnectNodeReport`, and the counter-offer a tie-break winner sends back. The privacy of
//! that one family is already spent, since the two ends exchange addresses in their SDP and the
//! carrier names both DIDs. Its carrier therefore keeps the relays it crossed, and a reply retraces
//! them instead of being routed afresh over a topology that may have just dead-ended.
//!
//! A path is outbound or a return. Both record the relays they cross in `crossed`, read as a
//! stack with its top last; a return also holds `ahead`, the relays it must still take:
//!
//! ```text
//! Outbound(crossed)        routed by the DHT; each forwarding hop c pushes c onto crossed
//! Return(ahead, crossed)   the next hop is top(ahead), or the destination once ahead = ε;
//!                          each forwarding hop pops ahead and pushes itself onto crossed
//!
//! reply(Outbound(c)) = reply(Return(_, c)) = Return(c, ε)
//! ```
//!
//! Law (Retrace): a payload that crossed the relays `r₁ … r_k` from `o` to `d` carries
//! `crossed = r₁ … r_k`; the reply `d` sends takes `r_k, …, r₁, o` in that order, each a hop
//! the payload itself just crossed, and arrives with `crossed = r_k … r₁`, so the reply to that
//! reply retraces `r₁ … r_k` again. Every hop of a return is checked to be a direct link of the
//! hop that takes it; a hop that lost the link drops the reply, which the initiator's pending
//! timeout covers.
//!
//! Law (Bound): `|ahead| ≤ MAX_RELAY_HOPS` and `|crossed| ≤ MAX_RELAY_HOPS`, established by
//! every constructor and by decoding. A forward spends one unit of the hop budget and pushes one
//! DID, so an honest carrier never reaches the bound; a carrier that does is dropped.
//!
//! Law (Distinct): the hops of `ahead ∪ crossed` are pairwise distinct and exclude both the
//! node holding the carrier and its destination. An honest route never revisits a node (the
//! delivery law has no cycles), so a repeat proves forgery and the carrier is refused
//! ([`SignalingPath::validate_at`]); forwarding preserves the law, since the forwarding node is
//! not on the path it pushes itself onto.
//!
//! The path is outside every signature, as the rest of the carrier is. A relay that forges or
//! truncates it can only misdeliver this one reply, which equals dropping it.
//!
//! Encoding (#952): a hop is its DID's 20 canonical bytes, where a `Did` alone serializes in its
//! 42-character string form, so a hop costs 20 bytes on the wire instead of 43. Only the path's
//! hops take this form; every other DID of the carrier keeps its own.

use ethereum_types::H160;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;

use crate::consts::MAX_RELAY_HOPS;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

/// A sequence of pairwise-distinct relay DIDs bounded by [`MAX_RELAY_HOPS`] (`Law (Bound)`,
/// `Law (Distinct)`), each encoded as its 20 canonical bytes (module docs, #952).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayPath(Vec<Did>);

/// The wire form of one hop: the DID's 20 canonical bytes.
type WireHop = [u8; 20];

impl Serialize for RelayPath {
    /// The hops in order, each as its 20 canonical bytes.
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_seq(
            self.0
                .iter()
                .map(|hop| -> WireHop { H160::from(*hop).to_fixed_bytes() }),
        )
    }
}

impl<'de> Deserialize<'de> for RelayPath {
    /// Decode the hops from their 20 canonical bytes, then admit them as any path is admitted
    /// (`Law (Bound)`, `Law (Distinct)`).
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let hops = Vec::<WireHop>::deserialize(deserializer)?
            .into_iter()
            .map(|bytes| Did::from(H160::from(bytes)))
            .collect::<Vec<_>>();
        Self::try_from(hops).map_err(serde::de::Error::custom)
    }
}

impl RelayPath {
    /// The relays in order, the most recent last.
    pub(crate) fn hops(&self) -> &[Did] {
        &self.0
    }

    /// The most recent relay.
    fn top(&self) -> Option<Did> {
        self.0.last().copied()
    }

    /// The path with `hop` appended; refused past the bound or when `hop` is already on it.
    fn pushed(&self, hop: Did) -> Result<Self> {
        let mut hops = self.0.clone();
        hops.push(hop);
        Self::try_from(hops)
    }

    /// The path without its most recent relay.
    fn popped(&self) -> Self {
        let mut hops = self.0.clone();
        hops.pop();
        Self(hops)
    }
}

impl TryFrom<Vec<Did>> for RelayPath {
    type Error = Error;

    /// Admit a path only inside the bound and without a repeated hop, so decoding cannot carry
    /// an unbounded or cyclic path.
    fn try_from(hops: Vec<Did>) -> Result<Self> {
        if hops.len() > usize::from(MAX_RELAY_HOPS) {
            return Err(Error::RelayPathAboveMax(hops.len()));
        }
        if let Some(hop) = first_repeat(hops.iter().copied()) {
            return Err(Error::RelayPathRepeatedHop { hop });
        }
        Ok(Self(hops))
    }
}

impl From<RelayPath> for Vec<Did> {
    fn from(path: RelayPath) -> Self {
        path.0
    }
}

/// The first DID `hops` repeats, if any.
fn first_repeat(hops: impl IntoIterator<Item = Did>) -> Option<Did> {
    let mut seen = std::collections::BTreeSet::new();
    hops.into_iter().find(|hop| !seen.insert(*hop))
}

/// An outbound path: routed by the DHT, recording the relays it crosses.
#[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboundPath {
    /// The relays crossed so far, the most recent last.
    crossed: RelayPath,
}

impl OutboundPath {
    /// The relays crossed so far.
    pub(crate) fn crossed(&self) -> &RelayPath {
        &self.crossed
    }

    /// The path after the forwarding hop `current` recorded itself.
    fn forwarded(&self, current: Did) -> Result<Self> {
        Ok(Self {
            crossed: self.crossed.pushed(current)?,
        })
    }
}

/// A return path: retracing recorded relays toward the destination, recording those it crosses.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ReturnPath {
    /// The relays still to take before the destination, the next one last.
    ahead: RelayPath,
    /// The relays crossed so far, the most recent last.
    crossed: RelayPath,
}

impl ReturnPath {
    /// The relays crossed so far.
    pub(crate) fn crossed(&self) -> &RelayPath {
        &self.crossed
    }

    /// The hop a carrier toward `destination` takes next: `top(ahead)`, or `destination` once
    /// `ahead` is empty.
    pub(crate) fn return_hop(&self, destination: Did) -> Did {
        self.ahead.top().unwrap_or(destination)
    }

    /// The path once the carrier left along [`Self::return_hop`]: `ahead` without its top.
    pub(crate) fn departed(&self) -> Self {
        Self {
            ahead: self.ahead.popped(),
            crossed: self.crossed.clone(),
        }
    }

    /// The path after the forwarding hop `current` recorded itself and the carrier departed.
    fn forwarded(&self, current: Did) -> Result<Self> {
        Ok(Self {
            ahead: self.ahead.popped(),
            crossed: self.crossed.pushed(current)?,
        })
    }
}

/// The recorded path of a connect-signaling carrier (see the module documentation).
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub enum SignalingPath {
    /// Routed by the DHT toward its destination.
    Outbound(OutboundPath),
    /// Retracing a recorded path toward its destination.
    Return(ReturnPath),
}

impl SignalingPath {
    /// The path of a freshly originated offer: outbound, nothing crossed.
    pub(crate) fn outbound() -> Self {
        Self::Outbound(OutboundPath::default())
    }

    /// The relays this carrier crossed, the most recent last.
    pub(crate) fn crossed(&self) -> &RelayPath {
        match self {
            Self::Outbound(path) => path.crossed(),
            Self::Return(path) => path.crossed(),
        }
    }

    /// The path of the reply to this carrier: a return along the relays it crossed.
    ///
    /// Law: `reply(p) = Return(crossed(p), ε)`.
    pub(crate) fn reply(&self) -> ReturnPath {
        ReturnPath {
            ahead: self.crossed().clone(),
            crossed: RelayPath::default(),
        }
    }

    /// The hop a carrier toward `destination` must take next when it retraces a path; `None`
    /// for an outbound carrier, which the DHT routes.
    pub(crate) fn return_hop(&self, destination: Did) -> Option<Did> {
        match self {
            Self::Outbound(_) => None,
            Self::Return(path) => Some(path.return_hop(destination)),
        }
    }

    /// The path after the forwarding hop `current` recorded itself and the carrier departed.
    pub(crate) fn forwarded(&self, current: Did) -> Result<Self> {
        Ok(match self {
            Self::Outbound(path) => Self::Outbound(path.forwarded(current)?),
            Self::Return(path) => Self::Return(path.forwarded(current)?),
        })
    }

    /// Refuse a path that breaks `Law (Distinct)` at the node `local` holding the carrier toward
    /// `destination`: a hop repeated across `ahead` and `crossed`, or `local` or `destination`
    /// on the path.
    pub(crate) fn validate_at(&self, local: Did, destination: Did) -> Result<()> {
        let ahead = match self {
            Self::Outbound(_) => &[][..],
            Self::Return(path) => path.ahead.hops(),
        };
        // At the destination itself `local = destination` names one node, not a repeat.
        let ends = if local == destination {
            vec![local]
        } else {
            vec![local, destination]
        };
        let hops = ahead.iter().chain(self.crossed().hops()).copied();
        match first_repeat(hops.chain(ends)) {
            Some(hop) => Err(Error::RelayPathRepeatedHop { hop }),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod test_signaling_path;
