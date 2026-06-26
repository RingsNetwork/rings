use std::sync::Arc;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::SwarmBuilder;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_predecessor;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;

#[tokio::test]
async fn test_stabilization_once() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    // key 2 > key 1 here
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    println!("swarm1: {:?}, swarm2: {:?}", node1.did(), node2.did());

    wait_for_successor(&node1, key2.address().into()).await?;
    wait_for_successor(&node2, key1.address().into()).await?;

    let stabilizer = node1.swarm.stabilizer();
    stabilizer.stabilize().await?;
    wait_for_predecessor(&node2, key1.address().into()).await?;
    wait_for_successor(&node1, key2.address().into()).await?;

    Ok(())
}

#[tokio::test]
async fn stabilize_republishes_local_entries_to_missing_affine_owners() -> Result<()> {
    let key = SecretKey::random();
    let session = SessionSk::new_with_seckey(&key)?;
    let swarm = Arc::new(
        SwarmBuilder::new(
            0,
            "stun://stun.l.google.com:19302",
            Box::new(MemStorage::new()),
            session,
        )
        .dht_storage_redundancy(2)
        .build(),
    );
    let node = Node::new(swarm);
    let entry = Entry {
        did: key.address().into(),
        data: vec![],
        kind: EntryKind::Data,
    };
    let placement_keys = entry.did.rotate_affine(2)?;
    node.dht()
        .storage
        .put(&placement_keys[0].to_string(), &entry)
        .await?;

    node.swarm.stabilizer().stabilize().await?;

    assert_eq!(
        node.dht()
            .storage
            .get(&placement_keys[1].to_string())
            .await?,
        Some(entry)
    );
    Ok(())
}

#[tokio::test]
async fn test_stabilization() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    // key 2 > key 1 here
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    wait_for_successor(&node1, key2.address().into()).await?;
    wait_for_successor(&node2, key1.address().into()).await?;

    let stabilizer1 = node1.swarm.stabilizer();
    let stabilizer2 = node2.swarm.stabilizer();
    tokio::try_join!(stabilizer1.stabilize(), stabilizer2.stabilize())?;

    wait_for_predecessor(&node2, key1.address().into()).await?;
    wait_for_predecessor(&node1, key2.address().into()).await?;
    Ok(())
}
