//! Group commit of replay transitions (#916; durable-records, Law Group commit).
//!
//! Every transition still runs whole under the one replay lock, but on the *speculative*
//! tables: it applies its effect at once, logging an undo entry, stages its record into the
//! open batch, and is released only once that batch is durable. One flusher at a time writes
//! the batches, in order, each by one [`put_batch`](crate::storage::KvStorageScan::put_batch),
//! outside the lock, while later transitions gather into the next batch:
//!
//! ```text
//! t: [lock ─▶ load ─▶ body(T̂) ─▶ T̂ := T̂ ∘ t, undo += u_t, open += (w_t, waiter)] ─▶ await
//!          └─ no flush in flight ─▶ spawn flusher
//! flusher: loop [lock ─▶ in_flight := open, open := ∅] ─▶ put_batch(W) ─▶
//!               [lock ─▶ Ok: drop undo, release Ok │ Err: undo open, then in_flight; release Err]
//!          until the open batch is empty
//! ```
//!
//! **Law (group commit).** With the batches `B₁, B₂, …` intervals of lock order:
//!
//! - *Release:* a transition's result, a refusal or a transition that writes nothing included,
//!   is returned only after the batch it joined is durable; batches complete in order, so a
//!   result never rests on a state that is not durable.
//! - *Atomicity per record:* each record is whole at its previous or its new value after any
//!   crash (the batch law of the store); nothing is promised across records.
//! - *Order:* a batch writes the last record each stream staged in it, and batches are written
//!   one at a time in lock order, so each stream's durable values follow lock order.
//! - *Fail closed:* a failed batch fails every pending transition (its own and the open batch,
//!   which ran on top of it) and undoes them all, last first, so the speculative tables are the
//!   tables before the failed batch and no member is adopted. A failed batch may still have
//!   landed some records (atomicity is per record), so memory ⊑ disk: every member that is
//!   durable but undone is a lost release (below).
//! - *Latency:* a transition waits for at most the batch in flight and then its own: two
//!   batch writes.
//! - *Size:* a batch writes one record per stream, so its records and bytes stay within the
//!   store's own bounds ([`TRANSACTION_REPLAY_STORE_MAX_RECORDS`],
//!   [`TRANSACTION_REPLAY_STORE_MAX_BYTES`]); its
//!   members are the transitions in flight, one waiter and its undo entries each, the same
//!   transitions that queued on the lock before group commit. No cap is set: the callers own
//!   the transitions, and a cap would move their wait, not bound it.
//!
//! **Lost release.** A member whose record is durable but that was never released (a crash
//! between a batch's write and its release, or a failed batch that landed the record) committed
//! as if its caller had dropped the result, which the law of whole transitions already admits:
//!
//! - an admission is in its window and was never dispatched; a later write of the stream from
//!   the undone window drops it again, and a re-admission of the digest writes an equal or
//!   larger window;
//! - a lease extension covers sequences nobody was returned, so `issued ≤ leased ≤ D` holds
//!   with `D` the last write (the lease law of `lease`, which reads `returned ≤ D`);
//! - a custody entry names a delivery still in the carrier, which the next pass requeues
//!   (`InCustody`) or the restriction step drops; a landed release only retires early what the
//!   application already took (the duplicate-window law of `custody`). A window and its custody
//!   stay one record, so the custody law holds unchanged.
//!
//! [`TRANSACTION_REPLAY_STORE_MAX_RECORDS`]: super::TRANSACTION_REPLAY_STORE_MAX_RECORDS
//! [`TRANSACTION_REPLAY_STORE_MAX_BYTES`]: super::TRANSACTION_REPLAY_STORE_MAX_BYTES

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::channel::oneshot;

use super::custody::RelayCustody;
use super::lease::SenderLease;
use super::store::ReplayStore;
use super::ReplayRecord;
use super::SequenceState;
use super::StreamKey;
use super::TransactionReplay;
use crate::error::Error;
use crate::error::Result;
use crate::message::quota::OriginQuotaReservation;
use crate::message::quota::OriginQuotaTable;

/// The inverse of one effect a transition applied to the speculative state.
enum Undo {
    /// Restore a sender stream's allocator (`None`: the stream was new).
    Sender(StreamKey, Option<SenderLease>),
    /// Restore a receiver stream's window (`None`: the stream was new), boxed so the other
    /// entries stay small.
    Receiver(StreamKey, Option<Box<SequenceState>>),
    /// Restore a receiver stream's custody (`None`: it held none).
    Custody(StreamKey, Option<RelayCustody>),
    /// Roll back an origin-quota reservation.
    Quota(OriginQuotaReservation),
}

impl Undo {
    /// Undo the effect on `store` and `quota`.
    fn apply(self, store: &mut ReplayStore, quota: &mut OriginQuotaTable) {
        match self {
            Self::Sender(key, previous) => restore(&mut store.tables.sender, key, previous),
            Self::Receiver(key, previous) => restore(
                &mut store.tables.receiver,
                key,
                previous.map(|window| *window),
            ),
            Self::Custody(key, previous) => restore(&mut store.tables.custody, key, previous),
            Self::Quota(reservation) => reservation.rollback(quota),
        }
    }
}

/// Put `previous` back under `key` in `table`, or remove `key` when there was none.
fn restore<V>(table: &mut BTreeMap<StreamKey, V>, key: StreamKey, previous: Option<V>) {
    match previous {
        Some(previous) => {
            table.insert(key, previous);
        }
        None => {
            table.remove(&key);
        }
    }
}

/// Undo `log` on `store` and `quota`, last entry first.
fn rewind(log: Vec<Undo>, store: &mut ReplayStore, quota: &mut OriginQuotaTable) {
    for undo in log.into_iter().rev() {
        undo.apply(store, quota);
    }
}

/// The effect one transition's body applies to the speculative state: each change logged with
/// its inverse, and the records to write.
pub(super) struct Ledger<'a> {
    /// The speculative tables.
    pub(super) store: &'a mut ReplayStore,
    /// The runtime-local origin quotas.
    pub(super) quota: &'a mut OriginQuotaTable,
    /// The inverses of the changes made, in order.
    undo: Vec<Undo>,
    /// The records to write, by storage key; a later record of one key supersedes an earlier.
    records: BTreeMap<String, ReplayRecord>,
}

impl<'a> Ledger<'a> {
    /// A ledger over the speculative `store` and `quota` with nothing applied yet.
    pub(super) fn new(store: &'a mut ReplayStore, quota: &'a mut OriginQuotaTable) -> Self {
        Self {
            store,
            quota,
            undo: Vec::new(),
            records: BTreeMap::new(),
        }
    }

    /// Set sender stream `key`'s allocator to `lease`.
    pub(super) fn set_sender(&mut self, key: StreamKey, lease: SenderLease) {
        let previous = self.store.tables.sender.insert(key, lease);
        self.undo.push(Undo::Sender(key, previous));
    }

    /// Set receiver stream `key`'s window to `window`.
    pub(super) fn set_receiver(&mut self, key: StreamKey, window: SequenceState) {
        let previous = self.store.tables.receiver.insert(key, window);
        self.undo.push(Undo::Receiver(key, previous.map(Box::new)));
    }

    /// Set receiver stream `key`'s custody to `custody`; an empty custody is held as none.
    pub(super) fn set_custody(&mut self, key: StreamKey, custody: RelayCustody) {
        let previous = match custody.is_empty() {
            true => self.store.tables.custody.remove(&key),
            false => self.store.tables.custody.insert(key, custody),
        };
        self.undo.push(Undo::Custody(key, previous));
    }

    /// Log the quota `reservation` already applied to `quota`.
    pub(super) fn charged(&mut self, reservation: OriginQuotaReservation) {
        self.undo.push(Undo::Quota(reservation));
    }

    /// Stage the write of `record` under `storage_key`.
    pub(super) fn write(&mut self, (storage_key, record): (String, ReplayRecord)) {
        self.records.insert(storage_key, record);
    }

    /// Undo every change made so far, last first, and drop the staged records: the effect of
    /// a body that refused.
    fn abandon(&mut self) {
        rewind(std::mem::take(&mut self.undo), self.store, self.quota);
        self.records.clear();
    }
}

/// A transition's wait for its batch: `Ok` once the batch is durable, the batch's failure
/// otherwise.
type Release = oneshot::Receiver<Result<()>>;

/// One batch: the members' undo entries in lock order, the records to write, and the waiters
/// to release.
#[derive(Default)]
struct Batch {
    /// The undo entries of every member, in lock order.
    undo: Vec<Undo>,
    /// The records, by storage key, each the last its stream staged.
    records: BTreeMap<String, ReplayRecord>,
    /// One waiter per member.
    waiters: Vec<oneshot::Sender<Result<()>>>,
}

impl Batch {
    /// Release every member with `outcome`, the batch's failure cloned per member.
    fn release(self, outcome: &std::result::Result<(), Arc<Error>>) {
        for waiter in self.waiters {
            // A waiter is gone only when its transition was abandoned; its effect stands.
            let _ = waiter.send(outcome.clone().map_err(persistence_failure));
        }
    }
}

/// The error each member of a failed batch returns: the store's failure, shared.
fn persistence_failure(source: Arc<Error>) -> Error {
    Error::TransactionReplayPersistence {
        operation: "store",
        source,
    }
}

/// The batches of the replay runtime: the open one, gathering transitions, and the undo log of
/// the one being written, `Some` exactly while a flusher runs.
#[derive(Default)]
pub(super) struct Journal {
    /// The batch transitions join.
    open: Batch,
    /// The undo log of the batch in flight; `Some` iff a flusher runs.
    in_flight: Option<Vec<Undo>>,
}

impl Journal {
    /// Join the open batch with the effect `ledger` holds, and return the member's wait and
    /// whether the caller must start a flusher (none runs).
    fn join(&mut self, ledger: Ledger<'_>) -> (Release, bool) {
        let (waiter, release) = oneshot::channel();
        self.open.undo.extend(ledger.undo);
        self.open.records.extend(ledger.records);
        self.open.waiters.push(waiter);
        let start = self.in_flight.is_none();
        if start {
            self.in_flight = Some(Vec::new());
        }
        #[cfg(test)]
        crate::tests::activity::record_activity();
        (release, start)
    }

    /// Log the quota `reservation` of a transition released at once (a link-local admission,
    /// #1081): into the open batch while a batch is pending, so that a failed batch rewinds the
    /// quota table last first across both; dropped otherwise, since no pending batch lies
    /// beneath it. The released transition is not failed with the batch: its verdict rests on
    /// no durable state, and only its charge is refunded.
    pub(super) fn log_released_charge(&mut self, reservation: OriginQuotaReservation) {
        if self.in_flight.is_some() || !self.open.waiters.is_empty() {
            self.open.undo.push(Undo::Quota(reservation));
        }
    }

    /// The transitions waiting in the open batch.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) fn open_members(&self) -> usize {
        self.open.waiters.len()
    }

    /// Take the open batch for writing: its records and waiters, its undo log kept in flight;
    /// `None`, ending the flusher, when no transition joined it.
    fn take(&mut self) -> Option<(BTreeMap<String, ReplayRecord>, Batch)> {
        if self.open.waiters.is_empty() {
            // Only released charges can be logged here, and no batch lies beneath them now.
            self.open.undo.clear();
            self.in_flight = None;
            return None;
        }
        let mut batch = std::mem::take(&mut self.open);
        self.in_flight = Some(std::mem::take(&mut batch.undo));
        Some((std::mem::take(&mut batch.records), batch))
    }

    /// Settle the batch in flight: drop its undo log once durable; on a failure, undo the open
    /// batch and then the batch in flight (the fail-closed law) and return the open batch, whose
    /// members fail with it.
    fn settle(
        &mut self,
        durable: bool,
        store: &mut ReplayStore,
        quota: &mut OriginQuotaTable,
    ) -> Option<Batch> {
        let in_flight = self.in_flight.replace(Vec::new()).unwrap_or_default();
        if durable {
            return None;
        }
        let mut open = std::mem::take(&mut self.open);
        rewind(std::mem::take(&mut open.undo), store, quota);
        rewind(in_flight, store, quota);
        Some(open)
    }
}

impl TransactionReplay {
    /// Run `body` as one transition under the group-commit law (module docs): lock, load, apply
    /// `body` to the speculative state through a [`Ledger`], join the open batch, then wait for
    /// it. A body that refuses has its changes undone at once and still waits, so its refusal is
    /// released only once every state it read is durable.
    ///
    /// The transition runs detached from the caller (the law of whole transitions): cancelling
    /// the caller abandons only the wait.
    pub(super) async fn commit<T, F>(self: &Arc<Self>, body: F) -> Result<T>
    where
        T: rings_runtime::MaybeSend + 'static,
        F: FnOnce(&Self, &mut Ledger<'_>) -> Result<T> + rings_runtime::MaybeSend + 'static,
    {
        let replay = Arc::clone(self);
        super::run_whole(async move {
            let (outcome, release, start) = {
                let mut state = replay.state.lock().await;
                let state = &mut *state;
                let store = replay.load_store(&mut state.store).await?;
                let mut ledger = Ledger::new(store, &mut state.quota);
                let outcome = body(&replay, &mut ledger);
                if outcome.is_err() {
                    ledger.abandon();
                }
                let (release, start) = state.journal.join(ledger);
                (outcome, release, start)
            };
            if start {
                replay.start_flusher().await;
            }
            release.await.map_err(|_| {
                Error::TransactionReplayUnscheduled(rings_runtime::Abandoned.into())
            })??;
            outcome
        })
        .await
    }

    /// Start the flusher detached; with no runtime current, run it inline.
    async fn start_flusher(self: &Arc<Self>) {
        let flusher = Arc::clone(self).flush();
        if let Err(unscheduled) = rings_runtime::spawn_detached(flusher) {
            unscheduled.into_inner().await;
        }
    }

    /// Write the batches one at a time, in order, until the open batch is empty (module docs).
    async fn flush(self: Arc<Self>) {
        loop {
            let Some((records, batch)) = self.state.lock().await.journal.take() else {
                return;
            };
            let written = match records.is_empty() {
                true => Ok(()),
                false => self.write_batch(records).await,
            };
            let failed = {
                let mut state = self.state.lock().await;
                let state = &mut *state;
                match state.store.as_mut() {
                    Some(store) => state
                        .journal
                        .settle(written.is_ok(), store, &mut state.quota),
                    None => None,
                }
            };
            let outcome = written.map_err(Arc::new);
            batch.release(&outcome);
            if let Some(open) = failed {
                open.release(&outcome);
            }
        }
    }

    /// Write `records` by one batch of the store; a failure is counted once.
    async fn write_batch(&self, records: BTreeMap<String, ReplayRecord>) -> Result<()> {
        let records = records.into_iter().collect::<Vec<_>>();
        self.storage()?
            .put_batch(&records)
            .await
            .inspect_err(|_| self.count_persistence_failure())
    }
}
