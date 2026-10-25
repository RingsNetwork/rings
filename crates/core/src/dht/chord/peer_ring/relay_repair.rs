//! Chain repair of a [`PeerRing`] (#777, `Law (Chain repair)`; durable-records, Section X): the
//! one digest-set offer per chain neighbour a repair pass sends, the carriers a neighbour wants
//! of it, and the carriers this node sends for them.
//!
//! ```text
//! pass:    m ──RelayCarrierDigests(carriers whose chain holds p₁)──▶ p₁
//!          m ──RelayCarrierDigests(carriers whose chain holds n₁)──▶ n₁
//! offer:   neighbour ──RelayCarriersWanted(keys it lacks or holds otherwise)──▶ m
//! wanted:  m ──sync (additive copy) of those carriers──▶ neighbour
//! ```
//!
//! Every carrier a member keeps has `p₁` and `n₁` as its previous and next member but at the
//! chain's ends, so a pass sends at most two offers whatever the carriers kept (Prop. 13 of
//! durable-records), and bytes move only for a difference.

use std::collections::BTreeMap;

use super::PeerRing;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::Did;
use crate::dht::StorageKey;
use crate::error::Result;
use crate::message::RelayCarrierDigest;

impl PeerRing {
    /// The live relay carriers this node keeps, by key.
    async fn live_relay_carriers(&self, now_ms: u128) -> Result<BTreeMap<Did, Entry>> {
        Ok(self
            .live_storage_entries(now_ms)
            .await?
            .into_iter()
            .filter(|(key, _)| key.kind().is_relay_inbox())
            .map(|(key, entry)| (key.placement(), entry))
            .collect())
    }

    /// The offers of one repair pass (`Law (Chain repair)`): for each chain neighbour, the
    /// digest of every carrier this node keeps as a member whose chain holds that neighbour as
    /// its previous or next member.
    ///
    /// Post: every offer goes to a member next to this one in some carrier's chain, which is
    /// `p₁` or `n₁`; each carrier is offered to each of its two neighbours at most once.
    pub(crate) async fn relay_repair_offers(
        &self,
        now_ms: u128,
    ) -> Result<BTreeMap<Did, Vec<RelayCarrierDigest>>> {
        let mut offers: BTreeMap<Did, Vec<RelayCarrierDigest>> = BTreeMap::new();
        for (key, carrier) in self.live_relay_carriers(now_ms).await? {
            let Some(position) = self.relay_chain_position(key)? else {
                continue;
            };
            let digest = RelayCarrierDigest::of(key, &carrier)?;
            let next = self.relay_chain_next(key, position)?;
            for neighbour in position.previous.into_iter().chain(next) {
                offers.entry(neighbour).or_default().push(digest);
            }
        }
        Ok(offers)
    }

    /// The keys of `offered`, a repair offer from `sender`, whose carrier this node lacks or
    /// holds in another state, of the keys whose chain makes `sender` its neighbour (`Law
    /// (Replica admission)`).
    pub(crate) async fn wanted_relay_carriers(
        &self,
        sender: Did,
        offered: &[RelayCarrierDigest],
        now_ms: u128,
    ) -> Result<Vec<Did>> {
        let mut wanted = Vec::new();
        for offer in offered {
            if !self.admits_relay_copy(offer.key, sender)? {
                continue;
            }
            let held = self
                .live_storage_entry(StorageKey::new(EntryKind::RelayMessage, offer.key), now_ms)
                .await?;
            let same = match held {
                Some(carrier) => RelayCarrierDigest::of(offer.key, &carrier)? == *offer,
                None => false,
            };
            if !same {
                wanted.push(offer.key);
            }
        }
        Ok(wanted)
    }

    /// The carriers of `keys` this node keeps whose chain makes `requester` its neighbour, to
    /// copy to it: what it asked for after this node's offer.
    pub(crate) async fn relay_carriers_wanted_by(
        &self,
        requester: Did,
        keys: &[Did],
        now_ms: u128,
    ) -> Result<Vec<(Did, Entry)>> {
        let mut carriers = Vec::new();
        for key in keys {
            if !self.admits_relay_copy(*key, requester)? {
                continue;
            }
            if let Some(carrier) = self
                .live_storage_entry(StorageKey::new(EntryKind::RelayMessage, *key), now_ms)
                .await?
            {
                carriers.push((*key, carrier));
            }
        }
        Ok(carriers)
    }
}
