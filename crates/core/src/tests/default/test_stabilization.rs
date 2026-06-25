use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::successor::SuccessorReader;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::SwarmBuilder;
use crate::tests::default::prepare_node;
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

    sleep(Duration::from_millis(1000)).await;
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));
    assert!(node2
        .dht()
        .successors()
        .list()?
        .contains(&key1.address().into()));

    let stabilizer = node1.swarm.stabilizer();
    let _ = stabilizer.stabilize().await;
    sleep(Duration::from_millis(10000)).await;
    assert_eq!(
        *node2.dht().lock_predecessor()?,
        Some(key1.address().into())
    );
    assert!(node1
        .dht()
        .successors()
        .list()?
        .contains(&key2.address().into()));

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

    tokio::select! {
        _ = async {
            tokio::join!(
                async {
                    let stabilizer = Arc::new(node1.swarm.stabilizer());
                    stabilizer.wait(Duration::from_secs(5)).await;
                },
                async {
                    let stabilizer = Arc::new(node2.swarm.stabilizer());
                    stabilizer.wait(Duration::from_secs(5)).await;
                }
            );
        } => { unreachable!(); }
        _ = async {
            sleep(Duration::from_millis(1000)).await;
            assert!(node1.dht().successors().list()?.contains(&key2.address().into()));
            assert!(node2.dht().successors().list()?.contains(&key1.address().into()));
            sleep(Duration::from_millis(10000)).await;
            assert_eq!(*node2.dht().lock_predecessor()?, Some(key1.address().into()));
            assert_eq!(*node1.dht().lock_predecessor()?, Some(key2.address().into()));
            Ok::<(), Error>(())
        } => {}
    }
    Ok(())
}
