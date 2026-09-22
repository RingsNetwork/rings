use super::*;
use crate::dht::Did;
use crate::ecc::SecretKey;

fn did() -> Did {
    SecretKey::random().address().into()
}

#[test]
fn test_peer_quality_evidence_classifies_unknown_healthy_and_degraded() {
    // Explicit policy preserves the former one-positive-observation test premise.
    let policy =
        rings_measure::ReliabilityPolicy::new(60, 1, PeerQualityThresholds::new(3, 10, 10))
            .unwrap();
    assert_eq!(
        PeerQualityEvidence::new(0, 0, 0, 0, 0, 0).classify_with_policy(policy),
        PeerQuality::Unknown
    );
    assert_eq!(
        PeerQualityEvidence::new(1, 0, 0, 0, 0, 0).classify_with_policy(policy),
        PeerQuality::Healthy
    );
    assert_eq!(
        PeerQualityEvidence::new(1, 3, 0, 0, 0, 0).classify_with_policy(policy),
        PeerQuality::Degraded
    );
    assert_eq!(
        PeerQualityEvidence::new(1, 0, 0, 10, 0, 0).classify_with_policy(policy),
        PeerQuality::Degraded
    );
    assert_eq!(
        PeerQualityEvidence::new(1, 0, 0, 0, 0, 10).classify_with_policy(policy),
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
