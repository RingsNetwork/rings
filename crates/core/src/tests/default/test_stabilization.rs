use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;

use crate::dht::successor::SuccessorReader;
use crate::dht::Chord;
use crate::dht::Stabilization;
use crate::dht::TStabilize;
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

    let stabilization = Stabilization::new(node1.swarm.clone(), 5);
    let _ = stabilization.stabilize().await;
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
                    let stabilization = Arc::new(Stabilization::new(node1.swarm.clone(), 5));
                    stabilization.wait().await;
                },
                async {
                    let stabilization = Arc::new(Stabilization::new(node2.swarm.clone(), 5));
                    stabilization.wait().await;
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
        "af3543cde0c40fd217c536a358fb5f3c609eb1135f68daf7e2f2fbd51f164221", // 0xfe81c75f0ef75d7436b84089c5be31b692518d73
        "d842f7143beac06ab8d81589c3c53cffd7eb8e07dadae8fdcb3ed1e1319ab477", // 0x8d4300b4df3c85ee107009e354c1b95717ab1c17
        "57e3ff36ab767056baba3ea9d09c6a178721bd686b8c5d98f66147865de6a288", // 0x78b3fdea83a3db371cfc79c45f0527b7f216e6c9
    ];

    for s in keys {
        let key = SecretKey::try_from(s).unwrap();
        let node = prepare_node(key).await;
        swarms.push(node.swarm.clone());
        let stabilization = Arc::new(Stabilization::new(node.swarm.clone(), 3));
        nodes.push(node);
        tokio::spawn(stabilization.wait());
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
            SwarmInspect::inspect(swarm).await.connections
        );
        current_dhts.push(DHTInspect::inspect(&swarm.dht()));
    }

    for (i, (cur, exp)) in std::iter::zip(current_dhts, expected_dhts)
        .enumerate()
        .skip(2)
    {
        println!("Check node{}", i);
        pretty_assertions::assert_eq!(cur, exp);
    }

    Ok(())
}
