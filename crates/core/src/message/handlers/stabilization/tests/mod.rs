use std::sync::Arc;

use super::*;
use crate::dht::successor::SuccessorReader;
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::message::MessageSigner;
use crate::session::SessionSk;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::Swarm;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::manually_establish_connection;
use crate::tests::TEST_NETWORK_ID;

struct NoopCallback;

impl SwarmCallback for NoopCallback {}

fn notify_context(origin: &SecretKey, destination: crate::dht::Did) -> Result<MessagePayload> {
    let session = SessionSk::new_with_seckey(origin)?;
    MessagePayload::new_send(
        Message::custom(b"notify predecessor context")?,
        MessageSigner::new(&session, TEST_NETWORK_ID),
        destination,
        destination,
    )
}

#[tokio::test]
async fn test_notify_predecessor_rejects_origin_mismatch_without_mutating_topology() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let origin = SecretKey::random();
    let spoofed = SecretKey::random().address().into();
    let context = notify_context(&origin, node.did())?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));

    let result = handler
        .handle(&context, &NotifyPredecessorSend { did: spoofed })
        .await;

    assert!(matches!(
        result,
        Err(Error::NotifyPredecessorOriginMismatch {
            claimed,
            origin: observed_origin,
        }) if claimed == spoofed && observed_origin == origin.address().into()
    ));
    assert_eq!(*node.dht().lock_predecessor()?, None);
    Ok(())
}

#[tokio::test]
async fn test_notify_predecessor_rejects_unadmitted_origin_without_mutating_topology() -> Result<()>
{
    let node = prepare_node(SecretKey::random()).await;
    let origin = SecretKey::random();
    let origin_did = origin.address().into();
    let context = notify_context(&origin, node.did())?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));

    let result = handler
        .handle(&context, &NotifyPredecessorSend { did: origin_did })
        .await;

    assert!(matches!(
        result,
        Err(Error::NotifyPredecessorOriginNotAdmitted { origin })
            if origin == origin_did
    ));
    assert_eq!(*node.dht().lock_predecessor()?, None);
    Ok(())
}

#[tokio::test]
async fn test_triple_nodes_stabilization_1_2_3() -> Result<()> {
    let [key1, key2, key3]: [SecretKey; 3] = gen_ordered_keys::<3>();
    test_triple_ordered_nodes_stabilization(key1, key2, key3).await
}

#[tokio::test]
async fn test_triple_nodes_stabilization_2_3_1() -> Result<()> {
    let [key1, key2, key3]: [SecretKey; 3] = gen_ordered_keys::<3>();

    test_triple_ordered_nodes_stabilization(key2, key3, key1).await
}

#[tokio::test]
async fn test_triple_nodes_stabilization_3_1_2() -> Result<()> {
    let [key1, key2, key3]: [SecretKey; 3] = gen_ordered_keys::<3>();
    test_triple_ordered_nodes_stabilization(key3, key1, key2).await
}

#[tokio::test]
async fn test_triple_nodes_stabilization_3_2_1() -> Result<()> {
    let [key1, key2, key3]: [SecretKey; 3] = gen_ordered_keys::<3>();
    test_triple_desc_ordered_nodes_stabilization(key3, key2, key1).await
}

#[tokio::test]
async fn test_triple_nodes_stabilization_2_1_3() -> Result<()> {
    let [key1, key2, key3]: [SecretKey; 3] = gen_ordered_keys::<3>();
    test_triple_desc_ordered_nodes_stabilization(key2, key1, key3).await
}

#[tokio::test]
async fn test_triple_nodes_stabilization_1_3_2() -> Result<()> {
    let [key1, key2, key3]: [SecretKey; 3] = gen_ordered_keys::<3>();
    test_triple_desc_ordered_nodes_stabilization(key1, key3, key2).await
}

async fn test_triple_ordered_nodes_stabilization(
    key1: SecretKey,
    key2: SecretKey,
    key3: SecretKey,
) -> Result<()> {
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    let node3 = prepare_node(key3).await;

    println!("========================================");
    println!("||  now we connect node1 and node2    ||");
    println!("========================================");

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    assert_no_more_msg([&node1, &node2, &node3]).await;

    println!("========================================");
    println!("||  now we start join node3 to node2  ||");
    println!("========================================");

    manually_establish_connection(&node3.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    assert_no_more_msg([&node1, &node2, &node3]).await;

    println!("=== Check state before stabilization ===");
    assert_eq!(node1.dht().successors().list()?, vec![
        node2.did(),
        node3.did()
    ]);
    assert_eq!(node2.dht().successors().list()?, vec![
        node3.did(),
        node1.did()
    ]);
    assert_eq!(node3.dht().successors().list()?, vec![
        node1.did(),
        node2.did()
    ]);
    assert!(node1.dht().lock_predecessor()?.is_none());
    assert!(node2.dht().lock_predecessor()?.is_none());
    assert!(node3.dht().lock_predecessor()?.is_none());

    println!("========================================");
    println!("||  now we start first stabilization  ||");
    println!("========================================");

    run_stabilization_once(node1.swarm.clone()).await?;
    run_stabilization_once(node2.swarm.clone()).await?;
    run_stabilization_once(node3.swarm.clone()).await?;

    wait_for_msgs([&node1, &node2, &node3]).await;
    assert_no_more_msg([&node1, &node2, &node3]).await;

    println!("=== Check state after first stabilization ===");
    assert!(node1.dht().successors().list()?.contains(&node2.did()));
    assert_eq!(node2.dht().successors().list()?, vec![
        node3.did(),
        node1.did()
    ]);
    assert!(node3.dht().successors().list()?.contains(&node2.did()));

    println!("==========================================");
    println!("||  now we start 5 times stabilization  ||");
    println!("==========================================");

    for _ in 0..5 {
        run_stabilization_once(node1.swarm.clone()).await?;
        run_stabilization_once(node2.swarm.clone()).await?;
        run_stabilization_once(node3.swarm.clone()).await?;

        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("=== Check state after stabilization ===");
        assert_eq!(node1.dht().successors().list()?, vec![
            node2.did(),
            node3.did()
        ]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node3.did(),
            node1.did()
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![
            node1.did(),
            node2.did()
        ]);
    }

    println!("=== Check predecessor after all stabilization ===");
    assert_eq!(*node1.dht().lock_predecessor()?, Some(node3.did()));
    assert_eq!(*node2.dht().lock_predecessor()?, Some(node1.did()));
    assert_eq!(*node3.dht().lock_predecessor()?, Some(node2.did()));
    Ok(())
}

async fn test_triple_desc_ordered_nodes_stabilization(
    key1: SecretKey,
    key2: SecretKey,
    key3: SecretKey,
) -> Result<()> {
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    let node3 = prepare_node(key3).await;

    println!("========================================");
    println!("||  now we connect node1 and node2    ||");
    println!("========================================");

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    assert_no_more_msg([&node1, &node2, &node3]).await;

    println!("========================================");
    println!("||  now we start join node3 to node2  ||");
    println!("========================================");

    manually_establish_connection(&node3.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    assert_no_more_msg([&node1, &node2, &node3]).await;

    println!("=== Check state before stabilization ===");
    assert_eq!(node1.dht().successors().list()?, vec![
        node3.did(),
        node2.did()
    ]);
    assert_eq!(node2.dht().successors().list()?, vec![
        node1.did(),
        node3.did()
    ]);
    assert_eq!(node3.dht().successors().list()?, vec![
        node2.did(),
        node1.did()
    ]);
    assert!(node1.dht().lock_predecessor()?.is_none());
    assert!(node2.dht().lock_predecessor()?.is_none());
    assert!(node3.dht().lock_predecessor()?.is_none());

    println!("========================================");
    println!("||  now we start first stabilization  ||");
    println!("========================================");

    run_stabilization_once(node1.swarm.clone()).await?;
    run_stabilization_once(node2.swarm.clone()).await?;
    run_stabilization_once(node3.swarm.clone()).await?;

    wait_for_msgs([&node1, &node2, &node3]).await;
    assert_no_more_msg([&node1, &node2, &node3]).await;

    println!("=== Check state after first stabilization ===");
    assert!(node1.dht().successors().list()?.contains(&node2.did()));
    assert_eq!(node2.dht().successors().list()?, vec![
        node1.did(),
        node3.did()
    ]);
    assert!(node3.dht().successors().list()?.contains(&node2.did()));

    println!("==========================================");
    println!("||  now we start 5 times stabilization  ||");
    println!("==========================================");

    for _ in 0..5 {
        run_stabilization_once(node1.swarm.clone()).await?;
        run_stabilization_once(node2.swarm.clone()).await?;
        run_stabilization_once(node3.swarm.clone()).await?;

        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("=== Check state after stabilization ===");
        assert_eq!(node1.dht().successors().list()?, vec![
            node3.did(),
            node2.did()
        ]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node1.did(),
            node3.did()
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![
            node2.did(),
            node1.did()
        ]);
    }

    println!("=== Check predecessor after all stabilization ===");
    assert_eq!(*node1.dht().lock_predecessor()?, Some(node2.did()));
    assert_eq!(*node2.dht().lock_predecessor()?, Some(node3.did()));
    assert_eq!(*node3.dht().lock_predecessor()?, Some(node1.did()));

    Ok(())
}

async fn run_stabilization_once(swarm: Arc<Swarm>) -> Result<()> {
    let stab = swarm.stabilizer();
    stab.notify_predecessor().await
}
