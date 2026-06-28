#![warn(missing_docs)]

//! This module implemented the `Measure` trait for swarm.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use dashmap::mapref::one::RefMut;
use dashmap::DashMap;
use rings_core::dht::Did;
use rings_core::measure;
use rings_core::measure::Measure;
use rings_core::measure::MeasureCounter;
use rings_core::measure::PeerQuality;
use rings_core::measure::PeerQualityEvidence;
use rings_core::measure::PeerQualityThresholds;
use rings_core::storage::KvStorageInterface;
use rings_derive::MeasureBehaviour;

#[cfg(test)]
const DURATION: u64 = 1;
#[cfg(not(test))]
const DURATION: u64 = 60 * 60;

/// `MeasureStorage` is the type accepted by `PeriodicMeasure::new`.
/// It's used to store counts in a storage media provided by user.
#[cfg(feature = "browser")]
pub type MeasureStorage = Box<dyn KvStorageInterface<u64>>;

/// `MeasureStorage` is the type accepted by `PeriodicMeasure::new`.
/// It's used to store counts in a storage media provided by user.
#[cfg(not(feature = "browser"))]
pub type MeasureStorage = Box<dyn KvStorageInterface<u64> + Sync + Send>;

/// `PeriodicMeasure` is used to assess the reliability of peers by counting their behaviour.
/// It currently count the number of sent and received messages in a given period (1 hour).
/// The method [Measure::incr] should be called in the proper places.
#[derive(MeasureBehaviour)]
pub struct PeriodicMeasure {
    storage: MeasureStorage,
    counters: DashMap<(Did, MeasureCounter), Mutex<PeriodicCounter>>,
    clock: Arc<dyn MeasureClock>,
}

// Boundary: wall-clock time is injected here. The counter transition below is
// pure with respect to its `now` input, so tests can advance time without
// sleeping while production still reads `Utc::now`.
trait MeasureClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

struct SystemMeasureClock;

impl MeasureClock for SystemMeasureClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug)]
struct PeriodicCounter {
    // Invariant: `previous` is the start time of the current counting window;
    // `count` records the current window; `previous_count` records the most
    // recently completed window persisted through `barely_get`.
    period: Duration,
    count: u64,
    previous: DateTime<Utc>,
    previous_count: u64,
}

impl PeriodicCounter {
    fn new(period: u64, previous_count: u64, now: DateTime<Utc>) -> Self {
        Self {
            period: Duration::seconds(period as i64),
            count: 0,
            previous: now,
            previous_count,
        }
    }

    // Reset periodic count on next period
    fn refresh_at(&mut self, now: DateTime<Utc>) -> bool {
        if now - self.previous < self.period {
            return false;
        }

        self.previous_count = self.count;
        self.count = 0;
        self.previous = now;
        true
    }

    // If there is no recourd in current period, get previous_count instead
    fn barely_get(&self) -> u64 {
        if self.previous_count == 0 {
            self.count
        } else {
            self.previous_count
        }
    }

    // Check period, then increase
    fn incr_at(&mut self, now: DateTime<Utc>) -> (u64, bool) {
        let is_refreshed = self.refresh_at(now);
        self.count += 1;
        (self.barely_get(), is_refreshed)
    }

    // Check period, return count or previous count
    fn get_at(&mut self, now: DateTime<Utc>) -> (u64, bool) {
        let is_refreshed = self.refresh_at(now);
        (self.barely_get(), is_refreshed)
    }
}

impl PeriodicMeasure {
    /// Create a new `PeriodicMeasure` with the given storage.
    pub fn new(storage: MeasureStorage) -> Self {
        Self {
            storage,
            counters: DashMap::new(),
            clock: Arc::new(SystemMeasureClock),
        }
    }

    #[cfg(all(test, feature = "node"))]
    fn new_with_clock(storage: MeasureStorage, clock: Arc<dyn MeasureClock>) -> Self {
        Self {
            storage,
            counters: DashMap::new(),
            clock,
        }
    }

    fn gen_storage_key(did: Did, counter: MeasureCounter) -> String {
        format!("PeriodicMeasure/counters/{did}/{counter:?}")
    }

    // Get count from storage, or create a new count instance.
    async fn ensure_counter(
        &self,
        did: Did,
        counter: MeasureCounter,
        now: DateTime<Utc>,
    ) -> RefMut<'_, (Did, MeasureCounter), Mutex<PeriodicCounter>> {
        let k = Self::gen_storage_key(did, counter);
        let count = self
            .storage
            .get(&k)
            .await
            .unwrap_or_else(|e| {
                log::error!("Failed to get counter: {e:?}");
                Some(0)
            })
            .unwrap_or(0);
        self.counters
            .entry((did, counter))
            .or_insert_with(|| Mutex::new(PeriodicCounter::new(DURATION, count, now)))
    }

    async fn save_counter(&self, did: Did, counter: MeasureCounter, count: u64) {
        let k = Self::gen_storage_key(did, counter);
        self.storage.put(&k, &count).await.unwrap_or_else(|e| {
            log::error!("Failed to save counter: {e:?}");
        })
    }
}

#[cfg_attr(feature = "node", async_trait)]
#[cfg_attr(feature = "browser", async_trait(?Send))]
impl Measure for PeriodicMeasure {
    /// `incr` increments the counter of the given peer.
    async fn incr(&self, did: Did, counter: MeasureCounter) {
        let now = self.clock.now();
        let (count, is_refreshed) = {
            let c = self.ensure_counter(did, counter, now).await;
            let result = if let Ok(mut c) = c.lock() {
                c.incr_at(now)
            } else {
                return;
            };
            result
        };
        if is_refreshed {
            self.save_counter(did, counter, count).await;
        }
    }

    /// `get_count` returns the counter of a peer in the current or previous period.
    async fn get_count(&self, did: Did, counter: MeasureCounter) -> u64 {
        let now = self.clock.now();
        let (count, is_refreshed) = {
            let c = self.ensure_counter(did, counter, now).await;
            let result = if let Ok(mut c) = c.lock() {
                c.get_at(now)
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

#[cfg_attr(feature = "node", async_trait)]
#[cfg_attr(feature = "browser", async_trait(?Send))]
impl measure::BehaviourJudgement for PeriodicMeasure {
    async fn quality(&self, did: Did) -> PeerQuality {
        let thresholds = PeerQualityThresholds::new(
            crate::consts::CONNECT_FAILED_LIMIT,
            crate::consts::MSG_SEND_FAILED_LIMIT,
            crate::consts::MSG_RECV_FAILED_LIMIT,
        );
        PeerQualityEvidence::from_measure(self, did)
            .await
            .classify(thresholds)
    }

    async fn good(&self, did: Did) -> bool {
        let connection_is_good = <Self as measure::ConnectBehaviour<
            { crate::consts::CONNECT_FAILED_LIMIT },
        >>::good(self, did)
        .await;
        let send_is_good = <Self as measure::MessageSendBehaviour<
            { crate::consts::MSG_SEND_FAILED_LIMIT },
        >>::good(self, did)
        .await;
        let receive_is_good = <Self as measure::MessageRecvBehaviour<
            { crate::consts::MSG_RECV_FAILED_LIMIT },
        >>::good(self, did)
        .await;
        connection_is_good && send_is_good && receive_is_good
    }
}

#[cfg(test)]
#[cfg(feature = "node")]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;
    use std::sync::Mutex;

    use rings_core::measure::BehaviourJudgement;
    use rings_core::storage::sled::SledStorage;
    use rings_core::storage::MemStorage;

    use super::*;

    #[derive(Clone)]
    struct ManualMeasureClock {
        now: Arc<Mutex<DateTime<Utc>>>,
    }

    impl ManualMeasureClock {
        fn new(now: DateTime<Utc>) -> Self {
            Self {
                now: Arc::new(Mutex::new(now)),
            }
        }

        fn advance(&self, duration: Duration) {
            let Ok(mut now) = self.now.lock() else {
                panic!("manual measure clock lock poisoned");
            };
            let Some(advanced) = now.checked_add_signed(duration) else {
                panic!("manual measure clock overflow");
            };
            *now = advanced;
        }
    }

    impl MeasureClock for ManualMeasureClock {
        fn now(&self) -> DateTime<Utc> {
            let Ok(now) = self.now.lock() else {
                panic!("manual measure clock lock poisoned");
            };
            now.to_owned()
        }
    }

    fn advance_period(clock: &ManualMeasureClock) {
        clock.advance(Duration::seconds(DURATION as i64));
    }

    #[tokio::test]
    async fn test_measure_counter() {
        let ms = Box::new(MemStorage::new());

        let did1 = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let did2 = Did::from_str("0x999999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();

        let clock = ManualMeasureClock::new(Utc::now());
        let measure = PeriodicMeasure::new_with_clock(ms, Arc::new(clock.clone()));
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
        let ms = Box::new(MemStorage::new());

        let did = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();

        let clock = ManualMeasureClock::new(Utc::now());
        let measure = PeriodicMeasure::new_with_clock(ms, Arc::new(clock.clone()));
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 0);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 0);

        measure.incr(did, MeasureCounter::Sent).await;
        measure.incr(did, MeasureCounter::Sent).await;
        measure.incr(did, MeasureCounter::Received).await;

        // Will take current count since previous count is 0.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 2);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 1);

        advance_period(&clock);

        measure.incr(did, MeasureCounter::Sent).await;
        measure.incr(did, MeasureCounter::Received).await;
        measure.incr(did, MeasureCounter::Received).await;
        measure.incr(did, MeasureCounter::Received).await;

        // Will take previous count.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 2);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 1);

        advance_period(&clock);

        // Will take previous count.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 1);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 3);

        advance_period(&clock);

        // Will take previous count.
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 0);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 0);
    }

    #[tokio::test]
    async fn test_persistent_measure_storage() {
        let ms: MeasureStorage = Box::new(
            SledStorage::new_with_cap_and_path(4096, "tmp/measure_test_db")
                .await
                .unwrap(),
        );
        ms.clear().await.unwrap();

        let did = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let clock = ManualMeasureClock::new(Utc::now());
        let measure = PeriodicMeasure::new_with_clock(ms, Arc::new(clock.clone()));
        assert_eq!(measure.get_count(did, MeasureCounter::Sent).await, 0);
        assert_eq!(measure.get_count(did, MeasureCounter::Received).await, 0);

        measure.incr(did, MeasureCounter::Sent).await;
        measure.incr(did, MeasureCounter::Sent).await;
        measure.incr(did, MeasureCounter::Received).await;

        advance_period(&clock);

        // Flush to storage.
        let c1 = measure.get_count(did, MeasureCounter::Sent).await;
        assert_eq!(c1, 2);
        let c2 = measure.get_count(did, MeasureCounter::Received).await;
        assert_eq!(c2, 1);

        // Release lock of measure storage.
        drop(measure);

        // Create new measure.
        let ms2 = Box::new(
            SledStorage::new_with_cap_and_path(4096, "tmp/measure_test_db")
                .await
                .unwrap(),
        );
        let measure2 = PeriodicMeasure::new_with_clock(ms2, Arc::new(clock));

        // Will take previous count from storage.
        assert_eq!(measure2.get_count(did, MeasureCounter::Sent).await, 2);
        assert_eq!(measure2.get_count(did, MeasureCounter::Received).await, 1);
    }

    #[tokio::test]
    async fn repeated_disconnections_degrade_peer_quality(
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let ms = Box::new(MemStorage::new());
        let did = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0")?;
        let clock = ManualMeasureClock::new(Utc::now());
        let measure = PeriodicMeasure::new_with_clock(ms, Arc::new(clock));

        assert_eq!(measure.quality(did).await, PeerQuality::Unknown);

        measure.incr(did, MeasureCounter::Connect).await;
        assert_eq!(measure.quality(did).await, PeerQuality::Healthy);

        for _ in 0..crate::consts::CONNECT_FAILED_LIMIT {
            measure.incr(did, MeasureCounter::Disconnected).await;
        }

        assert_eq!(measure.quality(did).await, PeerQuality::Degraded);
        Ok(())
    }
}
