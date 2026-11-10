//! Relay carriers live in a store of their own (#1021): a retained store keeps a completed hold
//! across a restart, the data store's budget never evicts held mail, and a full relay store
//! refuses the hold instead of evicting.

use super::super::super::chord::PeerRing;
use super::data_entry_with_payload_len;
use crate::consts::UNACKNOWLEDGED_RELAY_HOLDS_MAX;
use crate::delegation::DelegateeKey;
use crate::dht::entry::inbox::inbox_key;
use crate::dht::entry::EntryOperation;
use crate::dht::Did;
use crate::dht::RelayCustody;
use crate::dht::StorageKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Principal;
use crate::message::RelayHoldDigest;
use crate::storage::file::test_root::TempRoot;
use crate::storage::file::FileStorage;
use crate::storage::RecordAuthority;
use crate::tests::held_inbox_for;
use crate::utils::get_epoch_ms;

/// The relay store at `root`, opened as the daemon opens it: retained, under `capacity` bytes.
async fn retained(root: &TempRoot, capacity: u32) -> Result<FileStorage> {
    FileStorage::new_with_cap_path_and_authority(capacity, root, RecordAuthority::Retained).await
}

/// A ring standing alone, so it routes every position to itself and is the hold authority, with
/// `data` and `relay` as its two stores.
fn ring(holder: &DelegateeKey, data: FileStorage, relay: FileStorage) -> PeerRing {
    PeerRing::new_with_storage(holder.delegator_did(), 3, Box::new(data))
        .with_relay_replication(crate::tests::ungated_relay_replication())
        .with_relay_storage(Box::new(relay))
}

/// Hold one message for `destination` at `ring`.
async fn hold(ring: &PeerRing, holder: &DelegateeKey, destination: Did) -> Result<()> {
    let now_ms = get_epoch_ms();
    let hold =
        EntryOperation::Extend(held_inbox_for(destination, holder)?).stamped(now_ms, ring.did)?;
    ring.operate_storage_entry(
        now_ms,
        inbox_key(destination),
        hold,
        ring.did,
        Principal::Local,
    )
    .await
}

/// A completed hold survives a restart of the node, and the data store's budget, however full,
/// never evicts it.
#[tokio::test]
async fn test_held_mail_survives_a_restart_and_the_data_budget() -> Result<()> {
    let (data_root, relay_root) = (TempRoot::new("relay-data"), TempRoot::new("relay-inbox"));
    let holder = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let destination = Did::from(50u32);
    {
        let data = FileStorage::new_with_cap_and_path(4096, &data_root).await?;
        let node = ring(&holder, data, retained(&relay_root, 1 << 20).await?);
        hold(&node, &holder, destination).await?;
        let now_ms = get_epoch_ms();
        for topic in 0..8u32 {
            let topic = Did::from(1000 + topic);
            let park = EntryOperation::Overwrite(data_entry_with_payload_len(topic, 1024))
                .stamped(now_ms, node.did)?;
            node.operate_storage_entry(now_ms, topic, park, node.did, Principal::Local)
                .await?;
        }
        assert!(
            node.storage.count().await? < 8,
            "the data budget evicted topics"
        );
    }

    let data = FileStorage::new_with_cap_and_path(4096, &data_root).await?;
    let restarted = ring(&holder, data, retained(&relay_root, 1 << 20).await?);
    let held = restarted
        .live_storage_entry(StorageKey::inbox_of(destination), get_epoch_ms())
        .await?;
    assert_eq!(held.map(|carrier| carrier.data.len()), Some(1));
    Ok(())
}

/// A relay store with no room left refuses the hold, changing nothing, instead of evicting the
/// mail it already holds.
#[tokio::test]
async fn test_a_full_relay_store_refuses_the_hold() -> Result<()> {
    let (data_root, relay_root) = (TempRoot::new("relay-data"), TempRoot::new("relay-full"));
    let holder = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let data = FileStorage::new_with_cap_and_path(4096, &data_root).await?;
    let relay = retained(&relay_root, 1 << 20).await?;
    let node = ring(&holder, data, relay);
    let first = Did::from(50u32);
    hold(&node, &holder, first).await?;
    assert_eq!(
        node.relay_storage
            .as_ref()
            .ok_or(Error::RelayStorageUnavailable)?
            .count()
            .await?,
        1
    );
    drop(node);

    // Reopen the relay store with room for the one carrier it holds and no more.
    let tight = std::fs::read_dir(&*relay_root)?
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum::<u64>();
    let tight = u32::try_from(tight).map_err(|_| Error::InvalidCapacity)?;
    let data = FileStorage::new_with_cap_and_path(4096, &data_root).await?;
    let node = ring(&holder, data, retained(&relay_root, tight).await?);
    let second = Did::from(500u32);
    assert!(matches!(
        hold(&node, &holder, second).await,
        Err(Error::StorageBudgetExhausted { .. })
    ));
    let now_ms = get_epoch_ms();
    assert!(node
        .live_storage_entry(StorageKey::inbox_of(first), now_ms)
        .await?
        .is_some());
    assert!(node
        .live_storage_entry(StorageKey::inbox_of(second), now_ms)
        .await?
        .is_none());
    Ok(())
}

/// A node with no relay store takes custody of no relay carrier (Law 1 admits no volatile
/// relay store): it refuses a hold with `RelayStorageUnavailable`, storing nothing anywhere, and
/// finds no carrier to read.
#[tokio::test]
async fn test_a_node_without_a_relay_store_refuses_custody() -> Result<()> {
    let holder = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let node = PeerRing::new_with_storage(
        holder.delegator_did(),
        3,
        Box::new(crate::storage::MemStorage::new()),
    );
    let destination = Did::from(50u32);
    assert!(matches!(
        hold(&node, &holder, destination).await,
        Err(Error::RelayStorageUnavailable)
    ));
    assert_eq!(node.storage.count().await?, 0);
    assert!(node
        .live_storage_entry(StorageKey::inbox_of(destination), get_epoch_ms())
        .await?
        .is_none());
    Ok(())
}

/// The hold of one message for `destination` by `holder`, stamped by `ring`, with its digest.
fn stamped_hold(
    ring: &PeerRing,
    holder: &DelegateeKey,
    destination: Did,
) -> Result<(RelayHoldDigest, EntryOperation)> {
    let hold = EntryOperation::Extend(held_inbox_for(destination, holder)?)
        .stamped(get_epoch_ms(), ring.did)?;
    Ok((RelayHoldDigest::of(&hold)?, hold))
}

/// A kept hold is forgotten only on the acknowledgement of the node it was sent to, and only a
/// refusal from that node names it: an answer forged by any other peer changes nothing.
#[test]
fn test_only_the_target_of_a_kept_hold_answers_for_it() -> Result<()> {
    let holder = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let ring = PeerRing::new_with_storage(
        holder.delegator_did(),
        3,
        Box::new(crate::storage::MemStorage::new()),
    );
    let (target, forger) = (Did::from(7u32), Did::from(9u32));
    let (digest, hold) = stamped_hold(&ring, &holder, Did::from(50u32))?;
    ring.await_relay_hold(digest, hold.clone(), Some(target))?;

    assert!(!ring.acknowledge_relay_hold(digest, forger)?);
    assert_eq!(ring.refused_relay_hold(digest, forger)?, None);
    assert_eq!(ring.refused_relay_hold(digest, target)?, Some(hold.clone()));
    assert_eq!(ring.unacknowledged_relay_holds()?, vec![(digest, hold)]);
    assert!(ring.acknowledge_relay_hold(digest, target)?);
    assert!(ring.unacknowledged_relay_holds()?.is_empty());
    Ok(())
}

/// `Law (Acknowledged)` bound: a writer keeps at most `UNACKNOWLEDGED_RELAY_HOLDS_MAX` holds and
/// refuses another to its local sender, forgetting none it keeps.
#[test]
fn test_a_hold_beyond_the_bound_is_refused_and_none_kept_is_forgotten() -> Result<()> {
    let holder = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let ring = PeerRing::new_with_storage(
        holder.delegator_did(),
        3,
        Box::new(crate::storage::MemStorage::new()),
    );
    let mut kept = Vec::new();
    for destination in 0..UNACKNOWLEDGED_RELAY_HOLDS_MAX {
        let (digest, hold) = stamped_hold(&ring, &holder, Did::from(1000 + destination as u32))?;
        ring.await_relay_hold(digest, hold, None)?;
        kept.push(digest);
    }
    let (digest, hold) = stamped_hold(&ring, &holder, Did::from(50u32))?;
    assert!(matches!(
        ring.await_relay_hold(digest, hold, None),
        Err(Error::RelayHoldsUnacknowledgedFull)
    ));
    let mut still = ring
        .unacknowledged_relay_holds()?
        .into_iter()
        .map(|(digest, _)| digest)
        .collect::<Vec<_>>();
    still.sort();
    kept.sort();
    assert_eq!(still, kept);
    Ok(())
}

/// `Law (Asked)`: an owner's answer is taken only for a key with a query outstanding, once, and
/// only as another node that holds relay mail; a hint change alone costs a repair pass. So a peer
/// can neither name a holder unasked nor overwrite a hint by alternating answers.
#[test]
fn test_an_owner_answer_is_taken_once_for_an_outstanding_query_alone() -> Result<()> {
    let holder = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let ring = PeerRing::new_with_storage(
        holder.delegator_did(),
        3,
        Box::new(crate::storage::MemStorage::new()),
    );
    let (key, named, other, storage_less) = (
        Did::from(51u32),
        Did::from(7u32),
        Did::from(8u32),
        Did::from(9u32),
    );
    ring.observe_relay_custody(storage_less, RelayCustody::HoldsNone)?;

    // Unasked: nothing is taken.
    let wrong_token = uuid::Uuid::from_u128(404);
    assert!(!ring.name_relay_holder(key, wrong_token, named, Some(named), None)?);
    assert!(ring.relay_holder_hints()?.is_empty());

    // Asked: a retry while the query is still outstanding reuses the live token so an in-flight
    // multi-hop response from the previous pass is not invalidated; a mismatched token or an
    // inconsistent origin is rejected without consuming the outstanding query, while the matching
    // token from a consistent origin is taken once.
    let token = ring.ask_relay_holder(key)?;
    let retry_token = ring.ask_relay_holder(key)?;
    assert_eq!(retry_token, token);
    assert!(!ring.name_relay_holder(key, wrong_token, named, Some(named), None)?);
    // `key` (51) is outside `(60, 70]` and `70 != 60`, so `origin = 60` cannot claim `70`.
    assert!(!ring.name_relay_holder(key, token, Did::from(60u32), Some(Did::from(70u32)), None,)?);
    assert!(ring.name_relay_holder(key, token, named, Some(named), None)?);
    assert!(!ring.name_relay_holder(key, token, other, Some(other), None)?);
    assert_eq!(ring.relay_holder_hints()?.get(&key), Some(&named));

    // Asked again: the same answer changes nothing and costs no pass.
    let token = ring.ask_relay_holder(key)?;
    assert!(!ring.name_relay_holder(key, token, named, Some(named), None)?);
    assert_eq!(ring.relay_holder_hints()?.get(&key), Some(&named));

    // A storage-less owner `storage_less` (9) may name a successor holder `other` (60) when
    // `key` (51) lies in `(storage_less, other]`.
    let other_holder = Did::from(60u32);
    let token = ring.ask_relay_holder(key)?;
    assert!(ring.name_relay_holder(key, token, storage_less, Some(other_holder), None)?);
    assert_eq!(ring.relay_holder_hints()?.get(&key), Some(&other_holder));

    // An answer naming a node that holds no relay mail names no holder.
    let token = ring.ask_relay_holder(key)?;
    assert!(ring.name_relay_holder(key, token, storage_less, Some(storage_less), None)?);
    assert!(ring.relay_holder_hints()?.is_empty());
    let token = ring.ask_relay_holder(key)?;
    assert!(ring.name_relay_holder(key, token, named, Some(named), None)?);

    // Horizon stepping (`#1221` Item 5): when an owner answers `(None, Some(horizon))` for an
    // unplaced `carrier_key`, `ask_relay_holder_query(carrier_key)` advances to `horizon + 1`
    // and records the subsequent owner's holder under the original `carrier_key`.
    let horizon = Did::from(70u32);
    let (query_key_1, token_1) = ring.ask_relay_holder_query(key)?;
    assert_eq!(query_key_1, key);
    assert!(ring.name_relay_holder(query_key_1, token_1, storage_less, None, Some(horizon))?);
    assert!(ring.relay_holder_hints()?.is_empty());
    let (query_key_2, token_2) = ring.ask_relay_holder_query(key)?;
    assert_eq!(query_key_2, horizon + Did::from(1u32));
    assert_eq!(ring.relay_holder_query_key(key)?, query_key_2);
    let resolved_holder = Did::from(80u32);
    assert!(ring.name_relay_holder(query_key_2, token_2, horizon, Some(resolved_holder), None)?);
    assert_eq!(ring.relay_holder_hints()?.get(&key), Some(&resolved_holder));

    // When a stepped query key lands inside `(local, head]` (`find_successor == Some`),
    // `reset_relay_holder_query` clears `stepped` and `asked` so the next query falls back to
    // `carrier_key`.
    let (query_key_3, token_3) = ring.ask_relay_holder_query(key)?;
    assert_eq!(query_key_3, key);
    assert!(ring.name_relay_holder(query_key_3, token_3, storage_less, None, Some(horizon))?);
    assert_eq!(ring.relay_holder_query_key(key)?, horizon + Did::from(1u32));
    ring.reset_relay_holder_query(key)?;
    assert_eq!(ring.relay_holder_query_key(key)?, key);
    Ok(())
}

/// `Law (Asked)` on `NextHolder` (#1181): `take_next_relay_holder` rejects unsolicited,
/// mismatched-token, replayed, and inconsistent-origin `RelayHolderFound` responses without
/// retiring the in-flight query token.
#[test]
fn test_next_relay_holder_requires_matching_token_and_consistent_origin() -> Result<()> {
    let holder = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let destination = Did::from(15u32);
    let held_key = inbox_key(destination);
    let predecessor = held_key - Did::from(10u32);
    let local = held_key + Did::from(10u32);
    let successor = held_key + Did::from(100u32);
    let ring = PeerRing::new_with_storage(local, 3, Box::new(crate::storage::MemStorage::new()))
        .with_relay_replication(crate::tests::ungated_relay_replication())
        .with_relay_storage(Box::new(crate::storage::MemStorage::new()));
    ring.admit_connected(successor, None)?;
    ring.observe_relay_custody(successor, RelayCustody::HoldsNone)?;
    let _ = crate::dht::Chord::notify(&ring, predecessor)?;
    assert!(ring.accept_previous_relay_holder(predecessor, vec![
        crate::dht::relay_holder::PreviousHolder {
            holder: predecessor,
            run_start: local,
        }
    ],)?);

    let (digest, hold) = stamped_hold(&ring, &holder, destination)?;
    let crate::dht::ReplicationStep::Ask { key, request_id } =
        ring.replicate_relay_hold(digest, hold, ring.did)?
    else {
        return Err(Error::InvalidMessage(
            "expected ReplicationStep::Ask beyond successor view".to_string(),
        ));
    };
    assert_eq!(key, successor + Did::from(1u32));
    assert_eq!(
        ring.outstanding_next_relay_holder_query()?,
        Some((key, request_id))
    );

    let wrong_token = uuid::Uuid::from_u128(999);
    let resolved_holder = successor + Did::from(200u32);
    // `key = successor + 1` lies outside `(inconsistent_origin, resolved_holder]`.
    let inconsistent_origin = successor + Did::from(50u32);

    // 1. Mismatched token is ignored and leaves the query outstanding.
    assert!(!ring.asks_next_relay_holder(key, wrong_token)?);
    assert_eq!(
        ring.take_next_relay_holder(
            key,
            wrong_token,
            resolved_holder,
            Some(resolved_holder),
            None
        )?,
        None
    );
    assert!(ring.asks_next_relay_holder(key, request_id)?);

    // 2. Inconsistent origin (a storage-holding node naming a different holder, or a horizon
    // that does not cover `key ∈ (origin, horizon]`) is rejected without retiring `request_id`.
    assert_eq!(
        ring.take_next_relay_holder(
            key,
            request_id,
            inconsistent_origin,
            Some(resolved_holder),
            None,
        )?,
        None
    );
    assert_eq!(
        ring.take_next_relay_holder(
            key,
            request_id,
            successor + Did::from(100u32),
            None,
            Some(successor + Did::from(150u32)),
        )?,
        None
    );
    assert!(ring.asks_next_relay_holder(key, request_id)?);

    // 3. Valid horizon response with `key ∈ (origin, horizon]` advances the query to
    // `horizon + 1` with a fresh single-use token.
    let horizon = successor + Did::from(100u32);
    let Some((next_key, next_request_id)) =
        ring.take_next_relay_holder(key, request_id, successor, None, Some(horizon))?
    else {
        return Err(Error::InvalidMessage(
            "expected horizon response to advance next-holder query".to_string(),
        ));
    };
    assert_eq!(next_key, horizon + Did::from(1u32));
    assert_ne!(next_request_id, request_id);
    assert!(!ring.asks_next_relay_holder(key, request_id)?);
    assert!(ring.asks_next_relay_holder(next_key, next_request_id)?);

    // 4. Valid holder response from `horizon` (which holds relay mail, `holds(horizon) == true`)
    // for `next_key = horizon + 1` naming `resolved_holder != horizon` with
    // `next_key ∈ (horizon, resolved_holder]` resolves the next holder and consumes
    // `next_request_id` once.
    assert_eq!(
        ring.take_next_relay_holder(
            next_key,
            next_request_id,
            horizon,
            Some(resolved_holder),
            None,
        )?,
        None
    );
    assert_eq!(ring.outstanding_next_relay_holder_query()?, None);
    assert_eq!(
        ring.relay_chain_neighbours()?,
        (Some(predecessor), Some(resolved_holder))
    );
    Ok(())
}

/// On a capacity-1 ring `H0(100) -> X(200, HoldsNone) -> H1(300, suspected) -> H2(400)`, next-holder
/// discovery advances strictly forward past suspected `H1(300)` without cycling `300 <-> 301` and
/// resolves `H2(400)` (#1182).
#[test]
fn test_next_relay_holder_advances_past_suspected_holder_with_single_successor() -> Result<()> {
    let replication = crate::tests::ungated_relay_replication();
    let (h0_id, x_id, h1_id, h2_id) = (
        Did::from(100u32),
        Did::from(200u32),
        Did::from(300u32),
        Did::from(400u32),
    );
    let h0 = PeerRing::new_with_storage(h0_id, 1, Box::new(crate::storage::MemStorage::new()))
        .with_relay_replication(replication)
        .with_relay_storage(Box::new(crate::storage::MemStorage::new()));
    let x = PeerRing::new_with_storage(x_id, 1, Box::new(crate::storage::MemStorage::new()))
        .with_relay_replication(replication);
    let h1 = PeerRing::new_with_storage(h1_id, 1, Box::new(crate::storage::MemStorage::new()))
        .with_relay_replication(replication)
        .with_relay_storage(Box::new(crate::storage::MemStorage::new()));
    let h2 = PeerRing::new_with_storage(h2_id, 1, Box::new(crate::storage::MemStorage::new()))
        .with_relay_replication(replication)
        .with_relay_storage(Box::new(crate::storage::MemStorage::new()));

    h0.admit_connected(x_id, None)?;
    let _ = crate::dht::Chord::notify(&h0, h2_id)?;
    x.admit_connected(h1_id, None)?;
    let _ = crate::dht::Chord::notify(&x, h0_id)?;
    h1.admit_connected(h2_id, None)?;
    let _ = crate::dht::Chord::notify(&h1, x_id)?;
    h2.admit_connected(h0_id, None)?;
    let _ = crate::dht::Chord::notify(&h2, h1_id)?;

    for ring in [&h0, &x, &h1, &h2] {
        ring.observe_relay_custody(x_id, RelayCustody::HoldsNone)?;
    }
    h0.suspect_relay_chain_neighbour(h1_id)?;

    let (key0, token0) = h0
        .outstanding_next_relay_holder_query()?
        .ok_or_else(|| Error::InvalidMessage("expected initial query at 201".to_string()))?;
    assert_eq!(key0, Did::from(201u32));

    // Even if X(200) answers Some(300) before seeing H0's exclusion set, H0 advances directly to
    // 301 (never cycling back to 300), and H1(300) answers Some(400) under H0's exclusion set.
    let (key1, token1) = h0
        .take_next_relay_holder(key0, token0, x_id, Some(h1_id), None)?
        .ok_or_else(|| Error::InvalidMessage("expected query to advance past 300".to_string()))?;
    assert_eq!(key1, Did::from(301u32));

    let excluded = h0.relay_excluded_holders()?;
    assert!(excluded.contains(&h1_id));
    let (holder, horizon) = h1.answer_relay_holder_query(key1, &excluded)?;
    assert_eq!((holder, horizon), (Some(h2_id), None));
    assert_eq!(
        h0.take_next_relay_holder(key1, token1, h1_id, holder, horizon)?,
        None
    );
    assert_eq!(h0.remote_relay_chain_neighbours()?.1, Some(h2_id));

    // Once two on-time probe answers clear H1(300)'s suspicion, H0 invalidates its cached fallback
    // holder H2(400) and re-queries at 201 to restore H1(300) as its immediate relay successor.
    h0.observe_chain_neighbour_answer(h1_id, true)?;
    h0.observe_chain_neighbour_answer(h1_id, true)?;
    assert_eq!(h0.remote_relay_chain_neighbours()?.1, None);
    let (recovery_key, recovery_token) = h0
        .outstanding_next_relay_holder_query()?
        .ok_or_else(|| Error::InvalidMessage("expected recovery query at 201".to_string()))?;
    assert_eq!(recovery_key, Did::from(201u32));
    assert_eq!(
        h0.take_next_relay_holder(recovery_key, recovery_token, x_id, Some(h1_id), None)?,
        None
    );
    assert_eq!(h0.remote_relay_chain_neighbours()?.1, Some(h1_id));

    // Custody declarations from an unrelated peer 500 beyond confirmed H1(300) neither discard
    // H1(300) nor cancel an in-flight next-holder query token.
    let unrelated = Did::from(500u32);
    h0.observe_relay_custody(unrelated, RelayCustody::HoldsNone)?;
    h0.observe_relay_custody(unrelated, RelayCustody::Holds)?;
    assert_eq!(h0.remote_relay_chain_neighbours()?.1, Some(h1_id));

    h0.forget_next_relay_holder(None)?;
    let (ask_key, ask_token) = h0
        .outstanding_next_relay_holder_query()?
        .ok_or_else(|| Error::InvalidMessage("expected query at 201".to_string()))?;
    for _ in 0..3 {
        h0.observe_relay_custody(unrelated, RelayCustody::HoldsNone)?;
        h0.observe_relay_custody(unrelated, RelayCustody::Holds)?;
    }
    assert_eq!(
        h0.take_next_relay_holder(ask_key, ask_token, x_id, Some(h1_id), None)?,
        None
    );
    assert_eq!(h0.remote_relay_chain_neighbours()?.1, Some(h1_id));
    Ok(())
}
