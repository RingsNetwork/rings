//! Relay custody: the relay-inbox deliveries a receiver admitted and the application has not
//! taken yet (#936; durable-records, Law 9).
//!
//! A relay element `e = (s, q, h)` carries a transaction of stream `s` at sequence `q` with
//! digest `h`. Admitting it enters `h` into the window `W_s`; the queued message lives only in the
//! volatile application inbox. Without more state, a restart between the admission and the take
//! loses the queue but not `W_s`, so the redelivery observes `Replay` and is retired: the mail is
//! lost. The custody set `K_s` keeps that fact durable, inside the receiver record of `s`, so one
//! record write commits `W_s` and `K_s` together. Each entry carries the instant `x` its
//! transaction is spent ([`spent_at`](super::spent_at)), after which no carrier delivers it:
//!
//! ```text
//! transition:  K_s ← { (q, h, x) ∈ K_s | now < x }               (written only if it changed)
//! offer e:     (q, h, x) ∈ K_s           ▶ Requeue: queue again, W_s untouched
//!              |⋃ K| = RELAY_INBOX_MAX_LEN ▶ Defer: e and later elements wait
//!              observe(W_s, q, h) admits ▶ commit (W_s′, K_s ∪ {(q, h, x)}), then queue
//!              otherwise                  ▶ Retire, as the live path would drop h
//! take:        commit (W_s, K_s ∖ {(q, h, x)})  (the release), then e may be retired
//! ```
//!
//! **Law (removal only by evidence).** An entry leaves `K_s` only by its release, which follows
//! the take, or by its expiry, after which no carrier can offer its element (Lemma 2): never by
//! its absence from a carrier report. A report is a holder's view, which may lag behind a
//! membership change or a hand-off not yet landed, so absence is no evidence of removal; an
//! entry restricted by it would turn a redelivery of untaken mail into a `Replay` (#1059).
//!
//! **Law (at least once across restart).** With the carrier durable (#1021), an element leaves
//! the carrier only once the application took it, once the live path would drop its transaction
//! too, or once its retention bound elapsed: a crash cannot separate an admission from its
//! custody entry, nor a release from its effect, since each is one record write, and its entry
//! stays until the release or until no carrier can offer the element.
//!
//! **Law (duplicate window).** The application receives a message of one element twice only if
//! a crash falls after its take and before its release committed; the release is issued on the
//! take's signal, so the window is one record write. Both copies carry the same transaction id,
//! which is the application's key to deduplicate by.
//!
//! **Law (bounded custody).** `|⋃_s K_s| ≤ RELAY_INBOX_MAX_LEN`: the defer arm refuses a new entry
//! at the bound, after the transition expired the spent entries. An element lost without a take
//! (a holder that dropped it) holds its entry at most until it is spent.
//!
//! The live path observes `W_s` alone and is unchanged. Mail below the window is refused as the
//! live path refuses it: no bounded receiver state can tell it from a re-hold of a transaction
//! already dispatched live.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use super::commit::Ledger;
use super::Admission;
use super::ReplayTable;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use crate::consts::RELAY_INBOX_MAX_LEN;
use crate::error::Error;
use crate::error::Result;

/// Most relay deliveries in custody over the whole store: one per element a relay inbox holds.
pub(crate) const RELAY_CUSTODY_CAPACITY: usize = RELAY_INBOX_MAX_LEN;

/// One relay delivery in custody: the sequence and digest of the transaction it admitted, and
/// the instant that transaction is spent.
///
/// Law: every field is a function of the transaction, so its admission and its release name the
/// same delivery.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct RelayDelivery {
    /// The transaction's sequence in its stream.
    pub(crate) sequence: u64,
    /// The transaction's digest.
    pub(crate) digest: TransactionDigest,
    /// The instant the transaction is spent ([`spent_at`](super::spent_at)): from then on no
    /// carrier delivers it, so its entry may expire.
    pub(crate) spent_at_ms: u128,
}

impl RelayDelivery {
    /// Whether this delivery is spent at `now_ms`, `now ≥ x` (pure).
    pub(super) const fn is_spent_at(&self, now_ms: u128) -> bool {
        now_ms >= self.spent_at_ms
    }
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
    /// Admit `admission` as a relay delivery (the custody law): its transaction enters the
    /// window and the delivery enters custody in one record write, unless the delivery is
    /// already in custody.
    ///
    /// Post: `Err(RelayCustodyFull)` at the custody bound, changing nothing; the window's
    /// refusals (`Replay`, `Fork`, `Stale`) and quota refusals as for a live admission.
    pub(crate) async fn admit_relayed(
        self: &std::sync::Arc<Self>,
        admission: Admission,
    ) -> Result<RelayAdmission> {
        let clock = super::replay_clock(self.started_at);
        let key = admission.key;
        let delivery = RelayDelivery {
            sequence: admission.sequence,
            digest: admission.digest,
            spent_at_ms: admission.spent_at_ms,
        };
        self.commit(move |replay, ledger| {
            let now = clock();
            ledger.retire_spent(now.wall_ms)?;
            replay.refuse_unavailable(ledger.store, ReplayTable::Receiver, key)?;
            let custody = &ledger.store.tables.custody;
            if custody
                .get(&key)
                .is_some_and(|custody| custody.contains(&delivery))
            {
                return Ok(RelayAdmission::InCustody);
            }
            let held = custody.values().map(BTreeSet::len).sum::<usize>();
            if held >= RELAY_CUSTODY_CAPACITY {
                return Err(Error::RelayCustodyFull {
                    capacity: RELAY_CUSTODY_CAPACITY,
                });
            }
            replay
                .admit_in(ledger, admission, move || now, Some(delivery))
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
        self.commit(move |_, ledger| {
            let Some(custody) = ledger.store.tables.custody.get(&key) else {
                return Ok(());
            };
            if !custody.contains(&delivery) {
                return Ok(());
            }
            let mut released = custody.clone();
            released.remove(&delivery);
            rewrite_custody(ledger, key, released)
        })
        .await
    }
}

/// Expire every relay delivery spent at `now_ms` from custody (the removal law): one staged
/// record per stream whose custody changed.
///
/// Post: no entry of custody is spent at `now_ms`; an entry not spent is kept.
pub(super) fn expire_custody(ledger: &mut Ledger<'_>, now_ms: u128) -> Result<()> {
    let expired = ledger
        .store
        .tables
        .custody
        .iter()
        .filter(|(_, custody)| custody.iter().any(|delivery| delivery.is_spent_at(now_ms)))
        .map(|(key, custody)| {
            let kept = custody
                .iter()
                .filter(|delivery| !delivery.is_spent_at(now_ms))
                .copied()
                .collect::<RelayCustody>();
            (*key, kept)
        })
        .collect::<Vec<_>>();
    expired
        .into_iter()
        .try_for_each(|(key, kept)| rewrite_custody(ledger, key, kept))
}

/// Stage stream `key`'s receiver record with `custody`, and adopt it on the speculative state.
///
/// Pre: `key` is a restored receiver stream of the store (custody exists only beside a
/// window).
fn rewrite_custody(ledger: &mut Ledger<'_>, key: StreamKey, custody: RelayCustody) -> Result<()> {
    ledger.set_custody(key, custody);
    ledger.write(ledger.store.tables.receiver_record(&key)?);
    Ok(())
}
