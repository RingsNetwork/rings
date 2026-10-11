//! Successor lookups the application issued, answered through the handle it holds.
//!
//! ```text
//!   Swarm::lookup_successor ─ register(tx) ─▶ pending[tx] ─ send ─▶ … ─▶ FindSuccessorReport(tx)
//!                                              │                                 │
//!   SuccessorReport (app awaits) ◀─ oneshot ───┘◀──────────── answer(tx, s) ─────┘
//! ```
//!
//! Laws:
//! - A transaction is registered before its request is sent, so its report always finds it.
//! - `pending` holds exactly the transactions whose [`SuccessorReport`] is alive: dropping the
//!   handle removes its entry. A report for any other transaction is unsolicited and dropped by
//!   core.
//! - `answer` resolves a transaction at most once.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::Weak;
use std::task::Context;
use std::task::Poll;

use futures::channel::oneshot;

use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

/// The application's outstanding successor lookups, by transaction.
#[derive(Default)]
pub(crate) struct ApplicationLookups {
    pending: Mutex<HashMap<uuid::Uuid, oneshot::Sender<Did>>>,
}

impl ApplicationLookups {
    /// The table; every critical section leaves it consistent, so a poisoned lock is still valid.
    fn lock(&self) -> MutexGuard<'_, HashMap<uuid::Uuid, oneshot::Sender<Did>>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register `tx_id` and return the handle its report resolves.
    pub(crate) fn register(self: &Arc<Self>, tx_id: uuid::Uuid) -> SuccessorReport {
        let (sender, receiver) = oneshot::channel();
        self.lock().insert(tx_id, sender);
        SuccessorReport {
            tx_id,
            receiver,
            lookups: Arc::downgrade(self),
        }
    }

    /// Resolve `tx_id` with `successor`. Returns whether an application lookup awaited it.
    pub(crate) fn answer(&self, tx_id: uuid::Uuid, successor: Did) -> bool {
        let Some(sender) = self.lock().remove(&tx_id) else {
            return false;
        };
        sender.send(successor).is_ok()
    }

    /// The number of outstanding lookups.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }
}

/// The report of one routed successor lookup: resolves to the reported successor.
///
/// The application bounds the wait itself; dropping the handle withdraws the lookup, and a
/// report arriving later is dropped by core.
pub struct SuccessorReport {
    tx_id: uuid::Uuid,
    receiver: oneshot::Receiver<Did>,
    lookups: Weak<ApplicationLookups>,
}

impl SuccessorReport {
    /// The transaction the lookup was routed under.
    pub fn tx_id(&self) -> uuid::Uuid {
        self.tx_id
    }
}

impl std::fmt::Debug for SuccessorReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuccessorReport")
            .field("tx_id", &self.tx_id)
            .finish_non_exhaustive()
    }
}

impl Future for SuccessorReport {
    type Output = Result<Did>;

    /// Ready with the reported successor, or with `SwarmStopped` once the swarm is gone.
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.receiver)
            .poll(cx)
            .map(|answer| answer.map_err(|_| Error::SwarmStopped))
    }
}

impl Drop for SuccessorReport {
    fn drop(&mut self) {
        if let Some(lookups) = self.lookups.upgrade() {
            lookups.lock().remove(&self.tx_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::FutureExt;

    use super::ApplicationLookups;
    use crate::ecc::SecretKey;

    #[test]
    fn test_a_report_resolves_its_registered_lookup_once() {
        let lookups = Arc::new(ApplicationLookups::default());
        let tx_id = uuid::Uuid::new_v4();
        let successor = SecretKey::random().address().into();
        let report = lookups.register(tx_id);
        assert!(lookups.answer(tx_id, successor));
        assert!(!lookups.answer(tx_id, successor));
        assert_eq!(report.now_or_never().map(|r| r.ok()), Some(Some(successor)));
    }

    #[test]
    fn test_a_dropped_handle_withdraws_its_lookup_and_its_report_is_unsolicited() {
        let lookups = Arc::new(ApplicationLookups::default());
        let tx_id = uuid::Uuid::new_v4();
        drop(lookups.register(tx_id));
        assert_eq!(lookups.len(), 0);
        assert!(!lookups.answer(tx_id, SecretKey::random().address().into()));
    }
}
