//! Conformance of the rerouting automaton with the dummy transport (#859): a user DHT
//! operation whose hop has no admitted link defers instead of failing, is woken by the link's
//! admission (an event, never a duration), and takes effect exactly once.
//!
//! Each test polls the operation once (`poll!`) to observe that it is waiting, where the
//! pre-#859 path returned `SwarmMissDidInTable`; the admission is then driven by
//! `manually_establish_connection`, and the operation's own completion is awaited.

use std::cmp::Ordering;

use futures::pin_mut;
use futures::poll;

use super::super::ChordStorageInterface;
use super::test_support::assert_cached_data_values;
use super::test_support::next_generated_key;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::Did;
use crate::dht::OperateRoute;
use crate::ecc::tests::gen_ordered_keys;
use crate::error::Result;
use crate::message::Encoder;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;

/// Two unlinked nodes, ordered so that `owner` holds `topic`'s placement: `(writer, owner)`.
///
/// The writer's topology admits the owner without a link, as a topology that outlived its
/// link does, so the writer routes the placement to a hop it cannot send to.
async fn unlinked_route(topic: &str) -> Result<(Node, Node)> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
    let first = prepare_node(next_generated_key(&mut keys)?).await;
    let second = prepare_node(next_generated_key(&mut keys)?).await;
    let entry: Entry = (topic.to_string(), topic.to_string()).try_into()?;
    let placement = entry.did;
    let (writer, owner) = if placement != second.did()
        && Did::cmp_from_observer(second.did(), placement, first.did()) == Ordering::Less
    {
        (first, second)
    } else {
        (second, first)
    };
    writer.dht().admit_connected(owner.did(), None)?;
    assert_eq!(
        writer.dht().operate_route(placement, EntryKind::Data)?,
        OperateRoute::Remote(owner.did())
    );
    Ok((writer, owner))
}

/// Law (S1, L1 on the shell): an append routed to an unlinked hop waits for the hop's
/// admission, then is applied exactly once.
#[tokio::test]
async fn test_append_to_an_unlinked_hop_waits_for_admission_and_applies_once() -> Result<()> {
    let topic = "rerouted append waits for the hop's admission";
    let (writer, owner) = unlinked_route(topic).await?;
    let placement = Entry::gen_did(topic)?;

    let append = writer
        .swarm
        .storage_append_data(topic, "111".to_string().encode()?);
    pin_mut!(append);
    assert!(
        poll!(append.as_mut()).is_pending(),
        "a missing link defers the send"
    );
    assert_eq!(owner.dht().storage.count().await?, 0);

    manually_establish_connection(&writer.swarm, &owner.swarm).await;
    append.await?;
    wait_for_msgs([&writer, &owner]).await;

    writer.swarm.storage_fetch(placement).await?;
    wait_for_msgs([&writer, &owner]).await;
    assert_cached_data_values(&writer, placement, &["111"]).await
}

/// Law (L1 on the shell): a fetch routed to an unlinked hop waits for the hop's admission,
/// then finds the owner's value.
#[tokio::test]
async fn test_fetch_from_an_unlinked_hop_waits_for_admission() -> Result<()> {
    let topic = "rerouted fetch waits for the hop's admission";
    let (reader, owner) = unlinked_route(topic).await?;
    let placement = Entry::gen_did(topic)?;
    owner
        .swarm
        .storage_append_data(topic, "111".to_string().encode()?)
        .await?;

    let fetch = reader.swarm.storage_fetch(placement);
    pin_mut!(fetch);
    assert!(
        poll!(fetch.as_mut()).is_pending(),
        "a missing link defers the lookup"
    );

    manually_establish_connection(&reader.swarm, &owner.swarm).await;
    fetch.await?;
    wait_for_msgs([&reader, &owner]).await;
    assert_cached_data_values(&reader, placement, &["111"]).await
}
