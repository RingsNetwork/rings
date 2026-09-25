use std::str::FromStr;
#[cfg(feature = "dummy")]
use std::sync::Arc;

#[cfg(feature = "dummy")]
use rings_transport::connections::dummy_controlled;
use rings_transport::core::transport::WebrtcConnectionState;
#[cfg(feature = "dummy")]
use tokio::time::timeout;
#[cfg(feature = "dummy")]
use tokio::time::Duration;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::PlacedEntryOperation;
#[cfg(feature = "dummy")]
use crate::dht::Did;
#[cfg(feature = "dummy")]
use crate::dht::PeerRingAction;
#[cfg(feature = "dummy")]
use crate::dht::PeerRingRemoteAction;
use crate::dht::StorageKey;
#[cfg(feature = "dummy")]
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::message;
use crate::message::Encoder;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::Message;
#[cfg(feature = "dummy")]
use crate::message::MessageHandler;
#[cfg(feature = "dummy")]
use crate::swarm::callback::SwarmCallback;
#[cfg(feature = "dummy")]
use crate::tests::default::dummy_hooks::ControlledDeliveryGuard;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_connection_state;
use crate::tests::default::wait_for_finger;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_predecessor;
use crate::tests::default::wait_for_storage_entry;
use crate::tests::default::wait_for_successor;
#[cfg(feature = "dummy")]
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;

#[cfg(feature = "dummy")]
struct NoopCallback;

#[cfg(feature = "dummy")]
impl SwarmCallback for NoopCallback {}

#[cfg(feature = "dummy")]
async fn drain_controlled_dummy_events() {
    while dummy_controlled::pending() > 0 {
        assert!(dummy_controlled::deliver(0).await);
        tokio::task::yield_now().await;
    }
}

#[cfg(feature = "dummy")]
async fn drain_node_messages(nodes: &[&Node]) {
    loop {
        let mut drained = false;
        for node in nodes {
            while node.try_listen_once().await.is_some() {
                drained = true;
            }
        }
        if !drained {
            return;
        }
        tokio::task::yield_now().await;
    }
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_wait_for_msgs_does_not_ignore_controlled_transport_events() {
    let _controlled = ControlledDeliveryGuard::new();
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    assert!(dummy_controlled::pending() > 0);
    let wait = timeout(Duration::from_millis(20), wait_for_msgs([&node1, &node2])).await;

    assert!(wait.is_err(), "transport-queued events are not quiescent");
}

#[tokio::test]
async fn test_handle_join() -> Result<()> {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    assert!(node1.listen_once().await.is_some());
    assert!(node1.dht().successors().list()?.contains(&node2.did()));
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_join_dht_keeps_local_join_when_convergence_send_fails() -> Result<()> {
    dummy_controlled::enable(true);
    dummy_controlled::set_max_message_size(1);

    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    assert!(
        node1.dht().successors().list()?.is_empty(),
        "controlled delivery should prevent automatic DataChannelOpen join"
    );

    drain_controlled_dummy_events().await;

    dummy_controlled::enable(false);
    dummy_controlled::set_max_message_size(0);

    assert!(
        node1.dht().successors().list()?.contains(&node2.did()),
        "local join must survive failed follow-up convergence sends"
    );

    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_handle_dht_notify_remote_action_sends_predecessor_to_target() -> Result<()> {
    dummy_controlled::enable(true);

    let [key1, key2, key3]: [SecretKey; 3] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    let node3 = prepare_node(key3).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    drain_controlled_dummy_events().await;
    drain_node_messages(&[&node1, &node2, &node3]).await;

    // Clear any empty controlled queue so this test exercises only the
    // explicit handler action below.
    dummy_controlled::enable(false);

    let handler = MessageHandler::new(node1.swarm.transport.clone(), Arc::new(NoopCallback));
    handler
        .handle_dht_events(&PeerRingAction::RemoteAction(
            node2.did(),
            PeerRingRemoteAction::Notify(node3.did()),
        ))
        .await?;

    let payload = timeout(Duration::from_secs(1), node2.listen_once())
        .await
        .expect("notify target should receive a message")
        .expect("notify target message stream should stay open");

    assert_eq!(payload.transaction.destination, node2.did());
    match payload.transaction.data::<Message>()? {
        Message::NotifyPredecessorSend(message::NotifyPredecessorSend { did }) => {
            assert_eq!(did, node3.did());
        }
        other => panic!("expected NotifyPredecessorSend, got {other:?}"),
    }

    Ok(())
}

/// Upper bound on scheduler steps [`deliver_until`] takes before it declares the
/// awaited state unreachable. A step is one FIFO delivery or one cooperative
/// yield, so the bound counts events, never wall-clock time.
#[cfg(feature = "dummy")]
const CONTROLLED_STEP_BOUND: usize = 4_096;

/// Whether `node` holds a `Connected` transport connection to `peer`.
#[cfg(feature = "dummy")]
fn is_connected(node: &Node, peer: Did) -> bool {
    node.swarm
        .transport
        .get_connection(peer)
        .is_some_and(|conn| conn.webrtc_connection_state() == WebrtcConnectionState::Connected)
}

/// Drive the controlled dummy queue in FIFO order until `reached` holds.
///
/// Each step first observes `reached`, then delivers the oldest queued event (if
/// any) and yields once so that tasks spawned by the delivered handler run.
///
/// Pre: controlled delivery is enabled on this thread.
/// Post: returns only after `reached` was observed true. Failing to reach it
/// within [`CONTROLLED_STEP_BOUND`] steps panics with `label`. The schedule is a
/// function of the queue alone; no step reads a clock.
#[cfg(feature = "dummy")]
async fn deliver_until(label: &str, mut reached: impl FnMut() -> Result<bool>) -> Result<()> {
    for _ in 0..CONTROLLED_STEP_BOUND {
        if reached()? {
            return Ok(());
        }
        if dummy_controlled::pending() > 0 {
            assert!(
                dummy_controlled::deliver(0).await,
                "controlled event targets a live connection"
            );
        }
        tokio::task::yield_now().await;
    }
    panic!("{label} not reached within {CONTROLLED_STEP_BOUND} controlled steps");
}

/// Reachability of a connection signalled through the DHT.
///
/// Let `d(n1) < d(n2) < d(n3)`, `C(a, b)` mean that `a` holds a `Connected`
/// connection to `b`, and `S(a)` be the successor list of `a`. Let `σ` be the
/// FIFO schedule of the controlled dummy queue. `σ` is deterministic, so the run
/// does not depend on wall-clock time or on suite load.
///
/// ```text
/// E₀ = { n3–n2, n1–n2 }                              (out-of-band bootstrap)
/// P₀ ≡ C(n1,n2) ∧ C(n2,n3) ∧ n2 ∈ S(n1) ∧ n3 ∈ S(n2) ∧ n2 ∈ S(n3)
/// P₁ ≡ C(n1,n3) ∧ C(n3,n1)
///      ∧ S(n1) = [n2, n3] ∧ S(n2) = [n3, n1] ∧ S(n3) = [n1, n2]
///
/// (1)  E₀ ⊢_σ ◇P₀,  and in the first state with P₀, n1 has no link to n3
/// (2)  P₀ ; connect(n1, n3) ⊢_σ ◇P₁
/// ```
///
/// In (2), n2 is n1's only neighbour when `connect` runs. So `ConnectNodeSend`
/// must travel `n1 → n2 → n3` and `ConnectNodeReport` must travel
/// `n3 → n2 → n1`: `C(n1, n3)` can only come from n2's signalling. The claim is
/// liveness along one fair schedule, not along every interleaving. The
/// all-orders question belongs to `test_dht_schedule` and the Stateright model.
///
/// The real-WebRTC form of this flow (ICE, DTLS and SCTP over the relayed SDP)
/// is covered by `message::handlers::connection::tests::test_triple_nodes_*`.
/// Those tests run in the default build and wait on quiescence, not on a
/// deadline.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_handle_connect_node() -> Result<()> {
    let [key1, key2, key3]: [SecretKey; 3] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    let node3 = prepare_node(key3).await;
    let _controlled = ControlledDeliveryGuard::new();

    manually_establish_connection(&node3.swarm, &node2.swarm).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    deliver_until("P0: n1-n2-n3 path joined", || {
        Ok(is_connected(&node1, node2.did())
            && is_connected(&node2, node3.did())
            && node1.dht().successors().contains(&node2.did())?
            && node2.dht().successors().contains(&node3.did())?
            && node3.dht().successors().contains(&node2.did())?)
    })
    .await?;
    assert!(
        node1.swarm.transport.get_connection(node3.did()).is_none(),
        "n1 must not reach n3 before the DHT-signalled connect"
    );

    node1.swarm.connect(node3.did()).await?;

    deliver_until("P1: n1-n3 connected via n2, successors converged", || {
        Ok(is_connected(&node1, node3.did())
            && is_connected(&node3, node1.did())
            && node1.dht().successors().list()? == vec![node2.did(), node3.did()]
            && node2.dht().successors().list()? == vec![node3.did(), node1.did()]
            && node3.dht().successors().list()? == vec![node1.did(), node2.did()])
    })
    .await
}

#[tokio::test]
async fn test_handle_notify_predecessor() -> Result<()> {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_successor(&node2, node1.did()).await?;
    node1
        .swarm
        .send_message(
            Message::NotifyPredecessorSend(message::NotifyPredecessorSend { did: node1.did() }),
            node2.did(),
        )
        .await
        .unwrap();
    wait_for_predecessor(&node2, node1.did()).await?;
    assert!(node1.dht().successors().list()?.contains(&node2.did()));

    Ok(())
}

#[tokio::test]
async fn test_handle_find_successor_increase() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    if key1.address() > key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_successor(&node2, node1.did()).await?;
    node1
        .swarm
        .send_message(
            Message::NotifyPredecessorSend(message::NotifyPredecessorSend { did: node1.did() }),
            node2.did(),
        )
        .await
        .unwrap();
    wait_for_predecessor(&node2, node1.did()).await?;
    assert!(node1.dht().successors().list()?.contains(&node2.did()));

    println!("node1: {:?}, node2: {:?}", node1.did(), node2.did());
    node2
        .swarm
        .send_message(
            Message::FindSuccessorSend(message::FindSuccessorSend {
                did: node2.did(),
                then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                strict: true,
            }),
            node1.did(),
        )
        .await
        .unwrap();
    wait_for_msgs([&node1, &node2]).await;
    assert!(node2.dht().successors().list()?.contains(&node1.did()));
    assert!(node1.dht().successors().list()?.contains(&node2.did()));

    Ok(())
}

#[tokio::test]
async fn test_handle_find_successor_decrease() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    // key 2 > key 1 here
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_successor(&node2, node1.did()).await?;
    wait_for_finger(&node1, node2.did()).await?;
    wait_for_finger(&node2, node1.did()).await?;
    node1
        .swarm
        .send_message(
            Message::NotifyPredecessorSend(message::NotifyPredecessorSend { did: node1.did() }),
            node2.did(),
        )
        .await
        .unwrap();
    wait_for_predecessor(&node2, node1.did()).await?;
    assert!(node1.dht().successors().list()?.contains(&node2.did()));
    println!("node1: {:?}, node2: {:?}", node1.did(), node2.did());
    node2
        .swarm
        .send_message(
            Message::FindSuccessorSend(message::FindSuccessorSend {
                did: node2.did(),
                then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                strict: true,
            }),
            node1.did(),
        )
        .await
        .unwrap();
    wait_for_msgs([&node1, &node2]).await;
    let dht1_successor = node1.dht().successors();
    let dht2_successor = node2.dht().successors();
    assert!(dht2_successor.list()?.contains(&node1.did()));
    assert!(dht1_successor.list()?.contains(&node2.did()));

    Ok(())
}

#[tokio::test]
async fn test_handle_storage() -> Result<()> {
    // random key may failed here, because if key1 is more close to virtual_peer
    // key2 will try send msg back to key1
    let key1 =
        SecretKey::from_str("ff3e0ea83de6909db79f3452764a24efb25c86c1e85c7c453d903c0cf462df07")
            .unwrap();
    let key2 =
        SecretKey::from_str("f782f6b07ae0151b5f83ff49f46087a7a45eb5c97d210c907a2b52ffece4be69")
            .unwrap();
    println!(
        "test with key1: {:?}, key2: {:?}",
        key1.address(),
        key2.address()
    );
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    // node1's successor is node2
    // node2's successor is node1
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_successor(&node2, node1.did()).await?;
    node1
        .swarm
        .send_message(
            Message::NotifyPredecessorSend(message::NotifyPredecessorSend { did: node1.did() }),
            node2.did(),
        )
        .await
        .unwrap();
    wait_for_predecessor(&node2, node1.did()).await?;
    assert!(node1.dht().successors().list()?.contains(&node2.did()));

    assert!(node2.dht().storage.count().await.unwrap() == 0);
    let message = String::from("this is a test string");
    let encoded_message = message.encode().unwrap();
    // the entry_key is hash of string
    let entry: Entry = (message.clone(), encoded_message).try_into().unwrap();
    node1
        .swarm
        .send_message(
            Message::OperateEntry(PlacedEntryOperation {
                placement: entry.did,
                op: EntryOperation::Overwrite(entry.clone()),
            }),
            node2.did(),
        )
        .await
        .unwrap();
    let data = wait_for_storage_entry(&node2, StorageKey::new(EntryKind::Data, entry.did)).await?;
    assert!(node1.dht().storage.count().await.unwrap() == 0);
    assert!(node2.dht().storage.count().await.unwrap() > 0);
    assert_eq!(data.data[0].clone().decode::<String>().unwrap(), message);
    Ok(())
}
