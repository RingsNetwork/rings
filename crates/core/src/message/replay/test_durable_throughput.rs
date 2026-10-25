//! Measured latency and throughput of replay transitions on a flushed native store (#909, #916).
//!
//! An authoritative store flushes before a batch of records returns, and every transition is
//! released only once its batch is durable (the group-commit law of `commit`). One admission at
//! a time therefore pays a whole batch write; admissions that run concurrently share batches,
//! so the node-wide rate grows with the transitions in flight. This benchmark measures both;
//! it is ignored by default because its figure depends on the disk, and is run with
//!
//! ```text
//! cargo test -p rings-core --release --lib test_durable_throughput -- --ignored --nocapture
//! ```

use std::time::Duration;
use std::time::Instant;

use futures::StreamExt;
use futures::TryStreamExt;

use super::ReplayStorage;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use crate::dht::Did;
use crate::error::Result;
use crate::message::MessageCategory;
use crate::storage::file::test_root::TempRoot;
use crate::storage::file::FileStorage;
use crate::storage::MemStorage;
use crate::storage::RecordAuthority;

/// Admissions measured per store.
const ADMISSIONS: u32 = 512;
/// Admissions in flight in the concurrent measurement: about the transitions a relay of an
/// onion loop has outstanding across its links.
const IN_FLIGHT: usize = 64;

/// The stream of origin `origin` toward the fixed destination of the benchmark.
fn stream(origin: u32) -> StreamKey {
    StreamKey::new(
        7,
        Did::from(origin),
        Did::from(u32::MAX),
        MessageCategory::Application,
    )
}

/// The digest of the benchmark transaction of origin `origin`.
fn digest(origin: u32) -> TransactionDigest {
    TransactionDigest::new([u8::try_from(origin % 251).unwrap_or(0); 32])
}

/// Admit the first transaction of `ADMISSIONS` streams through the production path (detached,
/// quota-charged), one per origin so that no origin quota binds, and return each admission's
/// latency, sorted. Each admission writes one record.
async fn admission_latencies(storage: ReplayStorage) -> Result<Vec<Duration>> {
    let replay = TransactionReplay::new_shared(storage);
    let mut latencies = Vec::new();
    for origin in 0..ADMISSIONS {
        let started = Instant::now();
        replay
            .admit_with_quota(
                stream(origin),
                0,
                digest(origin),
                MessageCategory::Application.into(),
                0,
            )
            .await?;
        latencies.push(started.elapsed());
    }
    latencies.sort();
    Ok(latencies)
}

/// Admit the first transaction of `ADMISSIONS` streams with [`IN_FLIGHT`] admissions running at
/// once, and return the admissions per second.
async fn concurrent_rate(storage: ReplayStorage) -> Result<f64> {
    let replay = TransactionReplay::new_shared(storage);
    let started = Instant::now();
    futures::stream::iter(0..ADMISSIONS)
        .map(|origin| {
            let replay = std::sync::Arc::clone(&replay);
            async move {
                replay
                    .admit_with_quota(
                        stream(origin),
                        0,
                        digest(origin),
                        MessageCategory::Application.into(),
                        0,
                    )
                    .await
            }
        })
        .buffer_unordered(IN_FLIGHT)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(f64::from(ADMISSIONS) / started.elapsed().as_secs_f64())
}

/// Print the throughput and latency quantiles of one store's admissions.
fn report(label: &str, latencies: &[Duration]) {
    let total = latencies.iter().sum::<Duration>();
    let quantile = |q: usize| latencies.get(latencies.len() * q / 100).copied();
    println!(
        "{label}: {:.0} admissions/s, p50 {:?}, p90 {:?}, p99 {:?}",
        latencies.len() as f64 / total.as_secs_f64(),
        quantile(50).unwrap_or_default(),
        quantile(90).unwrap_or_default(),
        quantile(99).unwrap_or_default(),
    );
}

/// The admission ceiling of the authoritative file store, against the disposable file store
/// and memory.
#[ignore = "benchmark: its figure depends on the disk"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_durable_throughput_of_replay_admissions() -> Result<()> {
    let root = TempRoot::new("replay-bench");
    let authoritative = FileStorage::new_with_cap_path_and_authority(
        1 << 24,
        root.join("authoritative"),
        RecordAuthority::Authoritative,
    )
    .await?;
    let disposable = FileStorage::new_with_cap_and_path(1 << 24, root.join("disposable")).await?;
    report(
        "authoritative file",
        &admission_latencies(Box::new(authoritative)).await?,
    );
    report(
        "disposable file",
        &admission_latencies(Box::new(disposable)).await?,
    );
    report(
        "memory",
        &admission_latencies(Box::new(MemStorage::new())).await?,
    );
    let concurrent = FileStorage::new_with_cap_path_and_authority(
        1 << 24,
        root.join("concurrent"),
        RecordAuthority::Authoritative,
    )
    .await?;
    println!(
        "authoritative file, {IN_FLIGHT} in flight: {:.0} admissions/s",
        concurrent_rate(Box::new(concurrent)).await?
    );
    Ok(())
}
