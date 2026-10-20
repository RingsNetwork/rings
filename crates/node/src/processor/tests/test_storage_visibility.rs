//! Read-your-writes over the controlled network (#948, #982): the trace in which a registry
//! store and the writer's following lookup reach different owners.
//!
//! ```text
//! ring (clockwise)   p₅ ─ A ─ B ─ C ─ p₀ … p₅        every placement pᵢ lies in (C, A)
//! {A–B}              A ─store─▶ B = owner(pᵢ)                  B applies the registry write
//! {A–B, B–C}         A ─fetch─▶ B ─▶ C = owner(pᵢ)             C holds nothing: a miss at pᵢ
//! ```
//!
//! Admitting C makes it B's successor head, so B forwards every placement to C at once; the
//! hand-off of B's replicas waits for a storage repair round, and no node here runs the
//! stabilizer. Before #982 the writer's lookup answered the empty registry; with it, the writer
//! reads its own write from its cache.

use rings_core::dht::entry::EntryKind;
use rings_core::dht::Chord;
use rings_core::dht::PeerRingAction;
use rings_core::dht::PeerRingRemoteAction;

use super::common::*;
use super::*;
use crate::consts::DATA_REDUNDANT;
use crate::tests::activity::probe_on_activity;

/// Upper bound on identities drawn for [`prepare_owner_move`]; each lands in the placement gap
/// with probability `1 / DATA_REDUNDANT`, so the bound is a failure bound only.
const OWNER_MOVE_DRAW_BOUND: usize = 4096;

/// The writer `A`, the first owner `B` and the later owner `C`, in that clockwise order inside
/// the gap `(p₅, p₀)` between the last and the first registry placement, so that every
/// placement lies in `(C, A)`: `B` owns all of them while it knows only `A`, and `C`, knowing
/// only `B`, owns all of them once it is `B`'s successor.
async fn prepare_owner_move(placements: &[Did]) -> Result<(Processor, Processor, Processor)> {
    let (Some(first), Some(last)) = (placements.first(), placements.last()) else {
        return Err(Error::InvalidConfig(
            "a registry has placements".to_string(),
        ));
    };
    let gap = *first - *last;
    let mut drawn = Vec::new();
    for _ in 0..OWNER_MOVE_DRAW_BOUND {
        let key = SecretKey::random();
        let did: Did = key.address().into();
        if did - *last < gap {
            drawn.push((did - *last, key));
        }
        if drawn.len() == 3 {
            break;
        }
    }
    drawn.sort_by_key(|(offset, _)| *offset);
    let mut keys = drawn.into_iter().map(|(_, key)| key);
    let (Some(writer), Some(first_owner), Some(later_owner)) =
        (keys.next(), keys.next(), keys.next())
    else {
        return Err(Error::InvalidConfig(
            "could not draw three identities inside the registry placement gap".to_string(),
        ));
    };
    let processor = |key| prepare_processor_with_identity_key_network_and_virtual_nodes(key, 0, 0);
    Ok((
        processor(writer).await,
        processor(first_owner).await,
        processor(later_owner).await,
    ))
}

/// Whether `processor` routes a lookup of `placement` to `next`, the next hop it asks.
fn routes_placement_to(processor: &Processor, placement: Did, next: Did) -> Result<bool> {
    Ok(matches!(
        processor.swarm.dht().find_successor(placement)?,
        PeerRingAction::RemoteAction(hop, PeerRingRemoteAction::FindSuccessor(_)) if hop == next
    ))
}

/// Whether `processor` stores a value at any of `placements`.
async fn stores_any(processor: &Processor, placements: &[Did]) -> Result<bool> {
    for placement in placements {
        let stored = processor
            .swarm
            .dht()
            .stored_entry_at(EntryKind::Data, *placement, get_epoch_ms())
            .await
            .map_err(Error::Storage)?;
        if stored.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// #948: the writer stores the online-node registry at its owner `B`, the owner then moves to
/// `C`, which never received it, and the writer's lookup still lists what it stored.
///
/// Without the writer's observation (#982) the lookup reaches `C` through `B` at every
/// placement, misses, and answers the empty registry: the `ExitWithoutRelayRegistration` of the
/// gateway fixture.
#[tokio::test]
async fn test_writer_lists_its_registry_write_after_the_owner_moves() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let registry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
    let placements = registry_key.rotate_affine(DATA_REDUNDANT)?;
    let (writer, first_owner, later_owner) = prepare_owner_move(placements.as_slice()).await?;
    let _recorders = [&writer, &first_owner, &later_owner].map(ApplicationRecorder::attach);

    connect_processors(&writer, &first_owner).await;
    wait_for_mutual_dht_topology(&writer, &first_owner).await?;
    for placement in placements.iter() {
        assert!(owns_entry_placement(&first_owner, *placement)?);
        assert!(routes_placement_to(&writer, *placement, first_owner.did())?);
    }
    let now_ms = get_epoch_ms();
    let registrants = [&first_owner, &later_owner]
        .map(|registrant| registrant.online_node_descriptor_at(now_ms))
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    let expected = registrants
        .iter()
        .map(|descriptor| descriptor.did)
        .collect::<BTreeSet<_>>();
    writer
        .storage_store(Processor::registry_entry(ONLINE_NODES_TOPIC, registrants)?)
        .await?;
    wait_for_online_node_dids_in_storage(
        &first_owner,
        placements.as_slice(),
        &expected,
        "the first owner applies the registry write",
    )
    .await?;

    connect_processors(&first_owner, &later_owner).await;
    probe_on_activity(
        "the first owner routes every registry placement to the later owner",
        PROCESSOR_TEST_HANG_GUARD,
        || {
            let moved = placements.iter().try_fold(true, |moved, placement| {
                Ok::<_, Error>(
                    moved
                        && owns_entry_placement(&later_owner, *placement)?
                        && routes_placement_to(&first_owner, *placement, later_owner.did())?
                        && routes_placement_to(&writer, *placement, first_owner.did())?,
                )
            });
            async move { moved.map(|moved| moved.then_some(())) }
        },
    )
    .await?;
    assert!(!stores_any(&later_owner, placements.as_slice()).await?);

    let listed = writer
        .lookup_online_nodes(false)
        .await?
        .into_iter()
        .map(|descriptor| descriptor.did)
        .collect::<BTreeSet<_>>();
    assert_eq!(listed, expected);
    assert!(!stores_any(&later_owner, placements.as_slice()).await?);
    Ok(())
}
