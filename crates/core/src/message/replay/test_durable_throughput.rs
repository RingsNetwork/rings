//! Measured ceiling of replay transitions on a flushed native store (#909, #916).
//!
//! Every reservation and admission writes its stream's record under the global replay lock,
//! and an authoritative store flushes the file and the directory before the write returns, so
//! the flush latency bounds the node-wide transition rate. This benchmark measures that bound;
//! it is ignored by default because its figure depends on the disk, and is run with
//!
//! ```text
//! cargo test -p rings-core --release --lib test_durable_throughput -- --ignored --nocapture
//! ```

use std::time::Duration;
use std::time::Instant;

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

/// Admissions measured per store.
const ADMISSIONS: u32 = 512;

/// Admit the first transaction of `ADMISSIONS` streams through the production path (detached,
/// quota-charged), one per origin so that no origin quota binds, and return each admission's
/// latency, sorted. Each admission writes one record.
async fn admission_latencies(storage: ReplayStorage) -> Result<Vec<Duration>> {
    let replay = TransactionReplay::new_shared(storage);
    let mut latencies = Vec::new();
    for origin in 0..ADMISSIONS {
        let key = StreamKey::new(
            7,
            Did::from(origin),
            Did::from(u32::MAX),
            MessageCategory::Application,
        );
        let digest = TransactionDigest::new([u8::try_from(origin % 251).unwrap_or(0); 32]);
        let started = Instant::now();
        replay
            .admit_with_quota(key, 0, digest, MessageCategory::Application.into(), 0)
            .await?;
        latencies.push(started.elapsed());
    }
    latencies.sort();
    Ok(latencies)
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
    let authoritative =
        FileStorage::new_authoritative_with_cap_and_path(1 << 24, root.join("authoritative"))
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
    Ok(())
}
