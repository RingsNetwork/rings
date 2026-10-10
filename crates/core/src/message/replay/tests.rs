use std::collections::BTreeMap;

use super::*;
use crate::ecc::SecretKey;
#[cfg(not(target_family = "wasm"))]
use crate::storage::KvStorageInterface;

fn digest(value: u8) -> TransactionDigest {
    TransactionDigest::new([value; 32])
}

#[cfg(not(target_family = "wasm"))]
fn stream(destination: Did) -> StreamKey {
    StreamKey::new(
        7,
        SecretKey::random().address().into(),
        destination,
        MessageCategory::Application,
    )
}

#[test]
fn test_duplicate_late_stale_fork_and_gap_transitions_are_typed() {
    let (state, first) = observe(None, 10, digest(1));
    assert_eq!(first, SequenceVerdict::First);

    let (state, advanced) = observe(Some(state), 12, digest(2));
    assert_eq!(advanced, SequenceVerdict::Advance);

    let (state, late) = observe(Some(state), 11, digest(3));
    assert_eq!(late, SequenceVerdict::Late);

    let (state, replay) = observe(Some(state), 11, digest(3));
    assert_eq!(replay, SequenceVerdict::Replay);

    let (state, fork) = observe(Some(state), 11, digest(4));
    assert_eq!(fork, SequenceVerdict::Fork {
        accepted: digest(3),
        incoming: digest(4),
    });

    let (state, gap) = observe(Some(state), 100, digest(5));
    assert_eq!(gap, SequenceVerdict::Advance);
    let (_, stale) = observe(Some(state), 68, digest(6));
    assert_eq!(stale, SequenceVerdict::Stale { retained_min: 69 });
}

/// One transaction as the receiver sees it: its class and its per-class and shared
/// sequence numbers.
#[derive(Clone, Copy)]
struct Sent {
    /// Traffic class of the transaction.
    class: MessageCategory,
    /// Its sequence in its class's stream (#898).
    class_sequence: u64,
    /// Its sequence in a stream shared by every class (before #898).
    shared_sequence: u64,
}

/// A deterministic 64-bit xorshift step.
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// One arrival order of an honest sender's traffic, from a fixed `seed`.
///
/// The sender signs each class's transactions in sequence order, spread over four classes
/// and one shared counter. The scheduler then serves the class lanes in an arbitrary
/// interleaving: each lane stays FIFO up to its in-flight window, whose frames may still
/// arrive in any order (the lane window is below the replay window), and the lanes are
/// merged in any order, which lets one lane fall far behind the others. The in-window
/// shuffle is conservative: a lane pinned to one ordered channel delivers in order.
fn arrivals(seed: u64, per_class: u64, lane_window: usize) -> Vec<Sent> {
    let class_count = u64::try_from(CLASSES.len()).expect("four classes");
    let mut state = seed | 1;
    let mut lanes: Vec<Vec<Sent>> = vec![Vec::new(); CLASSES.len()];
    let mut class_next = [0_u64; MessageCategory::COUNT];
    for shared_sequence in 0..per_class * class_count {
        let lane = usize::try_from(xorshift(&mut state) % class_count).expect("small index");
        let class_sequence = class_next[lane];
        class_next[lane] += 1;
        lanes[lane].push(Sent {
            class: CLASSES[lane],
            class_sequence,
            shared_sequence,
        });
    }
    for lane in lanes.iter_mut() {
        for window in lane.chunks_mut(lane_window) {
            for index in (1..window.len()).rev() {
                let bound = u64::try_from(index + 1).expect("index");
                let other = usize::try_from(xorshift(&mut state) % bound).expect("index");
                window.swap(index, other);
            }
        }
    }
    let mut cursors = [0_usize; MessageCategory::COUNT];
    let mut merged = Vec::new();
    while cursors
        .iter()
        .zip(lanes.iter())
        .any(|(cursor, lane)| *cursor < lane.len())
    {
        // Favour one lane in long bursts so the others build a backlog behind it.
        let lane = usize::try_from(xorshift(&mut state) % class_count).expect("small index");
        let burst = xorshift(&mut state) % 64;
        for _ in 0..burst {
            if let Some(sent) = lanes[lane].get(cursors[lane]) {
                merged.push(*sent);
                cursors[lane] += 1;
            }
        }
    }
    merged
}

/// Law of #898: with one stream per `(origin, destination, class)`, an honest sender's
/// transactions are never rejected as stale, however the class lanes are interleaved. The
/// same arrivals keyed by one shared stream are rejected, which is the defect removed.
#[test]
fn test_honest_sender_is_never_stale_under_any_cross_class_interleaving() {
    let lane_window = 8;
    let mut shared_stale = 0_usize;
    for seed in 0..200_u64 {
        let mut per_class: BTreeMap<MessageCategory, SequenceState> = BTreeMap::new();
        let mut shared: Option<SequenceState> = None;
        for (index, sent) in arrivals(seed, 100, lane_window).into_iter().enumerate() {
            let tag = u8::try_from(index % 251).expect("small tag");
            let (next, verdict) = observe(
                per_class.remove(&sent.class),
                sent.class_sequence,
                digest(tag),
            );
            assert!(
                verdict.permits_dispatch(),
                "seed {seed}: {verdict:?} for class sequence {}",
                sent.class_sequence
            );
            per_class.insert(sent.class, next);

            let (next, verdict) = observe(shared.take(), sent.shared_sequence, digest(tag));
            shared_stale += usize::from(matches!(verdict, SequenceVerdict::Stale { .. }));
            shared = Some(next);
        }
    }
    assert!(
        shared_stale > 0,
        "the shared stream must exhibit the #898 defect"
    );
}

#[test]
fn test_transition_is_deterministic_for_the_same_state_and_input() {
    let (state, _) = observe(None, 4, digest(1));
    assert_eq!(
        observe(Some(state.clone()), 7, digest(2)),
        observe(Some(state), 7, digest(2))
    );
}

#[test]
fn test_destinations_advance_independently() {
    let origin: Did = SecretKey::random().address().into();
    let a: Did = SecretKey::random().address().into();
    let b: Did = SecretKey::random().address().into();
    let mut states = BTreeMap::new();
    for (destination, sequence) in [(a, 8), (b, 0), (a, 10), (b, 1)] {
        let key = StreamKey::new(1, origin, destination, MessageCategory::Application);
        let previous = states.remove(&key);
        let (next, verdict) = observe(previous, sequence, digest(sequence as u8));
        assert!(verdict.permits_dispatch());
        states.insert(key, next);
    }
    assert_eq!(
        states
            .get(&StreamKey::new(1, origin, a, MessageCategory::Application))
            .map(SequenceState::high),
        Some(10)
    );
    assert_eq!(
        states
            .get(&StreamKey::new(1, origin, b, MessageCategory::Application))
            .map(SequenceState::high),
        Some(1)
    );
}

#[test]
fn test_every_slot_in_the_bounded_reordering_window_is_admitted_once() {
    let high = TRANSACTION_REPLAY_BACKTRACK.saturating_add(50);
    let (mut state, verdict) = observe(None, high, digest(0));
    assert_eq!(verdict, SequenceVerdict::First);
    for sequence in state.retained_min()..high {
        let (next, verdict) = observe(Some(state), sequence, digest(sequence as u8));
        assert_eq!(verdict, SequenceVerdict::Late);
        state = next;
    }
    let below = state.retained_min().saturating_sub(1);
    let (_, verdict) = observe(Some(state), below, digest(255));
    assert_eq!(verdict, SequenceVerdict::Stale {
        retained_min: below.saturating_add(1),
    });
}

#[test]
fn test_session_rotation_preserves_the_account_destination_stream_key() -> Result<()> {
    let account = SecretKey::random();
    let first_session = crate::delegation::DelegateeKey::new_with_seckey(&account)?;
    let rotated_session = crate::delegation::DelegateeKey::new_with_seckey(&account)?;
    let destination: Did = SecretKey::random().address().into();
    let first = crate::message::Transaction::new(
        destination,
        uuid::Uuid::new_v4(),
        0,
        None,
        crate::message::Message::custom(b"first")?,
        crate::message::MessageSigner::new(&first_session, 7),
    )?;
    let rotated = crate::message::Transaction::new(
        destination,
        uuid::Uuid::new_v4(),
        1,
        None,
        crate::message::Message::custom(b"rotated")?,
        crate::message::MessageSigner::new(&rotated_session, 7),
    )?;

    assert_ne!(
        first_session.delegation().delegatee_did(),
        rotated_session.delegation().delegatee_did()
    );
    assert_eq!(first.stream_key(7)?, rotated.stream_key(7)?);
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn test_sender_allocators_are_independent_per_destination() -> Result<()> {
    let origin: Did = SecretKey::random().address().into();
    let a: Did = SecretKey::random().address().into();
    let b: Did = SecretKey::random().address().into();
    let runtime = TransactionReplay::new_shared(Box::new(crate::storage::MemStorage::new()));
    let key_a = StreamKey::new(1, origin, a, MessageCategory::Application);
    let key_b = StreamKey::new(1, origin, b, MessageCategory::Application);

    assert_eq!(runtime.reserve(key_a, NonZeroU64::MIN).await?, 0..=0);
    assert_eq!(runtime.reserve(key_b, NonZeroU64::MIN).await?, 0..=0);
    assert_eq!(runtime.reserve(key_a, NonZeroU64::MIN).await?, 1..=1);
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn test_sender_and_receiver_state_survive_runtime_recreation() -> Result<()> {
    let destination: Did = SecretKey::random().address().into();
    let key = stream(destination);
    let storage = std::sync::Arc::new(crate::storage::MemStorage::new());
    let first_runtime = TransactionReplay::new_shared(Box::new(storage.clone()));
    assert_eq!(first_runtime.reserve(key, NonZeroU64::MIN).await?, 0..=0);
    assert_eq!(
        first_runtime.admit(key, 0, digest(1)).await?,
        SequenceVerdict::First
    );
    drop(first_runtime);

    let restarted = TransactionReplay::new_shared(Box::new(storage));
    assert_eq!(restarted.reserve(key, NonZeroU64::MIN).await?, 1..=1);
    assert!(matches!(
        restarted.admit(key, 0, digest(1)).await,
        Err(Error::TransactionReplay { .. })
    ));
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn test_counter_exhaustion_fails_closed() -> Result<()> {
    let key = stream(SecretKey::random().address().into());
    let storage = crate::storage::MemStorage::new();
    let (storage_key, record) = sender_record(&key, u64::MAX)?;
    storage.put(storage_key.as_str(), &record).await?;
    let runtime = TransactionReplay::new_shared(Box::new(storage));
    assert!(matches!(
        runtime.reserve(key, NonZeroU64::MIN).await,
        Err(Error::TransactionSequenceExhausted { .. })
    ));
    Ok(())
}

/// The four classes of a class-keyed stream, in lane order.
const CLASSES: [MessageCategory; MessageCategory::COUNT] = [
    MessageCategory::DhtControl,
    MessageCategory::Storage,
    MessageCategory::E2e,
    MessageCategory::Application,
];

/// The table retains every class stream of 4096 pairs; the next pair's stream fails closed.
#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn test_new_sender_stream_fails_closed_at_the_table_bound() -> Result<()> {
    let pairs = u32::try_from(TRANSACTION_REPLAY_PAIR_CAPACITY)
        .map_err(|_| Error::TransactionReplayStateInvalid)?;
    let runtime = TransactionReplay::new_shared(Box::new(crate::storage::MemStorage::new()));
    let destination = Did::from(u32::MAX);
    {
        let mut state = runtime.state.lock().await;
        let store = runtime.load_store(&mut state.store).await?;
        for origin in 0..pairs {
            for class in CLASSES {
                let key = StreamKey::new(1, Did::from(origin), destination, class);
                store.tables.sender.insert(key, 0);
            }
        }
        assert_eq!(
            store.tables.sender.len(),
            TRANSACTION_REPLAY_STREAM_CAPACITY
        );
    }
    let new_key = StreamKey::new(
        1,
        Did::from(pairs),
        destination,
        MessageCategory::Application,
    );
    assert!(matches!(
        runtime.reserve(new_key, NonZeroU64::MIN).await,
        Err(Error::TransactionReplayStreamCapacityExceeded {
            capacity: TRANSACTION_REPLAY_STREAM_CAPACITY
        })
    ));
    Ok(())
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
#[wasm_bindgen_test::wasm_bindgen_test]
async fn test_browser_storage_round_trip_retains_nonempty_replay_state() {
    const STORAGE_NAME: &str = "rings-core/replay-store-round-trip";
    let storage = crate::storage::idb::IdbStorage::new_with_cap_name_and_authority(
        2,
        STORAGE_NAME,
        crate::storage::RecordAuthority::Authoritative,
    )
    .await
    .expect("IndexedDB opens");
    storage.clear().await.expect("IndexedDB clears");
    let origin: Did = SecretKey::random().address().into();
    let destination: Did = SecretKey::random().address().into();
    let key = StreamKey::new(7, origin, destination, MessageCategory::Application);
    let first = TransactionReplay::new_shared(Box::new(storage));

    assert_eq!(
        first
            .reserve(key, NonZeroU64::MIN)
            .await
            .expect("sender reservation persists"),
        0..=0
    );
    assert_eq!(
        first
            .admit(key, u64::MAX, digest(1))
            .await
            .expect("receiver state persists"),
        SequenceVerdict::First
    );
    drop(first);

    let reopened = crate::storage::idb::IdbStorage::new_with_cap_name_and_authority(
        2,
        STORAGE_NAME,
        crate::storage::RecordAuthority::Authoritative,
    )
    .await
    .expect("IndexedDB reopens");
    let restarted = TransactionReplay::new_shared(Box::new(reopened));
    assert_eq!(
        restarted
            .reserve(key, NonZeroU64::MIN)
            .await
            .expect("sender state reloads"),
        1..=1
    );
    assert!(matches!(
        restarted.admit(key, u64::MAX, digest(1)).await,
        Err(Error::TransactionReplay { .. })
    ));
}
