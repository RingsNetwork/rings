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

    // Asked: a mismatched token or an inconsistent origin is rejected without consuming the
    // outstanding query, while the matching token from a consistent origin is taken once.
    let token = ring.ask_relay_holder(key)?;
    assert!(!ring.name_relay_holder(key, wrong_token, named, Some(named), None)?);
    // `other` holds relay mail by default, so it cannot claim `named != other` as the holder.
    assert!(!ring.name_relay_holder(key, token, other, Some(named), None)?);
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
    let inconsistent_origin = successor + Did::from(300u32);

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

    // 4. Valid holder response resolves the next holder and consumes `next_request_id` once.
    assert_eq!(
        ring.take_next_relay_holder(
            next_key,
            next_request_id,
            resolved_holder,
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
