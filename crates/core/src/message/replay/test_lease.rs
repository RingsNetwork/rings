//! The laws of sender sequence leases (#1043): no sequence is issued twice across any crash
//! point, a restart skips at most a lease, and a stream's reservations write once per lease.
//!
//! The model checks every script of sender steps up to a fixed length against the pure
//! [`allocate`]; the runtime table drives `TransactionReplay` over a shared store through each
//! named crash point.

use std::collections::BTreeSet;
use std::num::NonZeroU64;

use super::lease::allocate;
use super::lease::SenderLease;
use super::lease::TRANSACTION_SEQUENCE_LEASE;

/// The lease size of the model, as a count.
const N: u64 = TRANSACTION_SEQUENCE_LEASE;

/// The longest script the model enumerates.
const SCRIPT_MAX_STEPS: usize = 4;

/// One step of a sender's life in the model, each reservation of its count.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// A reservation that returns its range.
    Reserve(u64),
    /// A crash during a reservation, before its lease write (if it needs one).
    CrashBeforeWrite(u64),
    /// A crash during a reservation, after its lease write landed and before it returned.
    CrashAfterWrite(u64),
    /// A reservation whose lease write reports failure although the record landed.
    FailedWriteLanded(u64),
    /// A clean stop and restart, between reservations.
    Restart,
}

/// The counts the model reserves: single sequences, and ranges just inside, at and past a
/// lease, so scripts reach the block boundary and cross it.
const COUNTS: [u64; 4] = [1, N - 1, N, N + 1];

/// Every step of the model's alphabet.
fn alphabet() -> Vec<Step> {
    let kinds: [fn(u64) -> Step; 4] = [
        Step::Reserve,
        Step::CrashBeforeWrite,
        Step::CrashAfterWrite,
        Step::FailedWriteLanded,
    ];
    kinds
        .iter()
        .flat_map(|kind| COUNTS.iter().map(move |count| kind(*count)))
        .chain(std::iter::once(Step::Restart))
        .collect()
}

/// The model's world: the durable lease `D` (`None` before the first write), the run's
/// allocator, and every sequence any run has returned.
#[derive(Clone, Default)]
struct World {
    /// The stream's record.
    durable: Option<u64>,
    /// The live run's allocator.
    allocator: Option<SenderLease>,
    /// Every sequence returned, by every run.
    issued: BTreeSet<u64>,
}

impl World {
    /// The allocator a run restarted from the record starts with.
    fn restart(&mut self) {
        self.allocator = self.durable.map(SenderLease::restored);
    }

    /// Apply `step`, asserting that no returned sequence was returned before.
    fn apply(&mut self, step: Step) {
        let count = match step {
            Step::Restart => return self.restart(),
            Step::Reserve(count)
            | Step::CrashBeforeWrite(count)
            | Step::CrashAfterWrite(count)
            | Step::FailedWriteLanded(count) => count,
        };
        let Some(count) = NonZeroU64::new(count) else {
            return;
        };
        let Some(allocation) = allocate(self.allocator, count) else {
            return;
        };
        match step {
            Step::Reserve(_) => {
                if let Some(leased) = allocation.extend {
                    self.durable = Some(leased);
                }
                self.allocator = Some(allocation.next);
                for sequence in allocation.range {
                    assert!(self.issued.insert(sequence), "{sequence} issued twice");
                }
            }
            Step::CrashBeforeWrite(_) => self.restart(),
            Step::CrashAfterWrite(_) => {
                if let Some(leased) = allocation.extend {
                    self.durable = Some(leased);
                }
                self.restart();
            }
            Step::FailedWriteLanded(_) => {
                if let Some(leased) = allocation.extend {
                    self.durable = Some(leased);
                }
            }
            Step::Restart => {}
        }
    }

    /// The invariants of the no-reuse law: `I ≤ D`, and the allocator is `issued ≤ leased ≤ D`
    /// with `issued ≥ I`.
    fn assert_invariants(&self) {
        let highest = self.issued.last().copied();
        if let Some(highest) = highest {
            assert!(self.durable.is_some_and(|durable| highest <= durable));
        }
        if let Some(lease) = self.allocator {
            assert!(self
                .durable
                .is_some_and(|durable| lease.leased() <= durable));
            let next = allocate(Some(lease), NonZeroU64::MIN).map(|allocation| allocation.range);
            assert!(next.is_some_and(|range| highest.is_none_or(|high| *range.start() > high)));
        }
    }

    /// The sequence the live run would issue next.
    fn next_first(&self) -> u64 {
        match self.allocator {
            None => 0,
            Some(lease) => allocate(Some(lease), NonZeroU64::MIN)
                .map_or(u64::MAX, |allocation| *allocation.range.start()),
        }
    }
}

/// The most sequences one step other than a returning reservation may skip: `max(N, n)` over
/// the counts `n` of the alphabet (the gap law of `lease`).
fn skip_bound() -> u64 {
    COUNTS.iter().copied().fold(N, u64::max)
}

/// Every script of at most [`SCRIPT_MAX_STEPS`] steps keeps the invariants after each step,
/// returns no sequence twice, and skips at most a lease (or the interrupted range) at each step
/// that returns nothing: the no-reuse and gap laws of `lease`, by induction over the steps.
#[test]
fn test_no_sequence_is_issued_twice_across_any_crash_point() {
    let alphabet = alphabet();
    let mut frontier = vec![World::default()];
    for _ in 0..SCRIPT_MAX_STEPS {
        frontier = frontier
            .iter()
            .flat_map(|world| {
                alphabet.iter().map(move |step| {
                    let mut next = world.clone();
                    next.apply(*step);
                    next.assert_invariants();
                    if !matches!(step, Step::Reserve(_)) {
                        let skipped = next.next_first().checked_sub(world.next_first());
                        assert!(
                            skipped.is_some_and(|skipped| skipped <= skip_bound()),
                            "{step:?} skipped {skipped:?}"
                        );
                    }
                    next
                })
            })
            .collect();
    }
}

/// A run that reserves one sequence at a time writes once per lease: the first reservation and
/// each one that crosses a lease boundary, and no other.
#[test]
fn test_single_reservations_write_once_per_lease() {
    let mut allocator = None;
    let mut writes = Vec::new();
    for sequence in 0..3 * N {
        let allocation = allocate(allocator, NonZeroU64::MIN);
        assert_eq!(
            allocation
                .as_ref()
                .map(|allocation| allocation.range.clone()),
            Some(sequence..=sequence)
        );
        let Some(allocation) = allocation else {
            return;
        };
        writes.extend(allocation.extend.map(|leased| (sequence, leased)));
        allocator = Some(allocation.next);
    }
    assert_eq!(writes, vec![(0, N - 1), (N, 2 * N - 1), (2 * N, 3 * N - 1)]);
}

/// A reservation larger than a lease leases exactly its own range, and the next one writes.
#[test]
fn test_a_reservation_past_a_lease_leases_its_own_range() {
    let count = NonZeroU64::new(N + 1).unwrap_or(NonZeroU64::MIN);
    let allocation = allocate(None, count);
    assert_eq!(
        allocation
            .as_ref()
            .map(|allocation| (allocation.range.clone(), allocation.extend)),
        Some((0..=N, Some(N)))
    );
    let next = allocation.and_then(|allocation| allocate(Some(allocation.next), NonZeroU64::MIN));
    assert_eq!(
        next.map(|allocation| (allocation.range, allocation.extend)),
        Some((N + 1..=N + 1, Some(2 * N)))
    );
}

/// The sequence space ends at `u64::MAX`: a lease saturates there, and the allocator then
/// refuses rather than wrap.
#[test]
fn test_a_lease_saturates_and_then_exhausts() {
    let last = allocate(Some(SenderLease::restored(u64::MAX - 1)), NonZeroU64::MIN);
    assert_eq!(
        last.as_ref()
            .map(|allocation| (allocation.range.clone(), allocation.extend)),
        Some((u64::MAX..=u64::MAX, Some(u64::MAX)))
    );
    assert_eq!(
        last.and_then(|allocation| allocate(Some(allocation.next), NonZeroU64::MIN)),
        None
    );
    assert_eq!(
        allocate(Some(SenderLease::restored(u64::MAX)), NonZeroU64::MIN),
        None
    );
}

/// The runtime through each named crash point, over one store shared across restarts.
mod runtime {
    use std::num::NonZeroU64;
    use std::ops::RangeInclusive;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use super::N;
    use crate::dht::Did;
    use crate::error::Error;
    use crate::error::Result;
    use crate::message::replay::test_storage::Hooked;
    use crate::message::replay::test_storage::StorageHooks;
    use crate::message::replay::StreamKey;
    use crate::message::replay::TransactionReplay;
    use crate::message::MessageCategory;

    /// The one stream the table drives.
    fn stream() -> StreamKey {
        StreamKey::new(
            7,
            Did::from(1_u32),
            Did::from(99_u32),
            MessageCategory::Application,
        )
    }

    /// Hooks that count record writes and fail them, before the store is touched, while
    /// `failing` is set.
    #[derive(Default)]
    struct WriteLedger {
        /// Record writes attempted.
        writes: AtomicUsize,
        /// Whether the next writes fail.
        failing: AtomicBool,
    }

    #[async_trait::async_trait]
    impl StorageHooks for WriteLedger {
        /// Count the write, then fail it if `failing` is set.
        async fn before_put(&self, _key: &str) -> Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.failing.load(Ordering::SeqCst) {
                return Err(Error::TransactionReplayStateInvalid);
            }
            Ok(())
        }
    }

    /// A memory store shared across restarts, behind a [`WriteLedger`].
    type Ledgered = Arc<Hooked<WriteLedger>>;

    /// A runtime over `storage`: a fresh process.
    fn start(storage: &Ledgered) -> Arc<TransactionReplay> {
        TransactionReplay::new_shared(Box::new(Arc::clone(storage)))
    }

    /// Reserve `count` sequences of the stream.
    async fn reserve(replay: &Arc<TransactionReplay>, count: u64) -> Result<RangeInclusive<u64>> {
        let count = NonZeroU64::new(count).ok_or(Error::TransactionReplayStateInvalid)?;
        replay.reserve(stream(), count).await
    }

    /// One crash point: the single reservations a run makes, whether the run's last lease
    /// write fails before landing, and what a restarted run then issues.
    struct CrashPoint {
        /// The crash point's name, for a failing assertion.
        name: &'static str,
        /// Single reservations the run completes.
        completed: u64,
        /// Whether one more reservation follows whose lease write fails (a crash before it).
        fail_next_write: bool,
        /// Record writes the run makes, the failed one included.
        writes: usize,
        /// The first sequence of the restarted run.
        resumes_at: u64,
    }

    /// Each crash point of the no-reuse law on the runtime: before the lease write, after it,
    /// mid-block and at the block boundary. A crash after the write and before the reply leaves
    /// the store as a completed reservation whose range was never used, so it is the run that
    /// completed that reservation.
    #[tokio::test]
    async fn test_each_crash_point_resumes_past_the_durable_lease() -> Result<()> {
        let table = [
            CrashPoint {
                name: "mid-block",
                completed: 3,
                fail_next_write: false,
                writes: 1,
                resumes_at: N,
            },
            CrashPoint {
                name: "at the block boundary",
                completed: N,
                fail_next_write: false,
                writes: 1,
                resumes_at: N,
            },
            CrashPoint {
                name: "before the lease write",
                completed: N,
                fail_next_write: true,
                writes: 2,
                resumes_at: N,
            },
            CrashPoint {
                name: "after the lease write",
                completed: N + 1,
                fail_next_write: false,
                writes: 2,
                resumes_at: 2 * N,
            },
        ];
        for point in table {
            let storage = Arc::new(Hooked::new(WriteLedger::default()));
            let replay = start(&storage);
            for sequence in 0..point.completed {
                assert_eq!(
                    reserve(&replay, 1).await?,
                    sequence..=sequence,
                    "{}",
                    point.name
                );
            }
            if point.fail_next_write {
                storage.hooks.failing.store(true, Ordering::SeqCst);
                assert!(reserve(&replay, 1).await.is_err(), "{}", point.name);
                storage.hooks.failing.store(false, Ordering::SeqCst);
            }
            assert_eq!(
                storage.hooks.writes.load(Ordering::SeqCst),
                point.writes,
                "{}",
                point.name
            );
            drop(replay);

            let restarted = start(&storage);
            let resumed = point.resumes_at;
            assert_eq!(
                reserve(&restarted, 1).await?,
                resumed..=resumed,
                "{}",
                point.name
            );
        }
        Ok(())
    }

    /// The sender side of one browser onion request (#1043): about 1,030 single reservations
    /// on one stream wrote 1,030 records before leases and write `⌈1030 / N⌉` = 17 now.
    #[tokio::test]
    async fn test_a_request_of_single_reservations_writes_once_per_lease() -> Result<()> {
        /// Reservations of one onion request's sender side, as measured in #1043.
        const RESERVATIONS: u64 = 1030;
        let storage = Arc::new(Hooked::new(WriteLedger::default()));
        let replay = start(&storage);
        for _ in 0..RESERVATIONS {
            reserve(&replay, 1).await?;
        }
        let expected = usize::try_from(RESERVATIONS.div_ceil(N))
            .map_err(|_| Error::TransactionReplayStateInvalid)?;
        assert_eq!(expected, 17);
        assert_eq!(storage.hooks.writes.load(Ordering::SeqCst), expected);
        Ok(())
    }
}
