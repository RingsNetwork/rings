//! This module implemented the `Measure` trait for swarm.
#![warn(missing_docs)]
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;

use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::measure::Measure;
use crate::prelude::rings_core::measure::MeasureCounter;
use crate::prelude::rings_core::prelude::dashmap::mapref::one::RefMut;
use crate::prelude::rings_core::prelude::dashmap::DashMap;
use crate::prelude::PersistenceStorage;
use crate::prelude::PersistenceStorageReadAndWrite;

#[cfg(test)]
const DURATION: u64 = 1;
#[cfg(not(test))]
const DURATION: u64 = 60 * 60;

/// `PeriodicMeasure` is used to assess the reliability of peers by counting their behaviour.
/// It currently count the number of sent and received messages in a given period (1 hour).
/// The method [Measure::incr] should be called in the proper places.
#[derive(Debug)]
pub struct PeriodicMeasure {
    storage: Arc<PersistenceStorage>,
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
    fn new(period: u64, previous_count: u64) -> Self {
        Self {
            period: Duration::seconds(period as i64),
            count: 0,
            previous: Utc::now(),
            previous_count,
        }
    }

    fn refresh(&mut self) -> bool {
        let now = Utc::now();

        if now - self.previous < self.period {
            return false;
        }

        self.previous_count = self.count;
        self.count = 0;
        self.previous = now;
        true
    }

    fn barely_get(&self) -> u64 {
        if self.previous_count == 0 {
            self.count
        } else {
            self.previous_count
        }
    }

    fn incr(&mut self) -> (u64, bool) {
        let is_refreshed = self.refresh();
        self.count += 1;
        (self.barely_get(), is_refreshed)
    }

    fn get(&mut self) -> (u64, bool) {
        let is_refreshed = self.refresh();
        (self.barely_get(), is_refreshed)
    }
}

impl PeriodicMeasure {
    /// Create a new `PeriodicMeasure` with the given storage.
    pub fn new(storage: PersistenceStorage) -> Self {
        Self {
            storage: Arc::new(storage),
            counters: DashMap::new(),
        }
    }

    fn gen_storage_key(did: Did, counter: MeasureCounter) -> String {
        format!("PeriodicMeasure/counters/{}/{:?}", did, counter)
    }

    async fn ensure_counter(
        &self,
        did: Did,
        counter: MeasureCounter,
    ) -> RefMut<'_, (Did, MeasureCounter), Mutex<PeriodicCounter>> {
        let k = Self::gen_storage_key(did, counter);
        let count = self.storage.get(&k).await.unwrap_or_else(|e| {
            log::error!("Failed to get counter: {:?}", e);
            0
        });
        self.counters
            .entry((did, counter))
            .or_insert_with(|| Mutex::new(PeriodicCounter::new(DURATION, count)))
    }

    async fn save_counter(&self, did: Did, counter: MeasureCounter, count: u64) {
        let k = Self::gen_storage_key(did, counter);
        self.storage.put(&k, &count).await.unwrap_or_else(|e| {
            log::error!("Failed to save counter: {:?}", e);
        })
    }
}

#[cfg_attr(feature = "node", async_trait)]
#[cfg_attr(feature = "browser", async_trait(?Send))]
impl Measure for PeriodicMeasure {
    /// `incr` increments the counter of the given peer.
    async fn incr(&self, did: Did, counter: MeasureCounter) {
        let (count, is_refreshed) = {
            let c = self.ensure_counter(did, counter).await;
            let result = if let Ok(mut c) = c.lock() {
                c.incr()
            } else {
                return;
            };
            result
        };
        if is_refreshed {
            self.save_counter(did, counter, count).await;
        }
    }

    /// `get_count` returns the counter of a peer in the previous period.
    async fn get_count(&self, did: Did, counter: MeasureCounter) -> u64 {
        let (count, is_refreshed) = {
            let c = self.ensure_counter(did, counter).await;
            let result = if let Ok(mut c) = c.lock() {
                c.get()
            } else {
                return 0;
            };
            result
        };
        if is_refreshed {
            self.save_counter(did, counter, count).await;
        }
        count
    }
}

#[cfg(test)]
#[cfg(feature = "node")]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[tokio::test]
    async fn test_measure_counter() {
        let ms_path = PersistenceStorage::random_path("./tmp");
        let ms = PersistenceStorage::new_with_path(ms_path.as_str())
            .await
            .unwrap();

        let did1 = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let did2 = Did::from_str("0x999999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();

        let measure = PeriodicMeasure::new(ms);
        assert_eq!(measure.get_count(did1, MeasureCounter::Sent).await, 0);
        assert_eq!(measure.get_count(did2, MeasureCounter::Sent).await, 0);
        assert_eq!(measure.get_count(did1, MeasureCounter::Received).await, 0);
        assert_eq!(measure.get_count(did2, MeasureCounter::Received).await, 0);

        measure.incr(did1, MeasureCounter::Sent).await;
        measure.incr(did1, MeasureCounter::Received).await;

        measure.incr(did2, MeasureCounter::Sent).await;
        measure.incr(did2, MeasureCounter::Sent).await;
        measure.incr(did2, MeasureCounter::Received).await;
        measure.incr(did2, MeasureCounter::Received).await;
        measure.incr(did2, MeasureCounter::Received).await;

        assert_eq!(measure.get_count(did1, MeasureCounter::Sent).await, 1);
        assert_eq!(measure.get_count(did2, MeasureCounter::Sent).await, 2);
        assert_eq!(measure.get_count(did1, MeasureCounter::Received).await, 1);
        assert_eq!(measure.get_count(did2, MeasureCounter::Received).await, 3);
    }

    #[tokio::test]
    async fn test_measure_period() {
        let ms_path = PersistenceStorage::random_path("./tmp");
        let ms = PersistenceStorage::new_with_path(ms_path.as_str())
            .await
            .unwrap();

        let did = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();

        let measure = PeriodicMeasure::new(ms);
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 0);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 0);

        measure.incr(did, MeasureCounter::Sent).await;
        measure.incr(did, MeasureCounter::Sent).await;
        measure.incr(did, MeasureCounter::Received).await;

        // Will take current count since previous count is 0.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 2);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 1);

        tokio::time::sleep(std::time::Duration::from_secs(DURATION)).await;

        measure.incr(did, MeasureCounter::Sent).await;
        measure.incr(did, MeasureCounter::Received).await;
        measure.incr(did, MeasureCounter::Received).await;
        measure.incr(did, MeasureCounter::Received).await;

        // Will take previous count.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 2);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 1);

        tokio::time::sleep(std::time::Duration::from_secs(DURATION)).await;

        // Will take previous count.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 1);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 3);

        tokio::time::sleep(std::time::Duration::from_secs(DURATION)).await;

        // Will take previous count.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 0);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 0);
    }

    #[tokio::test]
    async fn test_measure_storage() {
        let ms_path = PersistenceStorage::random_path("./tmp");
        let ms = PersistenceStorage::new_with_path(ms_path.as_str())
            .await
            .unwrap();

        let did = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let measure = PeriodicMeasure::new(ms);
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 0);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 0);

        measure.incr(did, MeasureCounter::Sent).await;
        measure.incr(did, MeasureCounter::Sent).await;
        measure.incr(did, MeasureCounter::Received).await;

        tokio::time::sleep(std::time::Duration::from_secs(DURATION)).await;

        // Flush to storage.
        measure.get_count(did, MeasureCounter::Sent).await;
        measure.get_count(did, MeasureCounter::Received).await;

        // Release lock of measure storage.
        drop(measure);

        // Create new measure.
        let ms2 = PersistenceStorage::new_with_path(ms_path.as_str())
            .await
            .unwrap();
        let measure2 = PeriodicMeasure::new(ms2);

        // Will take previous count from storage.
        assert_eq!(measure2.get_count(did, MeasureCounter::Sent).await, 2);
        assert_eq!(measure2.get_count(did, MeasureCounter::Received).await, 1);
    }
}
