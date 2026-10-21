//! Read-your-writes (#948, #982): a writer applies its own operation to its fetch cache, as every
//! owner applies it, so its reads contain the write whichever owner a later lookup reaches.
//!
//! The race of #948, stated over one placement whose owner moves from `X` to `Y`:
//!
//! ```text
//! events  Apply  : X applies op           (the placement left before Ok; it lands later)
//!         Move   : the owner becomes Y    (a referenced view changes)
//!         HandOff: Y ← Y ⊔ X              (the next storage repair round, after Move)
//!         Fetch  : read = cache ⊔ reply(owner now), and a miss is no reply
//! ```
//!
//! Every schedule of `{Apply, Move, HandOff}` with `HandOff` after `Move`, with the fetch at any
//! point, is enumerated. Without the observation some schedule reads nothing; with it every
//! schedule reads the write, and the observation is below every replica that applied it.

use super::super::super::chord::PeerRing;
use crate::dht::entry::inbox::inbox_key;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::dht::ChordStorageCache;
use crate::dht::Did;
use crate::error::Result;
use crate::storage::MemStorage;
use crate::tests::data_carrier;
use crate::tests::expired;
use crate::tests::stamped_operation;
use crate::utils::get_epoch_ms;

/// The carrier every test writes.
const RESOURCE: u32 = 10;

/// The writer: the node whose reads must contain its own writes.
const WRITER: u32 = 1;

/// Another writer, whose writes the replicas hold before the writer's arrives.
const OTHER: u32 = 2;

/// The owner applying an operation; every operation here is stamped, so it issues no dot.
const OWNER: u32 = 3;

/// The carrier of `values` under the test resource, live for an hour from `now_ms`.
fn carrier(values: &[&str], now_ms: u128) -> Entry {
    data_carrier(Did::from(RESOURCE), values, now_ms)
}

/// `operation` of `values` on the test resource, stamped by `writer` at `now_ms`.
fn stamped(
    operation: fn(Entry) -> EntryOperation,
    values: &[&str],
    now_ms: u128,
    writer: u32,
) -> Result<EntryOperation> {
    stamped_operation(
        operation,
        Did::from(RESOURCE),
        values,
        now_ms,
        Did::from(writer),
    )
}

/// A fresh node whose cache is empty.
fn reader() -> PeerRing {
    PeerRing::new_with_storage(Did::from(WRITER), 3, Box::new(MemStorage::new()))
}

/// Whether `entry` serves the element `value`.
fn serves(entry: Option<&Entry>, value: &str) -> bool {
    entry.is_some_and(|entry| {
        entry
            .data
            .iter()
            .any(|data| data.as_ref() == value.as_bytes())
    })
}

/// The replica states an owner may hold when the writer's operation reaches it: nothing, an
/// element of another writer, an older overwrite of another writer, and an older copy of the
/// written value beside a removal of another one.
fn replica_states(now_ms: u128) -> Result<Vec<Option<Entry>>> {
    let other = stamped(EntryOperation::Extend, &["b"], now_ms - 2, OTHER)?;
    let older_overwrite = stamped(EntryOperation::Overwrite, &["c"], now_ms - 2, OTHER)?;
    let older_copy = stamped(EntryOperation::Extend, &["a", "b"], now_ms - 2, OTHER)?;
    let older_copy = older_copy.entry().clone();
    let removed_b = older_copy.tombstone(carrier(&["b"], now_ms))?;
    Ok(vec![
        None,
        Some(other.entry().clone()),
        Some(older_overwrite.entry().clone()),
        Some(removed_b),
    ])
}

/// `replica` after it applies `op` at `now_ms`, as an owner's operation transition does.
fn applied(replica: &Option<Entry>, op: &EntryOperation, now_ms: u128) -> Result<Entry> {
    let local = match replica {
        Some(replica) => replica.clone(),
        None => op.gen_default_entry()?,
    };
    local.operate(now_ms, op.clone(), Did::from(OWNER))
}

/// Soundness: an overwrite or an extend observed by its writer is below every replica that
/// applied it, `⊥.operate(op) ⊑ R.operate(op)`, so the first reply from such a replica adds
/// nothing new from `op` to the writer's cache.
#[test]
fn test_observed_write_is_below_every_replica_that_applies_it() -> Result<()> {
    let now_ms = get_epoch_ms();
    for operation in [
        EntryOperation::Overwrite as fn(Entry) -> EntryOperation,
        EntryOperation::Extend,
    ] {
        let op = stamped(operation, &["a"], now_ms, WRITER)?;
        let observed = applied(&None, &op, now_ms)?;
        for replica in replica_states(now_ms)? {
            let replica = applied(&replica, &op, now_ms)?;
            assert_eq!(observed.join(replica.clone())?, replica);
        }
    }
    Ok(())
}

/// One event of the #948 race.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    /// The old owner applies the operation.
    Apply,
    /// The placement's owner moves from the old owner to the new one.
    Move,
    /// The old owner hands its replica to the new one.
    HandOff,
    /// The writer fetches the carrier.
    Fetch,
}

/// Every schedule of the race: each order of `{Apply, Move, HandOff}` with `HandOff` after
/// `Move`, with `Fetch` inserted at every position.
fn schedules() -> Vec<Vec<Event>> {
    let orders = [
        [Event::Apply, Event::Move, Event::HandOff],
        [Event::Move, Event::Apply, Event::HandOff],
        [Event::Move, Event::HandOff, Event::Apply],
    ];
    orders
        .iter()
        .flat_map(|order| {
            (0..=order.len()).map(move |at| {
                let mut schedule = order.to_vec();
                schedule.insert(at, Event::Fetch);
                schedule
            })
        })
        .collect()
}

/// The writer's read in `schedule`: its cache, holding its own observed write iff `observes`,
/// joined with the reply of the owner at the fetch, where a miss is no reply.
async fn read_in(
    schedule: &[Event],
    op: &EntryOperation,
    now_ms: u128,
    observes: bool,
) -> Result<Option<Entry>> {
    let writer = reader();
    if observes {
        writer.local_cache_observe_write(op.clone()).await?;
    }
    let (mut old_owner, mut new_owner, mut moved) = (None, None, false);
    for event in schedule {
        match event {
            Event::Apply => old_owner = Some(applied(&old_owner, op, now_ms)?),
            Event::Move => moved = true,
            Event::HandOff => new_owner = old_owner.clone(),
            Event::Fetch => {
                let reply = if moved { &new_owner } else { &old_owner };
                if let Some(reply) = reply {
                    writer.local_cache_put(reply.clone()).await?;
                }
            }
        }
    }
    writer.local_cache_get(Did::from(RESOURCE)).await
}

/// The race of #948: the store's `Ok` does not order the fetch after the write, so a schedule
/// reads nothing without the observation (the fetch reaches the new owner before the hand-off,
/// or the old owner before the write lands); with it, every schedule reads the write.
#[tokio::test]
async fn test_writer_reads_its_write_in_every_schedule_of_the_race() -> Result<()> {
    let now_ms = get_epoch_ms();
    let op = stamped(EntryOperation::Overwrite, &["a"], now_ms, WRITER)?;
    let mut unobserved_misses = Vec::new();
    for schedule in schedules() {
        if !serves(read_in(&schedule, &op, now_ms, false).await?.as_ref(), "a") {
            unobserved_misses.push(schedule.clone());
        }
        assert!(
            serves(read_in(&schedule, &op, now_ms, true).await?.as_ref(), "a"),
            "the writer misses its own write in {schedule:?}"
        );
    }
    assert!(unobserved_misses.contains(&vec![
        Event::Apply,
        Event::Move,
        Event::Fetch,
        Event::HandOff
    ]));
    Ok(())
}

/// A tombstone observed by its writer hides the removed value from its reads, and a stale reply
/// that still holds the value does not bring it back.
#[tokio::test]
async fn test_writer_reads_its_own_removal() -> Result<()> {
    let now_ms = get_epoch_ms();
    let writer = reader();
    let held = stamped(EntryOperation::Extend, &["a", "b"], now_ms - 1, OTHER)?;
    writer.local_cache_put(held.entry().clone()).await?;

    writer
        .local_cache_observe_write(EntryOperation::Tombstone(carrier(&["a"], now_ms)))
        .await?;
    writer.local_cache_put(held.entry().clone()).await?;

    let read = writer.local_cache_get(Did::from(RESOURCE)).await?;
    assert!(!serves(read.as_ref(), "a"));
    assert!(serves(read.as_ref(), "b"));
    Ok(())
}

/// What no owner applies is not observed: an operation the admission law refuses, and a relay
/// inbox write, which no lookup reads.
#[tokio::test]
async fn test_writer_observes_only_what_owners_apply() -> Result<()> {
    let now_ms = get_epoch_ms();
    let writer = reader();
    let refused = EntryOperation::Overwrite(expired(carrier(&["a"], now_ms)));
    writer.local_cache_observe_write(refused).await?;
    assert_eq!(writer.local_cache_get(Did::from(RESOURCE)).await?, None);

    let inbox = inbox_key(Did::from(OTHER));
    let hold = EntryOperation::Extend(Entry::new(inbox, Vec::new(), EntryKind::RelayMessage));
    writer.local_cache_observe_write(hold).await?;
    assert_eq!(writer.cache.count().await?, 0);
    Ok(())
}
