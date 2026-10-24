//! Relay custody: the relay-inbox deliveries a receiver admitted and the application has not
//! taken yet (#936; durable-records, Law 9).
//!
//! A relay element `e = (s, q, h)` carries a transaction of stream `s` at sequence `q` with
//! digest `h`. Admitting it enters `h` into the window `W_s`; the queued message lives only in the
//! volatile application inbox. Without more state, a restart between the admission and the take
//! loses the queue but not `W_s`, so the redelivery observes `Replay` and is retired: the mail is
//! lost. The custody set `K_s` keeps that fact durable, inside the receiver record of `s`, so one
//! record write commits `W_s` and `K_s` together:
//!
//! ```text
//! pass start:  K_s ← K_s ∩ stored(d)                            (written only if it changed)
//! offer e:     (q, h) ∈ K_s              ▶ Requeue: queue again, W_s untouched
//!              |⋃ K| = RELAY_INBOX_MAX_LEN ▶ Defer: e and later elements wait
//!              observe(W_s, q, h) admits ▶ commit (W_s′, K_s ∪ {(q, h)}), then queue
//!              otherwise                  ▶ Retire, as the live path would drop h
//! take:        commit (W_s, K_s ∖ {(q, h)})  (the release), then e may be retired
//! ```
//!
//! **Law (at least once across restart).** With the carrier durable (#1021), an element leaves
//! the carrier only once the application took it, once the live path would drop its transaction
//! too, or once its retention bound elapsed: a crash cannot separate an admission from its
//! custody entry, nor a release from its effect, since each is one record write.
//!
//! **Law (duplicate window).** The application receives a message of one element twice only if
//! a crash falls after its take and before its release committed; the release is issued on the
//! take's signal, so the window is one record write. Both copies carry the same transaction id,
//! which is the application's key to deduplicate by.
//!
//! **Law (bounded custody).** `|⋃_s K_s| ≤ RELAY_INBOX_MAX_LEN`: the defer arm refuses a new entry
//! at the bound, and the restriction keeps only entries of elements still stored.
//!
//! The live path observes `W_s` alone and is unchanged. Mail below the window is refused as the
//! live path refuses it: no bounded receiver state can tell it from a re-hold of a transaction
//! already dispatched live.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use super::receiver_record;
use super::ReplayTable;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use crate::consts::RELAY_INBOX_MAX_LEN;
use crate::error::Error;
use crate::error::Result;
use crate::message::OriginQuotaCharge;

/// Most relay deliveries in custody over the whole store: one per element a relay inbox holds.
pub(crate) const RELAY_CUSTODY_CAPACITY: usize = RELAY_INBOX_MAX_LEN;

/// One relay delivery in custody: the sequence and digest of the transaction it admitted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct RelayDelivery {
    /// The transaction's sequence in its stream.
    pub(crate) sequence: u64,
    /// The transaction's digest.
    pub(crate) digest: TransactionDigest,
}

/// The relay deliveries of one receiver stream in custody.
pub(crate) type RelayCustody = BTreeSet<RelayDelivery>;

/// What a relay admission did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelayAdmission {
    /// The transaction entered the window and its delivery entered custody.
    Admitted,
    /// The delivery was already in custody (admitted before a restart, not yet taken): queue it
    /// again, admitting nothing.
    InCustody,
}

impl TransactionReplay {
    /// Admit the relay delivery `delivery` of stream `key` (the custody law): its transaction
    /// enters the window and the delivery enters custody in one record write, unless the
    /// delivery is already in custody.
    ///
    /// Post: `Err(RelayCustodyFull)` at the custody bound, changing nothing; the window's
    /// refusals (`Replay`, `Fork`, `Stale`) and quota refusals as for a live admission.
    pub(crate) async fn admit_relayed(
        self: &std::sync::Arc<Self>,
        key: StreamKey,
        delivery: RelayDelivery,
        charge: OriginQuotaCharge,
        byte_cost: usize,
    ) -> Result<RelayAdmission> {
        let clock = super::quota_clock(self.started_at);
        let replay = std::sync::Arc::clone(self);
        super::run_whole(async move {
            let mut state = replay.state.lock().await;
            let store = replay.load_store(&mut state.store).await?;
            replay.refuse_unavailable(store, ReplayTable::Receiver, key)?;
            if store
                .tables
                .custody
                .get(&key)
                .is_some_and(|custody| custody.contains(&delivery))
            {
                return Ok(RelayAdmission::InCustody);
            }
            let held = store
                .tables
                .custody
                .values()
                .map(BTreeSet::len)
                .sum::<usize>();
            if held >= RELAY_CUSTODY_CAPACITY {
                return Err(Error::RelayCustodyFull {
                    capacity: RELAY_CUSTODY_CAPACITY,
                });
            }
            let admission = super::Admission {
                key,
                sequence: delivery.sequence,
                digest: delivery.digest,
                charge,
                byte_cost,
            };
            replay
                .admit_in(&mut state, admission, clock, Some(delivery))
                .await
                .map(|_| RelayAdmission::Admitted)
        })
        .await
    }

    /// Release the relay delivery `delivery` of stream `key` from custody (the take): one record
    /// write, or none when it is not in custody.
    pub(crate) async fn release_relayed(
        self: &std::sync::Arc<Self>,
        key: StreamKey,
        delivery: RelayDelivery,
    ) -> Result<()> {
        let replay = std::sync::Arc::clone(self);
        super::run_whole(async move {
            let mut state = replay.state.lock().await;
            let store = replay.load_store(&mut state.store).await?;
            let Some(custody) = store.tables.custody.get(&key) else {
                return Ok(());
            };
            if !custody.contains(&delivery) {
                return Ok(());
            }
            let mut released = custody.clone();
            released.remove(&delivery);
            replay.rewrite_custody(store, key, released).await
        })
        .await
    }

    /// Restrict custody to the deliveries of `stored`, the relay elements still in the carrier
    /// (the restriction step): one record write per stream whose custody changed.
    pub(crate) async fn restrict_custody(
        self: &std::sync::Arc<Self>,
        stored: BTreeSet<(StreamKey, RelayDelivery)>,
    ) -> Result<()> {
        let replay = std::sync::Arc::clone(self);
        super::run_whole(async move {
            let mut state = replay.state.lock().await;
            let store = replay.load_store(&mut state.store).await?;
            let shrunk = store
                .tables
                .custody
                .iter()
                .filter_map(|(key, custody)| {
                    let kept = custody
                        .iter()
                        .filter(|delivery| stored.contains(&(*key, **delivery)))
                        .copied()
                        .collect::<RelayCustody>();
                    (kept.len() < custody.len()).then_some((*key, kept))
                })
                .collect::<Vec<_>>();
            for (key, kept) in shrunk {
                replay.rewrite_custody(store, key, kept).await?;
            }
            Ok(())
        })
        .await
    }

    /// Persist stream `key`'s receiver record with `custody`, then adopt it.
    ///
    /// Pre: `key` is a restored receiver stream of `store` (custody exists only beside a
    /// window).
    async fn rewrite_custody(
        &self,
        store: &mut super::ReplayStore,
        key: StreamKey,
        custody: RelayCustody,
    ) -> Result<()> {
        let window = store
            .tables
            .receiver
            .get(&key)
            .ok_or(Error::TransactionReplayStateInvalid)?;
        self.persist(receiver_record(&key, window, &custody)?)
            .await?;
        if custody.is_empty() {
            store.tables.custody.remove(&key);
        } else {
            store.tables.custody.insert(key, custody);
        }
        Ok(())
    }
}
