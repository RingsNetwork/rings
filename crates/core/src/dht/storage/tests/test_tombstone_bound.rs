//! The tombstone bound of a data carrier (#921; removal-stability paper, Law 5 and Prop. 4): a
//! carrier holds at most `DATA_TOMBSTONE_MAX_LEN` unstable removes, a delta past it is
//! inadmissible, a join or operation past it is refused and changes nothing, and a carrier at
//! every bound encodes in exactly `DATA_CARRIER_WIRE_MAX` bytes.

use bytes::Bytes;

use super::super::super::chord::PeerRing;
use crate::consts::DATA_CARRIER_WIRE_MAX;
use crate::consts::DATA_TOMBSTONE_MAX_LEN;
use crate::consts::ENTRY_CARRIER_HEADER_WIRE_MAX;
use crate::consts::ENTRY_DATA_MAX_LEN;
use crate::consts::ENTRY_DOT_WIRE_MAX;
use crate::consts::ENTRY_ELEMENT_PREFIX_WIRE_MAX;
use crate::consts::ENTRY_PAYLOAD_MAX_BYTES;
use crate::consts::ENTRY_REGISTER_WIRE_MAX;
use crate::consts::ENTRY_TOMBSTONE_WIRE_MAX;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::entry::ElementDigest;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryCrdt;
use crate::dht::entry::EntryDot;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::EntryTombstone;
use crate::dht::entry::EntryVersion;
use crate::dht::Did;
use crate::dht::StorageKey;
use crate::error::Error;
use crate::error::Result;
use crate::storage::MemStorage;
use crate::tests::removes_only_carrier;
use crate::tests::stamped_operation;
use crate::utils::get_epoch_ms;

/// The widest version: every field at its widest encoding.
fn widest_version() -> EntryVersion {
    EntryVersion::new(u128::MAX, Did::from(u32::MAX), Did::from(u32::MAX))
}

/// The widest dot.
fn widest_dot() -> EntryDot {
    EntryDot {
        version: widest_version(),
        index: u32::MAX,
    }
}

/// The encoded size of `value`.
fn wire_size<T: serde::Serialize>(value: &T) -> Result<usize> {
    let size = rings_codec::serialized_size(value).map_err(Error::CodecSerialize)?;
    usize::try_from(size).map_err(|_| Error::MessageSizeOverflow)
}

/// The suprema of `consts` are the encodings of the widest values, with equality, and a carrier
/// at every bound encodes in exactly `DATA_CARRIER_WIRE_MAX` bytes.
#[test]
fn test_carrier_wire_suprema_are_the_widest_encodings() -> Result<()> {
    let tombstone = EntryTombstone {
        element: ElementDigest([0xff; 32]),
        dot: widest_dot(),
    };
    assert_eq!(wire_size(&widest_dot())?, ENTRY_DOT_WIRE_MAX);
    assert_eq!(wire_size(&tombstone)?, ENTRY_TOMBSTONE_WIRE_MAX);
    assert_eq!(wire_size(&Some(widest_version()))?, ENTRY_REGISTER_WIRE_MAX);
    let element = Bytes::from(vec![0u8; ENTRY_PAYLOAD_MAX_BYTES]);
    assert_eq!(
        wire_size(&element)?,
        ENTRY_PAYLOAD_MAX_BYTES + ENTRY_ELEMENT_PREFIX_WIRE_MAX
    );

    let full = Entry {
        did: Did::from(u32::MAX),
        data: (0..ENTRY_DATA_MAX_LEN)
            .map(|index| {
                let mut bytes = vec![0u8; ENTRY_PAYLOAD_MAX_BYTES];
                bytes[..8].copy_from_slice(&index.to_be_bytes());
                Bytes::from(bytes)
            })
            .collect(),
        kind: EntryKind::Data,
        crdt: EntryCrdt {
            register: Some(widest_version()),
            dots: vec![widest_dot(); ENTRY_DATA_MAX_LEN],
            tombstones: vec![tombstone; DATA_TOMBSTONE_MAX_LEN],
        },
        expires_at_ms: Some(u128::MAX),
    };
    let header = Entry {
        data: Vec::new(),
        crdt: EntryCrdt::default(),
        ..full.clone()
    };
    // The header's three empty vectors and its absent register encode in one byte each; at the
    // bounds each vector prefix takes two bytes, and the register, tag included, is counted on
    // its own.
    assert_eq!(wire_size(&header)?, ENTRY_CARRIER_HEADER_WIRE_MAX - 3 + 1);
    assert_eq!(wire_size(&full)?, DATA_CARRIER_WIRE_MAX);
    Ok(())
}

/// The data carrier stored at `did` on `node`.
async fn stored(node: &PeerRing, did: Did, now_ms: u128) -> Result<Option<Entry>> {
    node.live_storage_entry(StorageKey::new(EntryKind::Data, did), now_ms)
        .await
}

/// A delta carrying more removes than the bound is inadmissible, whatever the slot holds.
#[tokio::test]
async fn test_a_delta_past_the_tombstone_bound_is_inadmissible() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let did = Did::from(10u32);
    let now_ms = get_epoch_ms();
    let forged = removes_only_carrier(did, DATA_TOMBSTONE_MAX_LEN + 1, now_ms, "forged")?;

    assert!(matches!(
        node.join_storage_entry(now_ms, did, forged).await,
        Err(Error::EntryTombstonesExceedMax { count, bound })
            if count == DATA_TOMBSTONE_MAX_LEN + 1 && bound == DATA_TOMBSTONE_MAX_LEN
    ));
    assert_eq!(stored(&node, did, now_ms).await?, None);
    Ok(())
}

/// A sync delta that fills the bound is admitted; one more remove, though admissible on its
/// own, is refused at the join, and the slot keeps what it held.
#[tokio::test]
async fn test_a_join_past_the_tombstone_bound_is_refused_and_changes_nothing() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let did = Did::from(10u32);
    let now_ms = get_epoch_ms();
    node.join_storage_entry(
        now_ms,
        did,
        removes_only_carrier(did, DATA_TOMBSTONE_MAX_LEN, now_ms, "full")?,
    )
    .await?;
    let before = stored(&node, did, now_ms).await?;
    assert_eq!(
        before.as_ref().map(|entry| entry.crdt.tombstones.len()),
        Some(DATA_TOMBSTONE_MAX_LEN)
    );

    assert!(matches!(
        node.join_storage_entry(
            now_ms,
            did,
            removes_only_carrier(did, 1, now_ms, "one-more")?
        )
        .await,
        Err(Error::EntryTombstonesExceedMax { .. })
    ));
    assert_eq!(stored(&node, did, now_ms).await?, before);
    Ok(())
}

/// A writer's remove past the bound is refused and removes nothing; once the held removes are
/// stable, `H + σ` after their dots, they retire and the same remove is applied.
#[tokio::test]
async fn test_a_remove_past_the_bound_is_refused_until_the_held_removes_are_stable() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let writer = Did::from(7u32);
    let now_ms = get_epoch_ms();
    let did = Did::from(10u32);
    let added = stamped_operation(EntryOperation::Extend, did, &["a"], now_ms, writer)?;
    let horizon_ms = u128::from(EntryKind::Data.max_lifetime_ms());
    let held_at_ms = now_ms - horizon_ms;
    node.operate_storage_entry(now_ms, did, added, writer)
        .await?;
    node.join_storage_entry(
        now_ms,
        did,
        removes_only_carrier(did, DATA_TOMBSTONE_MAX_LEN, held_at_ms, "held")?,
    )
    .await?;
    let before = stored(&node, did, now_ms).await?;

    let remove = |at_ms| stamped_operation(EntryOperation::Tombstone, did, &["a"], at_ms, writer);
    assert!(matches!(
        node.operate_storage_entry(now_ms, did, remove(now_ms)?, writer)
            .await,
        Err(Error::EntryTombstonesExceedMax { .. })
    ));
    assert_eq!(stored(&node, did, now_ms).await?, before);

    let stable_ms = held_at_ms + horizon_ms + TS_OFFSET_TOLERANCE_MS;
    node.operate_storage_entry(stable_ms, did, remove(stable_ms)?, writer)
        .await?;
    let after = stored(&node, did, stable_ms).await?;
    assert_eq!(after.as_ref().map(|entry| entry.data.len()), Some(0));
    assert_eq!(after.map(|entry| entry.crdt.tombstones.len()), Some(1));
    Ok(())
}
