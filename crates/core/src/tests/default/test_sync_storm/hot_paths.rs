//! The hot-path operation counts of the load-bounds paper's Obligation "Admission complexity"
//! (#938), read on the sync-storm matrix's largest scenarios: the counters are the transport's
//! test-only ones (`rings_transport::hot_path`), per thread, so a current-thread scenario reads
//! its own.

use rings_transport::hot_path::count;
use rings_transport::hot_path::reset;
use rings_transport::hot_path::HotPath;
use rings_transport::hot_path::HotPathCount;

use super::scenario::run_scenario;
use super::DeliveryStrategy;
use super::ProtectionProfile;
use super::ScenarioTopology;

/// The counts of every hot path over one run of `scenario`, which runs on this thread.
pub(super) async fn hot_path_counts(
    scenario: impl std::future::Future<Output = super::ScenarioOutcome>,
) -> [(HotPath, HotPathCount); 6] {
    reset();
    let outcome = scenario.await;
    super::cases::assert_and_report_enabled(outcome);
    HotPath::ALL.map(|path| (path, count(path)))
}

/// One row per hot path: its events, the operations per event on average, and the most one
/// event cost.
pub(super) fn report(label: &str, counts: &[(HotPath, HotPathCount)]) {
    for (path, count) in counts {
        let mean = count.operations as f64 / (count.events.max(1) as f64);
        println!(
            "hot-path {label} {path:?}: events={} operations={} mean={mean:.2} max={}",
            count.events, count.operations, count.max
        );
    }
}

/// Assert each hot path's regression bound over a run of `nodes` nodes, `L = ⌈log₂ nodes⌉`:
///
/// ```text
///   frame admission   max ≤ 2          the link lock, and the pool's only on a demand
///   frame release     max ≤ 2          the link lock, and the pool's only on a change
///   queue service     max ≤ L          an event visits what it may unblock, not the queue
///   pool service      operations ≤ L · events    a service visits the holdings it serves
///   protocol sends    operations ≤ nodes         one admission watcher per node
///   inbox withdrawal  max ≤ L          a fact reads its peer's facts, not the queue
/// ```
///
/// An `O(n)` path coming back exceeds its bound at the matrix's largest size.
pub(super) fn assert_hot_path_bounds(nodes: usize, counts: &[(HotPath, HotPathCount)]) {
    let log = u64::from(nodes.max(2).next_power_of_two().trailing_zeros());
    let nodes = u64::try_from(nodes).expect("node count fits");
    for (path, count) in counts {
        let within = match path {
            HotPath::FrameAdmission | HotPath::FrameRelease => count.max <= 2,
            HotPath::QueueService | HotPath::InboxRecord => count.max <= log,
            HotPath::PoolService => count.operations <= log * count.events,
            HotPath::ProtocolSend => count.operations <= nodes,
        };
        assert!(
            within,
            "{path:?} exceeds its bound at {nodes} nodes: {count:?}"
        );
    }
}

/// Report the hot-path counts of the PR matrix's largest scenarios.
#[tokio::test(start_paused = true)]
#[ignore = "explicit hot-path measurement"]
async fn test_report_hot_path_counts() {
    for (kind, seed) in [
        (ScenarioTopology::Ring, 750),
        (ScenarioTopology::Hotspot, 850),
    ] {
        let counts = hot_path_counts(run_scenario(
            50,
            kind,
            seed,
            ProtectionProfile::ALL_ENABLED,
            DeliveryStrategy::Fifo,
        ))
        .await;
        report(&format!("{kind:?}-50-seed{seed}"), &counts);
    }
}
