//! Relay mail handed to the application inbox and not yet retired from storage (#913 R10 H3).
//!
//! ```text
//!   drain pass:  Unknown ──deliver──▶ Pending(rx)
//!                Pending ──application takes it──▶ Consumed ──retire the stored element
//!                Pending ──inbox discards it────▶ Discarded (the element stays stored)
//! ```
//!
//! Law (at least once within a run): a stored element is retired only after the application
//! took its message from the inbox, so a later pass or a released inbox before that never
//! loses it; a restart in between still can, as the replay window refuses the redelivery
//! (#936).
//! The table holds only elements still stored: each pass keeps the entries of the elements it
//! saw, so it is bounded by the inbox.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use futures::channel::oneshot;

/// Where one stored relay element stands toward the application.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelayConsumption {
    /// Never handed over in this process.
    Unknown,
    /// Queued in the inbox, not yet taken.
    Pending,
    /// Taken by the application: the stored element may be retired.
    Consumed,
    /// Discarded by a released inbox before it was taken: it stays stored.
    Discarded,
}

/// One handed-over element's consumption signal, or the record that it was discarded.
enum InFlight {
    Queued(oneshot::Receiver<()>),
    Discarded,
}

/// The relay elements handed to the inbox, by transaction.
#[derive(Default)]
pub(crate) struct RelayInFlight(Mutex<HashMap<uuid::Uuid, InFlight>>);

impl RelayInFlight {
    /// The table; every critical section leaves it consistent, so a poisoned lock is still valid.
    fn lock(&self) -> MutexGuard<'_, HashMap<uuid::Uuid, InFlight>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where the element of `tx_id` stands; a consumed one leaves the table.
    pub(crate) fn poll(&self, tx_id: uuid::Uuid) -> RelayConsumption {
        let mut table = self.lock();
        let Some(entry) = table.get_mut(&tx_id) else {
            return RelayConsumption::Unknown;
        };
        let InFlight::Queued(taken) = entry else {
            return RelayConsumption::Discarded;
        };
        match taken.try_recv() {
            Ok(Some(())) => {
                table.remove(&tx_id);
                RelayConsumption::Consumed
            }
            Ok(None) => RelayConsumption::Pending,
            Err(oneshot::Canceled) => {
                *entry = InFlight::Discarded;
                RelayConsumption::Discarded
            }
        }
    }

    /// Record the element of `tx_id` as queued, with the signal its taking fires.
    pub(crate) fn queued(&self, tx_id: uuid::Uuid, taken: oneshot::Receiver<()>) {
        self.lock().insert(tx_id, InFlight::Queued(taken));
    }

    /// Forget every element not in `stored`, the elements still stored.
    pub(crate) fn retain(&self, stored: &BTreeSet<uuid::Uuid>) {
        self.lock().retain(|tx_id, _| stored.contains(tx_id));
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::oneshot;

    use super::RelayConsumption;
    use super::RelayInFlight;

    /// An element is retired only once taken; a discarded one stays stored and is never
    /// handed over again.
    #[test]
    fn test_relay_mail_is_consumed_only_when_taken() {
        let table = RelayInFlight::default();
        let (taken, discarded) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        assert_eq!(table.poll(taken), RelayConsumption::Unknown);
        let (fire, wait) = oneshot::channel();
        table.queued(taken, wait);
        assert_eq!(table.poll(taken), RelayConsumption::Pending);
        fire.send(()).unwrap();
        assert_eq!(table.poll(taken), RelayConsumption::Consumed);
        assert_eq!(table.poll(taken), RelayConsumption::Unknown);

        let (dropped, wait) = oneshot::channel::<()>();
        table.queued(discarded, wait);
        drop(dropped);
        assert_eq!(table.poll(discarded), RelayConsumption::Discarded);
        assert_eq!(table.poll(discarded), RelayConsumption::Discarded);
        table.retain(&Default::default());
        assert_eq!(table.poll(discarded), RelayConsumption::Unknown);
    }
}
