//! The store reports a node awaits for the reported operations it sent (#866): per operation,
//! by its digest, the placements whose owners have not reported yet, the quorum their reports
//! must meet, and the receipt its caller holds.
//!
//! ```text
//!   expect(d, P, q)   pending[d] ← (P, q), and a receipt for d   (P = ∅: resolved Ok at once)
//!   report(d, p, o)   P ← P − {p};  o decides q ∨ P = ∅ ⟹ the receipt resolves o, d leaves
//!                     (o = Stored: Ok;  o = Refused: Refused(p))
//!   drop(receipt)     d leaves at the next expect that finds the table full
//!
//!   o decides q   Stored  Refused   Ok ⟺           resolved at
//!   q = Every       ·        ✓      ∀p. stored(p)   the first refusal, or the last store
//!   q = Any         ✓        ·      ∃p. stored(p)   the first store, or the last refusal
//! ```
//!
//! Law (duality): `Any` is `Every` with the two outcomes exchanged, `∃p. s(p) = ¬∀p. ¬s(p)`, so
//! each receipt resolves at the first report that decides its quorum and with that report's
//! outcome, or with the last report's when none decides it.
//!
//! Law (bounded): at most [`MAX_PENDING_STORE_REPORTS`] operations are pending. An `expect`
//! that finds the table full first drops the operations whose receipts are gone, and if it is
//! still full it tracks nothing: the new receipt resolves `Untracked` at once. The operation is
//! sent all the same; only its signal is lost.
//!
//! Law (ignore the unknown): a report of an operation or a placement not pending changes
//! nothing, so a duplicate report, a late one or one from a placement never asked is dropped.
//! A report is an observation (the one-report law of `message::types::store_report`): a lying
//! owner shifts only this receipt, never a reader's path.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::task::Context;
use std::task::Poll;

use futures::channel::oneshot;

use crate::dht::Did;
use crate::message::OperationDigest;
use crate::message::StoreOutcome;

/// The most reported operations a node awaits at once (the bounded law).
pub(crate) const MAX_PENDING_STORE_REPORTS: usize = 1024;

/// The reports that resolve a [`StoreReceipt`] stored, over the remote placements it awaits
/// (the module laws).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreQuorum {
    /// Every placement reported the operation stored; the first refusal resolves it refused.
    Every,
    /// One placement reported the operation stored; it resolves refused once every placement
    /// refused it.
    Any,
}

impl StoreQuorum {
    /// Whether one report of `outcome` decides a receipt under this quorum: a refusal decides
    /// [`Self::Every`], a store [`Self::Any`].
    pub(crate) const fn decided_by(self, outcome: StoreOutcome) -> bool {
        matches!(
            (self, outcome),
            (Self::Every, StoreOutcome::Refused) | (Self::Any, StoreOutcome::Stored)
        )
    }
}

/// Why a [`StoreReceipt`] resolved without its quorum reporting the operation stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreUnconfirmed {
    /// The owner of `placement` refused the operation: under [`StoreQuorum::Every`] the first
    /// refusal, under [`StoreQuorum::Any`] the last, every placement having refused.
    Refused {
        /// The placement refused.
        placement: Did,
    },
    /// The node was awaiting as many reported operations as it may, so it tracked none for
    /// this one (the bounded law).
    Untracked,
    /// The node stopped awaiting the operation before its quorum reported.
    Abandoned,
}

/// Resolves once the reports of a reported operation's remote placements meet its
/// [`StoreQuorum`], or with why they will not (the module laws). A report that is lost leaves
/// it pending; a caller bounds the wait or publishes again.
#[derive(Debug)]
pub struct StoreReceipt(oneshot::Receiver<Result<(), StoreUnconfirmed>>);

impl StoreReceipt {
    /// The receipt of an operation with no remote placement to await: stored once sent.
    pub(crate) fn stored() -> Self {
        let (done, receipt) = oneshot::channel();
        let _ = done.send(Ok(()));
        Self(receipt)
    }
}

impl Future for StoreReceipt {
    type Output = Result<(), StoreUnconfirmed>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|outcome| outcome.unwrap_or(Err(StoreUnconfirmed::Abandoned)))
    }
}

/// One operation awaited: the placements not reported yet, the quorum their reports must meet,
/// and its receipt's sender.
#[derive(Debug)]
struct Pending {
    awaiting: BTreeSet<Did>,
    quorum: StoreQuorum,
    done: oneshot::Sender<Result<(), StoreUnconfirmed>>,
}

/// The reported operations a node awaits (module laws).
#[derive(Debug, Default)]
pub(crate) struct StoreReports {
    pending: Mutex<BTreeMap<OperationDigest, Pending>>,
}

impl StoreReports {
    /// The pending operations, for one pure step. A poisoned lock still guards a well-formed
    /// table: no step panics between two writes.
    fn pending(&self) -> MutexGuard<'_, BTreeMap<OperationDigest, Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Await the reports of `operation` from the owners of `awaiting`, its remote placements:
    /// the receipt resolves once their reports decide `quorum` (the module laws). Called before
    /// the operation is sent, so no report can precede it.
    pub(crate) fn expect(
        &self,
        operation: OperationDigest,
        awaiting: BTreeSet<Did>,
        quorum: StoreQuorum,
    ) -> StoreReceipt {
        if awaiting.is_empty() {
            return StoreReceipt::stored();
        }
        let (done, receipt) = oneshot::channel();
        let refused = {
            let mut pending = self.pending();
            if pending.len() >= MAX_PENDING_STORE_REPORTS {
                pending.retain(|_, awaited| !awaited.done.is_canceled());
            }
            if pending.len() >= MAX_PENDING_STORE_REPORTS && !pending.contains_key(&operation) {
                Some(done)
            } else {
                pending.insert(operation, Pending {
                    awaiting,
                    quorum,
                    done,
                });
                None
            }
        };
        if let Some(done) = refused {
            let _ = done.send(Err(StoreUnconfirmed::Untracked));
        }
        StoreReceipt(receipt)
    }

    /// Take the report of the owner of `placement` that it did `outcome` with `operation`.
    pub(crate) fn report(&self, operation: OperationDigest, placement: Did, outcome: StoreOutcome) {
        let resolved = {
            let mut pending = self.pending();
            let Some(awaited) = pending.get_mut(&operation) else {
                return;
            };
            if !awaited.awaiting.remove(&placement) {
                return;
            }
            let decided = awaited.quorum.decided_by(outcome) || awaited.awaiting.is_empty();
            let resolution = match outcome {
                StoreOutcome::Stored => Ok(()),
                StoreOutcome::Refused => Err(StoreUnconfirmed::Refused { placement }),
            };
            decided.then_some(resolution).and_then(|resolution| {
                pending
                    .remove(&operation)
                    .map(|awaited| (awaited.done, resolution))
            })
        };
        if let Some((done, resolution)) = resolved {
            let _ = done.send(resolution);
        }
    }

    /// How many operations are pending.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.pending().len()
    }
}

#[cfg(test)]
mod test_store_reports;
