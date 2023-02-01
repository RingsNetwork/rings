//! This module provide the `Measure` struct and its implementations.
//! It is used to assess the reliability of remote peers.
#![warn(missing_docs)]
use std::sync::Mutex;

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use dashmap::mapref::one::RefMut;
use dashmap::DashMap;

use crate::dht::Did;

#[cfg(test)]
const DURATION: u64 = 1;
#[cfg(not(test))]
const DURATION: u64 = 60 * 60;

/// The tag of counters in measure.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MeasureCounter {
    /// The number of sent messages.
    Sent,
    /// The number of failed to sent messages.
    FailedToSend,
    /// The number of received messages.
    Received,
    /// The number of failed to receive messages.
    FailedToReceive,
}

/// `Measure` is used to assess the reliability of peers by counting their behaviour.
/// It currently count the number of sent and received messages in a given period (1 hour).
/// The method [incr] should be called in the proper places.
#[derive(Default, Debug)]
pub struct Measure {
    counters: DashMap<(Did, MeasureCounter), Mutex<PeriodCounter>>,
}

#[derive(Debug)]
struct PeriodCounter {
    period: Duration,
    count: u64,
    previous: DateTime<Utc>,
    previous_count: u64,
}

impl PeriodCounter {
    fn new(period: u64) -> Self {
        Self {
            period: Duration::seconds(period as i64),
            count: 0,
            previous: Utc::now(),
            previous_count: 0,
        }
    }

    fn refresh(&mut self) {
        let now = Utc::now();
        if now - self.previous >= self.period {
            self.previous_count = self.count;
            self.count = 0;
            self.previous = now;
        }
    }

    fn incr(&mut self) {
        self.refresh();
        self.count += 1;
    }

    fn get(&mut self) -> u64 {
        self.refresh();
        if self.previous_count == 0 {
            self.count
        } else {
            self.previous_count
        }
    }
}

fn ensure_counter(
    map: &DashMap<(Did, MeasureCounter), Mutex<PeriodCounter>>,
    key: (Did, MeasureCounter),
) -> RefMut<'_, (Did, MeasureCounter), Mutex<PeriodCounter>> {
    map.entry(key)
        .or_insert_with(|| Mutex::new(PeriodCounter::new(DURATION)))
}

impl Measure {
    /// `incr` increments the counter of the given peer.
    pub fn incr(&self, did: Did, counter: MeasureCounter) {
        let c = ensure_counter(&self.counters, (did, counter));
        let Ok(mut c) = c.lock() else {return};
        c.incr()
    }

    /// `get_count` returns the counter of a peer in the previous period.
    pub fn get_count(&self, did: Did, counter: MeasureCounter) -> u64 {
        let c = ensure_counter(&self.counters, (did, counter));
        let Ok(mut c) = c.lock() else {return 0};
        c.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dht::tests::gen_ordered_dids;

    #[test]
    fn test_measure_counter() {
        let did1 = gen_ordered_dids(1)[0];
        let did2 = gen_ordered_dids(1)[0];

        let measure = Measure::default();
        assert_eq!(measure.get_count(did1, MeasureCounter::Sent), 0);
        assert_eq!(measure.get_count(did2, MeasureCounter::Sent), 0);
        assert_eq!(measure.get_count(did1, MeasureCounter::Received), 0);
        assert_eq!(measure.get_count(did2, MeasureCounter::Received), 0);

        measure.incr(did1, MeasureCounter::Sent);
        measure.incr(did1, MeasureCounter::Received);

        measure.incr(did2, MeasureCounter::Sent);
        measure.incr(did2, MeasureCounter::Sent);
        measure.incr(did2, MeasureCounter::Received);
        measure.incr(did2, MeasureCounter::Received);
        measure.incr(did2, MeasureCounter::Received);

        assert_eq!(measure.get_count(did1, MeasureCounter::Sent), 1);
        assert_eq!(measure.get_count(did2, MeasureCounter::Sent), 2);
        assert_eq!(measure.get_count(did1, MeasureCounter::Received), 1);
        assert_eq!(measure.get_count(did2, MeasureCounter::Received), 3);
    }

    #[test]
    fn test_measure_period() {
        let did = gen_ordered_dids(1)[0];

        let measure = Measure::default();
        assert_eq!(measure.get_count(did, MeasureCounter::Sent), 0);
        assert_eq!(measure.get_count(did, MeasureCounter::Received), 0);

        measure.incr(did, MeasureCounter::Sent);
        measure.incr(did, MeasureCounter::Sent);
        measure.incr(did, MeasureCounter::Received);

        // Will take current count since previous count is 0.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent), 2);
        assert_eq!(measure.get_count(did, MeasureCounter::Received), 1);

        std::thread::sleep(std::time::Duration::from_secs(DURATION));

        measure.incr(did, MeasureCounter::Sent);
        measure.incr(did, MeasureCounter::Received);
        measure.incr(did, MeasureCounter::Received);
        measure.incr(did, MeasureCounter::Received);

        // Will take previous count.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent), 2);
        assert_eq!(measure.get_count(did, MeasureCounter::Received), 1);

        std::thread::sleep(std::time::Duration::from_secs(DURATION));

        // Will take previous count.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent), 1);
        assert_eq!(measure.get_count(did, MeasureCounter::Received), 3);

        std::thread::sleep(std::time::Duration::from_secs(DURATION));

        // Will take previous count.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent), 0);
        assert_eq!(measure.get_count(did, MeasureCounter::Received), 0);
    }
}
