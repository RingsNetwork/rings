//! Relay mail handed to the application inbox and not yet retired from storage.
//!
//! ```text
//!   drain pass:  Unknown ──deliver──▶ Untaken
//!                Untaken ──application takes it from the inbox──▶ Consumed
//!                Consumed ──absent from a carrier read──▶ forgotten (`retain`)
//!                Untaken ──spent──▶ forgotten (`retain`)
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
//!
//! Law (at most once in process): an untaken element is forgotten only once it is spent, never
//! because a carrier read lacks it. A read may come from a member that lags behind the holder
//! (#1059), so the element may still be offered later; forgetting it then would hand it over
//! twice. A consumed element absent from a read is forgotten: a later offer of it is refused by
//! the receiver window as a replay and retired, never handed over.
//!
//! Law (bounded): the table holds the consumed elements of the last read, and the untaken ones,
//! each of which is in relay custody until it is spent (the custody law), so at most the stored
//! carrier and `RELAY_CUSTODY_CAPACITY` entries.

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
    /// Handed over; `taken` resolves once taken and is cancelled once discarded, and the
    /// element is spent at `spent_at_ms`.
    Queued {
        /// The signal of the take.
        taken: oneshot::Receiver<()>,
        /// The instant from which no carrier delivers the element again.
        spent_at_ms: u128,
    },
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
        let InFlight::Queued { taken, .. } = entry else {
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

    /// Record the element of `tx_id` as handed over, with the signal its taking fires and the
    /// instant it is spent.
    pub(crate) fn queued(
        &self,
        tx_id: uuid::Uuid,
        taken: oneshot::Receiver<()>,
        spent_at_ms: u128,
    ) {
        self.lock()
            .insert(tx_id, InFlight::Queued { taken, spent_at_ms });
    }

    /// Forget every element absent from `stored`, the elements of a carrier read at `now_ms`,
    /// unless it was handed over and is not spent yet (the laws of this module).
    pub(crate) fn retain(&self, stored: &BTreeSet<uuid::Uuid>, now_ms: u128) {
        self.lock().retain(|tx_id, entry| {
            stored.contains(tx_id)
                || matches!(entry, InFlight::Queued { spent_at_ms, .. } if now_ms < *spent_at_ms)
        });
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::oneshot;

    use super::RelayConsumption;
    use super::RelayInFlight;

    /// The instant the elements of these tests are spent.
    const SPENT_AT_MS: u128 = 1_000;

    /// An element is consumed only once taken, and stays consumed until a carrier read lacks
    /// it, so a retirement that failed is retried by a later pass instead of redelivering it.
    #[test]
    fn test_relay_mail_is_consumed_once_taken_until_it_leaves_storage() {
        let table = RelayInFlight::default();
        let taken = uuid::Uuid::new_v4();
        assert_eq!(table.poll(taken), RelayConsumption::Unknown);
        let (fire, wait) = oneshot::channel();
        table.queued(taken, wait, SPENT_AT_MS);
        assert_eq!(table.poll(taken), RelayConsumption::Untaken);
        fire.send(()).unwrap();
        assert_eq!(table.poll(taken), RelayConsumption::Consumed);
        assert_eq!(table.poll(taken), RelayConsumption::Consumed);
        table.retain(&[taken].into(), 0);
        assert_eq!(table.poll(taken), RelayConsumption::Consumed);
        table.retain(&Default::default(), 0);
        assert_eq!(table.poll(taken), RelayConsumption::Unknown);
    }

    /// A discarded element stays stored and is never handed over again in this process.
    #[test]
    fn test_discarded_relay_mail_stays_untaken() {
        let table = RelayInFlight::default();
        let discarded = uuid::Uuid::new_v4();
        let (dropped, wait) = oneshot::channel::<()>();
        table.queued(discarded, wait, SPENT_AT_MS);
        drop(dropped);
        assert_eq!(table.poll(discarded), RelayConsumption::Untaken);
        assert_eq!(table.poll(discarded), RelayConsumption::Untaken);
    }

    /// An untaken element survives every carrier read that lacks it until it is spent, so a read
    /// of a lagging member never makes a later offer hand it over twice (#1059).
    #[test]
    fn test_untaken_relay_mail_is_forgotten_only_once_spent() {
        let table = RelayInFlight::default();
        let untaken = uuid::Uuid::new_v4();
        let (_fire, wait) = oneshot::channel::<()>();
        table.queued(untaken, wait, SPENT_AT_MS);
        table.retain(&Default::default(), SPENT_AT_MS - 1);
        assert_eq!(table.poll(untaken), RelayConsumption::Untaken);
        table.retain(&Default::default(), SPENT_AT_MS);
        assert_eq!(table.poll(untaken), RelayConsumption::Unknown);
    }
}
