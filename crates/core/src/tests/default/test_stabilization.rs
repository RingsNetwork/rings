use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;

use crate::dht::successor::SuccessorReader;
use crate::dht::Chord;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::inspect::DHTInspect;
use crate::inspect::SwarmInspect;
use crate::tests::default::gen_pure_dht;
use crate::tests::default::prepare_node;
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

#[tokio::test]
async fn test_stabilization_final_dht() -> Result<()> {
    let mut swarms = vec![];

    // Save nodes to prevent the receiver from being lost,
    // which would lead to a panic in the monitoring callback when recording messages.
    let mut nodes = vec![];

    let keys = vec![
        "9c83fcb684af3dc71018b5a303245d2f2fed8a579096589f3234a67a52a7ac66", // 0xcc13321381c4be4d3264588d4573c9529c0167a0
        "fd674cb6089663935cb061254602e343da8a2fa3908980ae4f7a27adb8b7ac8a", // 0xdbf2d77c3a8bb59379009ec2ec423b8b58d60dbe
        "b9ce7159a2ad3b9fe885a7744d32afeec233e7ddeaed0759cbab2c00a1bd548b", // 0xd9863aad3267eaadca60adf51464e16d6f79465b
        "4efb629f54a3f3dd91f5efffc4f9b51ab27eb082b2393067757681ed6439480d", // 0x8a5f987d1c2cc0fd6e0083df22ba9bd802706348
        "f2cbca82fb82745c1f9d94c1c9d2b0606daaf6f15ac8a215fc72c8bc0478ecf5", // 0x2b5d1f769f346a08cee37f7382495b01126d480a
        "e1d7f24e2b725df077627fc0337b9c53b37ce594ca84fccd0f36dc58423a0ed2", // 0xca82ac762999ef4438d09223b01f9bf194cea94e
    ];

    for s in keys {
        let key = SecretKey::try_from(s).unwrap();
        let node = prepare_node(key).await;
        swarms.push(node.swarm.clone());
        let stabilizer = Arc::new(node.swarm.stabilizer());
        nodes.push(node);
        tokio::spawn(stabilizer.wait(Duration::from_secs(3)));
    }

    let swarm1 = Arc::clone(&swarms[0]);
    for swarm in swarms.iter().skip(1) {
        manually_establish_connection(&swarm1, swarm).await;
    }

    sleep(Duration::from_secs(9)).await;

    let mut expected_dhts = vec![];
    for swarm in swarms.iter() {
        let dht = gen_pure_dht(swarm.did());
        for other in swarms.iter() {
            if dht.did != other.did() {
                dht.join(other.did()).unwrap();
                dht.notify(other.did()).unwrap();
            }
        }

        expected_dhts.push(DHTInspect::inspect(&dht));
    }

    let mut current_dhts = vec![];
    for swarm in swarms.iter() {
        println!(
            "Connected peers: {:?}",
            SwarmInspect::inspect(swarm).await.peers
        );
        current_dhts.push(DHTInspect::inspect(&swarm.dht()));
    }

    for (i, (cur, exp)) in std::iter::zip(current_dhts, expected_dhts).enumerate() {
        println!("Check node{}", i);
        pretty_assertions::assert_eq!(cur, exp);
    }

    Ok(())
}
