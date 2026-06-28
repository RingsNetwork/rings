use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use super::*;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;
use crate::ecc::SecretKey;
use crate::measure::BehaviourJudgement;
use crate::measure::Measure;
use crate::storage::MemStorage;

#[derive(Default)]
struct RecordingMeasure {
    counters: Mutex<Vec<(Did, MeasureCounter)>>,
    qualities: Mutex<BTreeMap<Did, PeerQuality>>,
}

impl RecordingMeasure {
    fn snapshot_counters(&self) -> std::io::Result<Vec<(Did, MeasureCounter)>> {
        self.counters
            .lock()
            .map(|counters| counters.clone())
            .map_err(|_| std::io::Error::other("counters poisoned"))
    }

    fn set_quality(&self, did: Did, quality: PeerQuality) -> std::io::Result<()> {
        self.qualities
            .lock()
            .map(|mut qualities| {
                qualities.insert(did, quality);
            })
            .map_err(|_| std::io::Error::other("qualities poisoned"))
    }
}

#[async_trait]
impl Measure for RecordingMeasure {
    async fn incr(&self, did: Did, counter: MeasureCounter) {
        match self.counters.lock() {
            Ok(mut counters) => counters.push((did, counter)),
            Err(_) => tracing::error!("RecordingMeasure counters mutex is poisoned"),
        }
    }

    async fn get_count(&self, _did: Did, _counter: MeasureCounter) -> u64 {
        0
    }
}

#[async_trait]
impl BehaviourJudgement for RecordingMeasure {
    async fn quality(&self, did: Did) -> PeerQuality {
        match self.qualities.lock() {
            Ok(qualities) => qualities.get(&did).copied().unwrap_or(PeerQuality::Unknown),
            Err(_) => {
                tracing::error!("RecordingMeasure qualities mutex is poisoned");
                PeerQuality::Unknown
            }
        }
    }

    async fn good(&self, _did: Did) -> bool {
        true
    }
}

fn transport_with_measure(measure: MeasureImpl) -> Result<SwarmTransport> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key)?;
    let dht = Arc::new(PeerRing::new_with_storage_and_finger_table_size(
        session_sk.account_did(),
        3,
        Box::new(MemStorage::new()),
        DEFAULT_FINGER_TABLE_SIZE,
    ));
    Ok(SwarmTransport::new(
        0,
        SwarmWebrtcConfig::new("".to_string(), None, None),
        session_sk,
        dht,
        Some(measure),
        SwarmTransportSettings::new(1, ReassemblyLimits::production()),
    ))
}

#[tokio::test]
async fn disconnected_observation_is_once_per_connection_epoch() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = transport_with_measure(measure.clone())?;
    let peer = SecretKey::random().address().into();

    transport.record_peer_disconnected(peer).await;
    transport.record_peer_disconnected(peer).await;
    transport.record_peer_connected(peer).await;
    transport.record_peer_disconnected(peer).await;

    assert_eq!(measure.snapshot_counters()?.as_slice(), &[
        (peer, MeasureCounter::Disconnected),
        (peer, MeasureCounter::Connect),
        (peer, MeasureCounter::Disconnected),
    ]);

    Ok(())
}

#[tokio::test]
async fn dht_candidate_order_uses_peer_quality_without_dropping_candidates() -> Result<()> {
    let degraded = SecretKey::random().address().into();
    let unknown = SecretKey::random().address().into();
    let healthy = SecretKey::random().address().into();
    let measure = Arc::new(RecordingMeasure::default());
    measure.set_quality(degraded, PeerQuality::Degraded)?;
    measure.set_quality(healthy, PeerQuality::Healthy)?;
    let transport = transport_with_measure(measure)?;

    let ordered = transport
        .order_dht_candidates_by_quality([degraded, unknown, healthy])
        .await;

    assert_eq!(ordered, vec![healthy, unknown, degraded]);

    Ok(())
}
