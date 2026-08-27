use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::*;

#[test]
fn test_replace_fingers_for_test_rejects_invalid_input_before_mutation() -> Result<()> {
    let local = Did::from(0u32);
    let peer = Did::from(1u32);
    let node =
        PeerRing::new_with_storage_and_finger_table_size(local, 3, Box::new(MemStorage::new()), 4);
    node.replace_fingers_for_test(&[(0, peer)])?;
    let expected = node.lock_finger()?.list().clone();

    assert!(node.replace_fingers_for_test(&[(4, peer)]).is_err());
    assert_eq!(node.lock_finger()?.list(), &expected);
    assert!(node.replace_fingers_for_test(&[(1, local)]).is_err());
    assert_eq!(node.lock_finger()?.list(), &expected);
    Ok(())
}

#[test]
fn test_topology_transitions_serialize_remove_and_notify() -> Result<()> {
    let local = Did::from(0u32);
    let removed = Did::from(10u32);
    let predecessor = Did::from(20u32);
    let node = Arc::new(PeerRing::new_with_storage(
        local,
        3,
        Box::new(MemStorage::new()),
    ));
    node.join(removed)?;

    let (snapshot_tx, snapshot_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let remove_node = Arc::clone(&node);
    let remove_thread = thread::spawn(move || {
        remove_node
            .transition_topology_with_observer(
                TopologyEvent::Remove {
                    peer: removed,
                    successor: SuccessorRemoval::Preserve,
                },
                |_| {
                    let _ = snapshot_tx.send(());
                    let _ = release_rx.recv();
                },
            )
            .map(|_| ())
    });
    snapshot_rx
        .recv_timeout(Duration::from_secs(1))
        .map_err(|error| {
            Error::InvalidMessage(format!("remove snapshot was not observed: {error}"))
        })?;

    let (notify_started_tx, notify_started_rx) = mpsc::sync_channel(0);
    let (notify_done_tx, notify_done_rx) = mpsc::channel();
    let notify_node = Arc::clone(&node);
    let notify_thread = thread::spawn(move || {
        let _ = notify_started_tx.send(());
        let result = notify_node.notify(predecessor);
        let _ = notify_done_tx.send(());
        result
    });
    notify_started_rx
        .recv_timeout(Duration::from_secs(1))
        .map_err(|error| Error::InvalidMessage(format!("notify did not start: {error}")))?;
    assert!(notify_done_rx
        .recv_timeout(Duration::from_millis(25))
        .is_err());

    release_tx
        .send(())
        .map_err(|error| Error::InvalidMessage(format!("remove release failed: {error}")))?;
    remove_thread
        .join()
        .map_err(|_| Error::InvalidMessage("remove transition thread panicked".to_string()))??;
    assert_eq!(
        notify_thread.join().map_err(|_| {
            Error::InvalidMessage("notify transition thread panicked".to_string())
        })??,
        predecessor
    );

    let state = node.topology_state()?;
    assert!(!state.successors.contains(&removed));
    assert!(!state.fingers.contains(&Some(removed)));
    assert_eq!(state.predecessor, Some(predecessor));
    Ok(())
}

#[test]
fn test_topology_snapshot_waits_for_complete_transition_commit() -> Result<()> {
    let local = Did::from(0u32);
    let removed = Did::from(10u32);
    let node = Arc::new(PeerRing::new_with_storage(
        local,
        3,
        Box::new(MemStorage::new()),
    ));
    node.join(removed)?;

    let (snapshot_tx, snapshot_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let transition_node = Arc::clone(&node);
    let transition_thread = thread::spawn(move || {
        transition_node
            .transition_topology_with_observer(
                TopologyEvent::Remove {
                    peer: removed,
                    successor: SuccessorRemoval::Preserve,
                },
                |_| {
                    let _ = snapshot_tx.send(());
                    let _ = release_rx.recv();
                },
            )
            .map(|_| ())
    });
    snapshot_rx
        .recv_timeout(Duration::from_secs(1))
        .map_err(|error| {
            Error::InvalidMessage(format!("transition snapshot was not observed: {error}"))
        })?;

    let (read_done_tx, read_done_rx) = mpsc::channel();
    let (read_started_tx, read_started_rx) = mpsc::sync_channel(0);
    let read_node = Arc::clone(&node);
    let read_thread = thread::spawn(move || {
        let _ = read_started_tx.send(());
        let result = read_node.topology_state();
        let _ = read_done_tx.send(());
        result
    });
    read_started_rx
        .recv_timeout(Duration::from_secs(1))
        .map_err(|error| Error::InvalidMessage(format!("topology read did not start: {error}")))?;
    assert!(read_done_rx
        .recv_timeout(Duration::from_millis(25))
        .is_err());

    release_tx
        .send(())
        .map_err(|error| Error::InvalidMessage(format!("transition release failed: {error}")))?;
    transition_thread
        .join()
        .map_err(|_| Error::InvalidMessage("topology transition thread panicked".to_string()))??;
    let state = read_thread
        .join()
        .map_err(|_| Error::InvalidMessage("topology reader thread panicked".to_string()))??;

    assert!(!state.successors.contains(&removed));
    assert!(!state.fingers.contains(&Some(removed)));
    assert_ne!(state.predecessor, Some(removed));
    Ok(())
}

#[test]
fn test_unavailable_successor_promotes_verified_fallback_in_interpreted_state() -> Result<()> {
    let local = Did::from(0u32);
    let removed = Did::from(10u32);
    let unverified_nearer = Did::from(20u32);
    let verified_fallback = Did::from(30u32);
    let verified_tail = Did::from(40u32);
    let node = PeerRing::new_with_storage(local, 4, Box::new(MemStorage::new()));

    for peer in [removed, unverified_nearer, verified_fallback, verified_tail] {
        node.join(peer)?;
    }
    assert_eq!(node.successors().list()?, vec![
        removed,
        unverified_nearer,
        verified_fallback,
        verified_tail
    ]);

    node.remove_unavailable(removed, vec![verified_fallback, verified_tail])?;
    assert_eq!(node.successors().list()?, vec![
        verified_fallback,
        verified_tail
    ]);
    Ok(())
}
