//! Conformance of the rerouting automaton with the dummy transport (#859), on the path the
//! model checks: the writer's admitted generation toward the owner dies (`Die`, the send
//! terminal mark of `terminate_accepted_connection`) while the topology still routes to it
//! (`TopologyReferencesOnlyAdmitted`), so the operation's send is refused before acceptance
//! with `SwarmMissDidInTable` and the placement waits (`Waiting`, witnessed by its listener on
//! the link epoch). The dead generation then closes (`Close`) and a replacement is admitted
//! (`Dial`, `Admit`); the admission is the event that wakes the placement. No wall-clock wait
//! is involved: the test drives the events and awaits the operation's own completion.

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
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoder;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;

/// Two linked nodes, ordered so that `owner` holds `topic`'s placement: `(writer, owner)`.
async fn linked_route(topic: &str) -> Result<(Node, Node)> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
    let first = prepare_node(next_generated_key(&mut keys)?).await;
    let second = prepare_node(next_generated_key(&mut keys)?).await;
    let placement = Entry::gen_did(topic)?;
    let (writer, owner) = if placement != second.did()
        && Did::cmp_from_observer(second.did(), placement, first.did()) == Ordering::Less
    {
        (first, second)
    } else {
        (second, first)
    };
    manually_establish_connection(&writer.swarm, &owner.swarm).await;
    wait_for_msgs([&writer, &owner]).await;
    assert_eq!(
        writer.dht().operate_route(placement, EntryKind::Data)?,
        OperateRoute::Remote(owner.did())
    );
    Ok((writer, owner))
}

/// `Die`: mark the writer's admitted generation toward `owner` send-terminal; the topology
/// keeps routing to it until the generation closes.
fn kill_generation(writer: &Node, owner: &Node) -> Result<()> {
    let admitted = writer
        .swarm
        .transport
        .admitted_send_connection(owner.did())?
        .ok_or(Error::SwarmMissDidInTable(owner.did()))?;
    assert!(
        admitted.mark_send_terminal()?,
        "the generation was sendable"
    );
    Ok(())
}

/// `Close`, `Dial`, `Admit`: retire the dead generation and admit a replacement.
async fn replace_generation(writer: &Node, owner: &Node) -> Result<()> {
    writer.swarm.disconnect(owner.did()).await?;
    wait_for_msgs([writer, owner]).await;
    manually_establish_connection(&writer.swarm, &owner.swarm).await;
    wait_for_msgs([writer, owner]).await;
    Ok(())
}

/// Law (S1, L1 on the shell): an append refused by a dead generation waits for the
/// replacement's admission, then is applied at the owner exactly once.
#[tokio::test]
async fn test_append_refused_by_a_dead_generation_waits_and_applies_once() -> Result<()> {
    let topic = "rerouted append waits for the replacement generation";
    let (writer, owner) = linked_route(topic).await?;
    let placement = Entry::gen_did(topic)?;
    kill_generation(&writer, &owner)?;

    let append = writer
        .swarm
        .storage_append_data(topic, "111".to_string().encode()?);
    pin_mut!(append);
    assert!(poll!(append.as_mut()).is_pending());
    assert_eq!(
        writer.swarm.transport.link_waiters_for_test(),
        1,
        "the refused placement waits for a link event"
    );
    assert_eq!(owner.dht().storage.count().await?, 0);

    replace_generation(&writer, &owner).await?;
    append.await?;
    assert_eq!(writer.swarm.transport.link_waiters_for_test(), 0);
    wait_for_msgs([&writer, &owner]).await;

    writer.swarm.storage_fetch(placement).await?;
    wait_for_msgs([&writer, &owner]).await;
    assert_cached_data_values(&writer, placement, &["111"]).await
}

/// Law (L1 on the shell): a lookup refused by a dead generation waits for the replacement's
/// admission, then finds the owner's value.
#[tokio::test]
async fn test_fetch_refused_by_a_dead_generation_waits_for_the_replacement() -> Result<()> {
    let topic = "rerouted fetch waits for the replacement generation";
    let (reader, owner) = linked_route(topic).await?;
    let placement = Entry::gen_did(topic)?;
    reader
        .swarm
        .storage_append_data(topic, "111".to_string().encode()?)
        .await?;
    wait_for_msgs([&reader, &owner]).await;
    kill_generation(&reader, &owner)?;

    let fetch = reader.swarm.storage_fetch(placement);
    pin_mut!(fetch);
    assert!(poll!(fetch.as_mut()).is_pending());
    assert_eq!(
        reader.swarm.transport.link_waiters_for_test(),
        1,
        "the refused lookup waits for a link event"
    );

    replace_generation(&reader, &owner).await?;
    fetch.await?;
    wait_for_msgs([&reader, &owner]).await;
    assert_cached_data_values(&reader, placement, &["111"]).await
}
