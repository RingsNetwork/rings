use super::*;
use crate::dht::Did;
use crate::ecc::SecretKey;

fn did() -> Did {
    SecretKey::random().address().into()
}

#[test]
fn test_peer_quality_evidence_classifies_unknown_healthy_and_degraded() {
    let thresholds = PeerQualityThresholds::new(3, 10, 10);
    assert_eq!(
        PeerQualityEvidence::new(0, 0, 0, 0, 0, 0).classify(thresholds),
        PeerQuality::Unknown
    );
    assert_eq!(
        PeerQualityEvidence::new(1, 0, 0, 0, 0, 0).classify(thresholds),
        PeerQuality::Healthy
    );
    assert_eq!(
        PeerQualityEvidence::new(1, 3, 0, 0, 0, 0).classify(thresholds),
        PeerQuality::Degraded
    );
    assert_eq!(
        PeerQualityEvidence::new(1, 0, 0, 10, 0, 0).classify(thresholds),
        PeerQuality::Degraded
    );
    assert_eq!(
        PeerQualityEvidence::new(1, 0, 0, 0, 0, 10).classify(thresholds),
        PeerQuality::Degraded
    );
}

#[test]
fn test_order_peers_by_quality_is_stable_permutation() {
    let degraded = did();
    let unknown_a = did();
    let healthy = did();
    let unknown_b = did();

    let ordered = order_peers_by_quality([
        (degraded, PeerQuality::Degraded),
        (unknown_a, PeerQuality::Unknown),
        (healthy, PeerQuality::Healthy),
        (unknown_b, PeerQuality::Unknown),
    ]);

    assert_eq!(ordered, vec![healthy, unknown_a, unknown_b, degraded]);
}
