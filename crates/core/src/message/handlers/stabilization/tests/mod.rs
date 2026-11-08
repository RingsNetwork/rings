use std::sync::Arc;

use super::*;
use crate::delegation::DelegateeKey;
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::message::handlers::HandlerContext;
use crate::message::Message;
use crate::message::MessageSigner;
use crate::swarm::Swarm;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::manually_establish_connection;
use crate::tests::TEST_NETWORK_ID;

fn notify_context(origin: &SecretKey, destination: crate::dht::Did) -> Result<MessagePayload> {
    let session = DelegateeKey::new_with_seckey(origin)?;
    MessagePayload::new_send(
        Message::custom(b"notify predecessor context")?,
        MessageSigner::new(&session, TEST_NETWORK_ID),
        destination,
        destination,
    )
}

fn leave_context(
    origin: &SecretKey,
    destination: crate::dht::Did,
    successor_list: Vec<crate::dht::Did>,
) -> Result<(MessagePayload, Leave)> {
    let session = DelegateeKey::new_with_seckey(origin)?;
    let leave = Leave { successor_list };
    let payload = MessagePayload::new_send(
        Message::Leave(leave.clone()),
        MessageSigner::new(&session, TEST_NETWORK_ID),
        destination,
        destination,
    )?;
    Ok((payload, leave))
}

#[tokio::test]
async fn test_leave_is_ignored_by_node_not_holding_sender_as_predecessor_or_successor() -> Result<()>
{
    let [key1, key2, key3] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2.clone()).await;
    let node3 = prepare_node(key3).await;
    let outsider = SecretKey::random();
    let outsider_did: crate::dht::Did = outsider.address().into();

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    manually_establish_connection(&node3.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    for _ in 0..3 {
        run_stabilization_once(node1.swarm.clone()).await?;
        run_stabilization_once(node2.swarm.clone()).await?;
        run_stabilization_once(node3.swarm.clone()).await?;
        wait_for_msgs([&node1, &node2, &node3]).await;
    }

    node1.dht().replace_fingers_for_test(&[(0, outsider_did)])?;
    assert_eq!(*node1.dht().lock_predecessor()?, Some(node3.did()));
    assert_eq!(node1.dht().successors().list()?, vec![
        node2.did(),
        node3.did()
    ]);

    let (context, leave) = leave_context(&outsider, node1.did(), vec![node2.did(), node3.did()])?;
    let handler = MessageHandler::new(node1.swarm.transport.clone());
    handler
        .handle(&HandlerContext::unbound_for_test(&context), &leave)
        .await?;

    assert_eq!(*node1.dht().lock_predecessor()?, Some(node3.did()));
    assert_eq!(node1.dht().successors().list()?, vec![
        node2.did(),
        node3.did()
    ]);
    assert_eq!(node1.dht().lock_finger()?.get(0), Some(outsider_did));
    Ok(())
}

#[tokio::test]
async fn test_leave_removes_only_authenticated_sender_from_predecessor_and_successor() -> Result<()>
{
    let [key1, key2, key3] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2.clone()).await;
    let node3 = prepare_node(key3).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    manually_establish_connection(&node3.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    for _ in 0..5 {
        run_stabilization_once(node1.swarm.clone()).await?;
        run_stabilization_once(node2.swarm.clone()).await?;
        run_stabilization_once(node3.swarm.clone()).await?;
        wait_for_msgs([&node1, &node2, &node3]).await;
    }

    assert_eq!(*node1.dht().lock_predecessor()?, Some(node3.did()));
    assert_eq!(node1.dht().successors().list()?, vec![
        node2.did(),
        node3.did()
    ]);
    assert_eq!(*node3.dht().lock_predecessor()?, Some(node2.did()));
    assert_eq!(node3.dht().successors().list()?, vec![
        node1.did(),
        node2.did()
    ]);

    // node2 sends Leave naming node3 and node1; neither node1 nor node3 may remove any peer
    // other than node2 itself.
    let (ctx_for_node1, leave_for_node1) =
        leave_context(&key2, node1.did(), vec![node3.did(), node1.did()])?;
    MessageHandler::new(node1.swarm.transport.clone())
        .handle(
            &HandlerContext::unbound_for_test(&ctx_for_node1),
            &leave_for_node1,
        )
        .await?;
    assert_eq!(*node1.dht().lock_predecessor()?, Some(node3.did()));
    assert_eq!(node1.dht().successors().list()?, vec![node3.did()]);

    let (ctx_for_node3, leave_for_node3) =
        leave_context(&key2, node3.did(), vec![node3.did(), node1.did()])?;
    MessageHandler::new(node3.swarm.transport.clone())
        .handle(
            &HandlerContext::unbound_for_test(&ctx_for_node3),
            &leave_for_node3,
        )
        .await?;
    assert_eq!(*node3.dht().lock_predecessor()?, None);
    assert_eq!(node3.dht().successors().list()?, vec![node1.did()]);
    Ok(())
}

#[tokio::test]
async fn test_notify_predecessor_rejects_origin_mismatch_without_mutating_topology() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let origin = SecretKey::random();
    let spoofed = SecretKey::random().address().into();
    let context = notify_context(&origin, node.did())?;
    let handler = MessageHandler::new(node.swarm.transport.clone());

    let result = handler
        .handle(
            &HandlerContext::unbound_for_test(&context),
            &NotifyPredecessorSend { did: spoofed },
        )
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
    let handler = MessageHandler::new(node.swarm.transport.clone());

    let result = handler
        .handle(
            &HandlerContext::unbound_for_test(&context),
            &NotifyPredecessorSend { did: origin_did },
        )
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

/// One stabilize round: query the successor head; applying its report notifies
/// the head, which is the only predecessor-propagation path.
async fn run_stabilization_once(swarm: Arc<Swarm>) -> Result<()> {
    let stab = swarm.stabilizer();
    stab.correct_stabilize().await
}

#[tokio::test]
async fn test_notify_predecessor_rejects_connected_peer_inside_forward_interval() -> Result<()> {
    let keys = gen_ordered_keys::<3>();
    // Clockwise order: node1 < node2 < node3.
    // Relative to `node1`, `node2` lies inside `(node1, node3]` on a 3-node non-wrapping ring.
    let node1 = prepare_node(keys[0].clone()).await;
    let node2 = prepare_node(keys[1].clone()).await;
    let node3 = prepare_node(keys[2].clone()).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    assert_no_more_msg([&node1, &node2, &node3]).await;

    manually_establish_connection(&node3.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    assert_no_more_msg([&node1, &node2, &node3]).await;

    assert!(node1
        .swarm
        .transport
        .admitted_send_connection(node2.did())?
        .is_some());
    assert_eq!(node1.dht().successors().list()?, vec![
        node2.did(),
        node3.did()
    ]);
    assert_eq!(*node1.dht().lock_predecessor()?, None);

    let context = notify_context(&keys[1], node1.did())?;
    let handler = MessageHandler::new(node1.swarm.transport.clone());
    handler
        .handle(
            &HandlerContext::unbound_for_test(&context),
            &NotifyPredecessorSend { did: node2.did() },
        )
        .await?;

    assert_eq!(*node1.dht().lock_predecessor()?, None);
    let topo = node1.dht().topology_state()?;
    assert!(!crate::dht::topology::is_responsible_for(
        &topo,
        node3.did()
    ));
    Ok(())
}
