//! Relay custody of a [`PeerRing`]: its own declaration, what it knows of its neighbours', and
//! the holder its view names for a relay key (see `dht::relay_holder`).
//!
//! Every decision runs on one topology snapshot together with the neighbourhood, which is
//! locked inside the snapshot's observation:
//!
//! ```text
//! Law (Lock order) : topology_transition ≺ relay_neighborhood
//! ```
//!
//! The neighbourhood lock is a leaf: nothing is acquired while it is held.

use std::sync::MutexGuard;

use super::PeerRing;
use crate::consts::REFUSED_RELAY_HOLDS_MAX;
use crate::dht::entry::EntryOperation;
use crate::dht::relay_holder::relay_holder;
use crate::dht::relay_holder::PreviousHolder;
use crate::dht::relay_holder::RelayCustody;
use crate::dht::relay_holder::RelayHolder;
use crate::dht::relay_holder::RelayNeighborhood;
use crate::dht::relay_holder::RelaySegment;
use crate::dht::topology::TopologyState;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

impl PeerRing {
    /// Whether this node holds relay mail: it keeps a retained relay store.
    pub fn holds_relay_mail(&self) -> bool {
        self.relay_storage.is_some()
    }

    /// The relay custody this node declares when it connects.
    pub fn relay_custody(&self) -> RelayCustody {
        RelayCustody::of(self.holds_relay_mail())
    }

    /// Lock the neighbourhood, a leaf of `Law (Lock order)`.
    fn lock_relay_neighborhood(&self) -> Result<MutexGuard<'_, RelayNeighborhood>> {
        self.relay_neighborhood
            .lock()
            .map_err(|_| Error::LockPoisoned)
    }

    /// Run `observe` on one topology snapshot and the neighbourhood together.
    fn with_relay_view<T>(
        &self,
        observe: impl FnOnce(&TopologyState, &mut RelayNeighborhood) -> T,
    ) -> Result<T> {
        self.with_topology_state(|state| {
            self.lock_relay_neighborhood()
                .map(|mut neighborhood| observe(state, &mut neighborhood))
        })?
    }

    /// Record the declaration `peer` made in its connection handshake.
    pub(crate) fn observe_relay_custody(&self, peer: Did, custody: RelayCustody) -> Result<()> {
        self.lock_relay_neighborhood()?.observe(peer, custody);
        Ok(())
    }

    /// Forget what is known of `peer`'s relay custody, once it left this node's topology.
    pub(crate) fn forget_relay_custody(&self, peer: Did) -> Result<()> {
        self.lock_relay_neighborhood()?.forget(peer);
        Ok(())
    }

    /// The peers that declared they hold no relay mail.
    pub(crate) fn relay_storage_less_peers(&self) -> Result<Vec<Did>> {
        Ok(self.lock_relay_neighborhood()?.storage_less_peers())
    }

    /// Forget the declarations of `stale`, the peers this node no longer deals with, except
    /// those its topology references, so the record stays bounded by the peers it still knows.
    pub(crate) fn forget_stale_relay_custody(&self, stale: &[Did]) -> Result<()> {
        self.with_relay_view(|state, neighborhood| {
            neighborhood.retain(|peer| !stale.contains(&peer) || state.references(peer));
        })
    }

    /// The segment of the ring this view places relay keys in.
    pub(crate) fn relay_segment(&self) -> Result<RelaySegment> {
        let holds = self.holds_relay_mail();
        self.with_relay_view(|state, neighborhood| neighborhood.segment(state, holds))
    }

    /// The holder this view names for the relay key `key` (`Law (Holder)`).
    pub(crate) fn relay_holder(&self, key: Did) -> Result<RelayHolder> {
        Ok(relay_holder(&self.relay_segment()?, key))
    }

    /// Whether this node holds the relay key `key` under its own view.
    pub(crate) fn holds_relay_key(&self, key: Did) -> Result<bool> {
        Ok(self.relay_holder(key)? == RelayHolder::Holder(self.did))
    }

    /// Take the previous holder `previous` that `from` passed, iff `from` is this node's
    /// predecessor; the result says whether it was taken.
    pub(crate) fn accept_previous_relay_holder(
        &self,
        from: Did,
        previous: PreviousHolder,
    ) -> Result<bool> {
        self.with_relay_view(|state, neighborhood| neighborhood.pass(state, from, previous))
    }

    /// The previous holder this node passes to its successor head, with that head.
    pub(crate) fn previous_relay_holder_to_pass(&self) -> Result<Option<(Did, PreviousHolder)>> {
        let holds = self.holds_relay_mail();
        self.with_relay_view(|state, neighborhood| neighborhood.record_to_pass(state, holds))
    }

    /// Whether `writer` may hold a message for `destination` here (see `HoldAuthority`).
    pub(crate) fn admits_relay_hold(&self, destination: Did, writer: Did) -> Result<bool> {
        self.with_relay_view(|state, neighborhood| {
            neighborhood
                .hold_authority(state, destination)
                .admits(writer, |peer| neighborhood.holds(peer))
        })
    }

    /// Keep `hold`, which this node wrote and which took no effect (`is_unplaced_hold`), to be
    /// sent again by the next repair pass. A hold kept already is kept once; at
    /// [`REFUSED_RELAY_HOLDS_MAX`] the oldest is dropped, logged at `warn`.
    pub(crate) fn keep_refused_relay_hold(&self, hold: EntryOperation) -> Result<()> {
        let mut kept = self
            .refused_relay_holds
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        if kept.contains(&hold) {
            return Ok(());
        }
        if kept.len() >= REFUSED_RELAY_HOLDS_MAX {
            kept.pop_front();
            tracing::warn!(local = %self.did, "relay hold dropped: too many holds await a holder");
        }
        kept.push_back(hold);
        Ok(())
    }

    /// Take every kept hold, to send again.
    pub(crate) fn take_refused_relay_holds(&self) -> Result<Vec<EntryOperation>> {
        Ok(self
            .refused_relay_holds
            .lock()
            .map_err(|_| Error::LockPoisoned)?
            .drain(..)
            .collect())
    }

    /// Whether this node takes a relay carrier relocated by `sender` (`relocates_from`).
    pub(crate) fn relocates_relay_from(&self, sender: Did) -> Result<bool> {
        Ok(self.lock_relay_neighborhood()?.relocates_from(sender))
    }
}
