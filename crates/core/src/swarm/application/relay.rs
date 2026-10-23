//! Relay mail handed to the application inbox and not yet retired from storage.
//!
//! ```text
//!   drain pass:  Unknown ──deliver──▶ Untaken
//!                Untaken ──application takes it from the inbox──▶ Consumed
//!                Consumed ──retired from storage──▶ forgotten (`retain`)
//! ```
//!
//! Law (at least once across restarts): a stored element is retired only after the application
//! took its message from the inbox and its release from relay custody committed (the signal
//! recorded here fires only then), so a later pass, a released inbox, a failed retirement or a
//! restart before that never loses it: a restart's redelivery finds it in custody and queues it
//! again (#936, the custody law of `message::replay`). Taking ends core's custody: what the
//! application does with the message afterwards, including dropping it beyond its own queues,
//! is the application's.
//! An untaken element whose inbox discarded it (a released inbox) stays stored and is not
//! handed over again in this process.
//! The table holds only elements still stored: each pass keeps the entries of the elements it
//! saw, so it is bounded by the stored carrier.

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
    /// Handed over and not taken: queued in the inbox, or discarded by a released one. Either
    /// way it stays stored and is not handed over again.
    Untaken,
    /// Taken by the application: the stored element may be retired.
    Consumed,
}

/// One handed-over element: the signal its taking fires, or the record that it fired.
enum InFlight {
    /// Handed over; the receiver resolves once taken and is cancelled once discarded.
    Queued(oneshot::Receiver<()>),
    /// Taken; kept until the element leaves storage, so a failed retirement is retried.
    Consumed,
}

/// The relay elements handed to the inbox, by transaction.
#[derive(Default)]
pub(crate) struct RelayInFlight(Mutex<HashMap<uuid::Uuid, InFlight>>);

impl RelayInFlight {
    /// The table; every critical section leaves it consistent, so a poisoned lock is still valid.
    fn lock(&self) -> MutexGuard<'_, HashMap<uuid::Uuid, InFlight>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where the element of `tx_id` stands. A consumed element stays recorded as consumed until
    /// it leaves storage ([`Self::retain`]).
    pub(crate) fn poll(&self, tx_id: uuid::Uuid) -> RelayConsumption {
        let mut table = self.lock();
        let Some(entry) = table.get_mut(&tx_id) else {
            return RelayConsumption::Unknown;
        };
        let InFlight::Queued(taken) = entry else {
            return RelayConsumption::Consumed;
        };
        match taken.try_recv() {
            Ok(Some(())) => {
                *entry = InFlight::Consumed;
                RelayConsumption::Consumed
            }
            Ok(None) | Err(oneshot::Canceled) => RelayConsumption::Untaken,
        }
    }

    /// Record the element of `tx_id` as handed over, with the signal its taking fires.
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

    /// An element is consumed only once taken, and stays consumed until it leaves storage, so
    /// a retirement that failed is retried by a later pass instead of redelivering it.
    #[test]
    fn test_relay_mail_is_consumed_once_taken_until_it_leaves_storage() {
        let table = RelayInFlight::default();
        let taken = uuid::Uuid::new_v4();
        assert_eq!(table.poll(taken), RelayConsumption::Unknown);
        let (fire, wait) = oneshot::channel();
        table.queued(taken, wait);
        assert_eq!(table.poll(taken), RelayConsumption::Untaken);
        fire.send(()).unwrap();
        assert_eq!(table.poll(taken), RelayConsumption::Consumed);
        assert_eq!(table.poll(taken), RelayConsumption::Consumed);
        table.retain(&[taken].into());
        assert_eq!(table.poll(taken), RelayConsumption::Consumed);
        table.retain(&Default::default());
        assert_eq!(table.poll(taken), RelayConsumption::Unknown);
    }

    /// A discarded element stays stored and is never handed over again in this process.
    #[test]
    fn test_discarded_relay_mail_stays_untaken() {
        let table = RelayInFlight::default();
        let discarded = uuid::Uuid::new_v4();
        let (dropped, wait) = oneshot::channel::<()>();
        table.queued(discarded, wait);
        drop(dropped);
        assert_eq!(table.poll(discarded), RelayConsumption::Untaken);
        assert_eq!(table.poll(discarded), RelayConsumption::Untaken);
        table.retain(&Default::default());
        assert_eq!(table.poll(discarded), RelayConsumption::Unknown);
    }
}
