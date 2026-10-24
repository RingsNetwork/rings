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
        .with_relay_storage(Box::new(relay))
}

/// Hold one message for `destination` at `ring`.
async fn hold(ring: &PeerRing, holder: &DelegateeKey, destination: Did) -> Result<()> {
    let now_ms = get_epoch_ms();
    let hold =
        EntryOperation::Extend(held_inbox_for(destination, holder)?).stamped(now_ms, ring.did)?;
    ring.operate_storage_entry(now_ms, inbox_key(destination), hold, ring.did)
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
            node.operate_storage_entry(now_ms, topic, park, node.did)
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
    assert!(!ring.name_relay_holder(key, Some(named))?);
    assert!(ring.relay_holder_hints()?.is_empty());

    // Asked: the first answer is taken, and a second answer to the same query is not.
    ring.ask_relay_holder(key)?;
    assert!(ring.name_relay_holder(key, Some(named))?);
    assert!(!ring.name_relay_holder(key, Some(other))?);
    assert_eq!(ring.relay_holder_hints()?.get(&key), Some(&named));

    // Asked again: the same answer changes nothing and costs no pass.
    ring.ask_relay_holder(key)?;
    assert!(!ring.name_relay_holder(key, Some(named))?);
    assert_eq!(ring.relay_holder_hints()?.get(&key), Some(&named));

    // An answer naming this node or a node that holds no relay mail names no holder.
    for unfit in [ring.did, storage_less] {
        ring.ask_relay_holder(key)?;
        assert!(ring.name_relay_holder(key, Some(unfit))?);
        assert!(ring.relay_holder_hints()?.is_empty());
        ring.ask_relay_holder(key)?;
        assert!(ring.name_relay_holder(key, Some(named))?);
    }
    Ok(())
}
