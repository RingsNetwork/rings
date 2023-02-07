//! This module implemented the `Measure` trait for swarm.
#![warn(missing_docs)]
use std::sync::Mutex;

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;

use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::measure::Measure;
use crate::prelude::rings_core::measure::MeasureCounter;
use crate::prelude::rings_core::prelude::dashmap::mapref::one::RefMut;
use crate::prelude::rings_core::prelude::dashmap::DashMap;

#[cfg(test)]
const DURATION: u64 = 1;
#[cfg(not(test))]
const DURATION: u64 = 60 * 60;

/// `Measure` is used to assess the reliability of peers by counting their behaviour.
/// It currently count the number of sent and received messages in a given period (1 hour).
/// The method [incr] should be called in the proper places.
#[derive(Default, Debug)]
pub struct PeriodicMeasure {
    counters: DashMap<(Did, MeasureCounter), Mutex<PeriodicCounter>>,
}

#[derive(Debug)]
struct PeriodicCounter {
    period: Duration,
    count: u64,
    previous: DateTime<Utc>,
    previous_count: u64,
}

impl PeriodicCounter {
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
    map: &DashMap<(Did, MeasureCounter), Mutex<PeriodicCounter>>,
    key: (Did, MeasureCounter),
) -> RefMut<'_, (Did, MeasureCounter), Mutex<PeriodicCounter>> {
    map.entry(key)
        .or_insert_with(|| Mutex::new(PeriodicCounter::new(DURATION)))
}

impl Measure for PeriodicMeasure {
    /// `incr` increments the counter of the given peer.
    fn incr(&self, did: Did, counter: MeasureCounter) {
        let c = ensure_counter(&self.counters, (did, counter));
        let Ok(mut c) = c.lock() else {return};
        c.incr()
    }

    /// `get_count` returns the counter of a peer in the previous period.
    fn get_count(&self, did: Did, counter: MeasureCounter) -> u64 {
        let c = ensure_counter(&self.counters, (did, counter));
        let Ok(mut c) = c.lock() else {return 0};
        c.get()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_measure_counter() {
        let did1 = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let did2 = Did::from_str("0x999999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();

        let measure = PeriodicMeasure::default();
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
        let did = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();

        let measure = PeriodicMeasure::default();
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
