use futures::FutureExt;

use super::common::*;
use super::*;
#[cfg(feature = "dummy")]
use crate::consts::DATA_REDUNDANT;

const LISTENER_STOP_TIMEOUT: Duration = Duration::from_secs(2);

#[tokio::test]
async fn test_listen_with_pre_stopped_token_returns_before_first_tick() {
    let processor = prepare_processor().await;
    let stop = StopSource::new();
    stop.request_stop();

    // Completing on the first poll means the listener waited for nothing, the first
    // stabilization tick included; this is decided by state, not by a timeout.
    assert!(
        processor.listen_with(stop.token()).now_or_never().is_some(),
        "pre-stopped listen token should exit before the first stabilization tick"
    );
}

#[tokio::test]
async fn test_provider_listen_with_pre_stopped_token_returns_before_first_tick() {
    let processor = prepare_processor().await;
    let provider = Provider::from_processor(Arc::new(processor));
    let stop = StopSource::new();
    stop.request_stop();

    // Completing on the first poll means the listener waited for nothing; see above.
    assert!(
        provider.listen_with(stop.token()).now_or_never().is_some(),
        "pre-stopped provider listen token should exit before the first stabilization tick"
    );
}

/// A started provider listener returns after its token is stopped.
///
/// The listener's first poll acquires the lifecycle lock and starts; `Pending` then proves it
/// is running and waiting (on its tick or its stop token), so the stop is requested after the
/// start as a state, not after a sleep.
#[tokio::test]
async fn test_provider_listen_with_started_token_returns_after_stop() {
    let processor = prepare_processor().await;
    let provider = Provider::from_processor(Arc::new(processor));
    let stop = StopSource::new();
    let mut listen = std::pin::pin!(provider.listen_with(stop.token()));
    assert!(
        futures::poll!(listen.as_mut()).is_pending(),
        "a started listener runs until it is stopped"
    );
    stop.request_stop();

    tokio::time::timeout(LISTENER_STOP_TIMEOUT, listen)
        .await
        .expect("started provider listen token should exit after stop");
}

/// Cloned processor handles queue listener starts and preserve restart after cleanup.
#[tokio::test]
async fn test_listener_generation_queues_cancelled_starts_and_restarts() {
    let processor = Arc::new(prepare_processor().await);
    let _first_provider = Provider::from_processor(processor.clone());
    let _second_provider = Provider::from_processor(processor.clone());
    assert!(Arc::ptr_eq(
        &processor.listener_lifecycle_lock_for_test(),
        &processor.clone().listener_lifecycle_lock_for_test(),
    ));

    let first_stop = StopSource::new();
    let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
    let first_processor = processor.clone();
    let first_token = first_stop.token();
    let first = tokio::spawn(async move {
        first_processor
            .listen_with_started(first_token, move || {
                let _sent = first_started_tx.send(());
            })
            .await;
    });
    first_started_rx
        .await
        .expect("the first generation should acquire processor ownership");

    let queued_stop = StopSource::new();
    let queued_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let queued_started_in_task = queued_started.clone();
    let queued_processor = processor.clone();
    let queued_token = queued_stop.token();
    let mut queued = Box::pin(async move {
        queued_processor
            .listen_with_started(queued_token, move || {
                queued_started_in_task.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await;
    });
    // Cancelling before ownership does not bypass the queue or let cleanup overlap: polled
    // after the cancellation, the queued generation is still waiting for the lock and has not
    // started. Its poll runs every step that needs no timer, so this is decided by state.
    queued_stop.request_stop();
    assert!(futures::poll!(queued.as_mut()).is_pending());
    assert!(!queued_started.load(std::sync::atomic::Ordering::SeqCst));
    let queued = tokio::spawn(queued);

    first_stop.request_stop();
    tokio::time::timeout(LISTENER_STOP_TIMEOUT, first)
        .await
        .expect("the active generation should finish graceful cleanup")
        .expect("the active listener task should not panic");
    tokio::time::timeout(LISTENER_STOP_TIMEOUT, queued)
        .await
        .expect("the queued cancelled generation should acquire then finish")
        .expect("the queued listener task should not panic");
    // A pre-cancelled token still acquires ownership in queue order before returning.
    assert!(queued_started.load(std::sync::atomic::Ordering::SeqCst));
    let restart_stop = StopSource::new();
    let restart_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let restart_started_in_task = restart_started.clone();
    let restart_processor = processor.clone();
    let restart_token = restart_stop.token();
    let mut restart = Box::pin(async move {
        restart_processor
            .listen_with_started(restart_token, move || {
                restart_started_in_task.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await;
    });
    // The lock is free after cleanup, so the first poll acquires it and starts listening.
    assert!(futures::poll!(restart.as_mut()).is_pending());
    assert!(restart_started.load(std::sync::atomic::Ordering::SeqCst));
    // Stopping the finished generation's token again must not stop the restarted one. Polled
    // after the stale stop, the restarted listener runs every step that needs no timer: had the
    // stale token reached it, its cleanup would run and the poll would complete. This is
    // decided by state, not by how many scheduler turns elapse.
    first_stop.request_stop();
    assert!(futures::poll!(restart.as_mut()).is_pending());
    restart_stop.request_stop();
    tokio::time::timeout(LISTENER_STOP_TIMEOUT, restart)
        .await
        .expect("the restarted generation should clean up");
}

/// Provider clones and independent wrappers over one processor queue cancelled starts.
#[tokio::test]
async fn test_provider_wrappers_share_listener_lifecycle_lock() {
    let processor = Arc::new(prepare_processor().await);
    let original_provider = Provider::from_processor(processor.clone());
    let cloned_provider = original_provider.clone();
    let independent_provider = Provider::from_processor(processor.clone());

    let active_stop = StopSource::new();
    let active_token = active_stop.token();
    let mut active = Box::pin(async move {
        original_provider.listen_with(active_token).await;
    });
    // The first poll acquires the lifecycle lock and starts listening; `Pending` proves the
    // first provider owns the processor listener before the others queue.
    assert!(futures::poll!(active.as_mut()).is_pending());
    assert!(processor
        .listener_lifecycle_lock_for_test()
        .try_lock()
        .is_none());

    let cloned_stop = StopSource::new();
    cloned_stop.request_stop();
    let cloned_token = cloned_stop.token();
    let mut cloned = Box::pin(async move {
        cloned_provider.listen_with(cloned_token).await;
    });

    let independent_stop = StopSource::new();
    independent_stop.request_stop();
    let independent_token = independent_stop.token();
    let mut independent = Box::pin(async move {
        independent_provider.listen_with(independent_token).await;
    });

    // Pre-cancelled starts through a clone and an independent wrapper still queue behind the
    // active listener: polled now, each waits for the shared lock.
    assert!(futures::poll!(cloned.as_mut()).is_pending());
    assert!(futures::poll!(independent.as_mut()).is_pending());

    let active = tokio::spawn(active);
    let cloned = tokio::spawn(cloned);
    let independent = tokio::spawn(independent);

    active_stop.request_stop();
    tokio::time::timeout(LISTENER_STOP_TIMEOUT, active)
        .await
        .expect("the active provider listener should finish graceful cleanup")
        .expect("the active provider listener should not panic");
    tokio::time::timeout(LISTENER_STOP_TIMEOUT, cloned)
        .await
        .expect("the cancelled clone listener should acquire ownership then finish")
        .expect("the cloned provider listener should not panic");
    tokio::time::timeout(LISTENER_STOP_TIMEOUT, independent)
        .await
        .expect("the cancelled independent wrapper should acquire ownership then finish")
        .expect("the independent provider listener should not panic");
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_online_node_registry_lists_two_publishers_over_network() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let (publisher, owner) = prepare_online_node_registry_pair(42).await?;
    let callback = test_callback();
    let other_callback = test_callback();
    publisher.swarm.set_callback(callback.clone()).unwrap();
    owner.swarm.set_callback(other_callback.clone()).unwrap();
    connect_processors(&publisher, &owner).await;
    wait_for_mutual_dht_topology(&publisher, &owner).await?;
    let registry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
    let placement_keys = registry_key.rotate_affine(DATA_REDUNDANT)?;
    for placement_key in placement_keys.as_slice() {
        assert!(!owns_entry_placement(&publisher, *placement_key)?);
        assert!(owns_entry_placement(&owner, *placement_key)?);
    }

    let published = publisher.publish_online_node_descriptor().await?;
    let mut expected = BTreeSet::from([published.did]);
    wait_for_online_node_dids_in_storage(
        &owner,
        placement_keys.as_slice(),
        &expected,
        "owner stores publisher publish",
    )
    .await?;

    let owner_published = owner.publish_online_node_descriptor().await?;
    expected.insert(owner_published.did);
    wait_for_online_node_dids_in_storage(
        &owner,
        placement_keys.as_slice(),
        &expected,
        "owner stores both publishers at every placement",
    )
    .await?;
    // The owner holds both descriptors at every placement, so one lookup from each node is
    // decided: no retry, hence no lookup of the test's own that could wake another.
    let other_nodes = owner.lookup_online_nodes(false).await?;
    let nodes = publisher.lookup_online_nodes(false).await?;
    for (lookup, observed) in [("owner", &other_nodes), ("publisher", &nodes)] {
        let observed = observed
            .iter()
            .map(|descriptor| descriptor.did)
            .collect::<BTreeSet<_>>();
        assert!(
            expected.is_subset(&observed),
            "the {lookup}'s lookup lists both publishers: expected {expected:?}, got {observed:?}"
        );
    }

    assert!(nodes
        .iter()
        .all(|descriptor| descriptor.verify_signature(publisher.swarm.network_id())));
    assert!(other_nodes
        .iter()
        .all(|descriptor| descriptor.verify_signature(owner.swarm.network_id())));
    Ok(())
}

#[tokio::test]
async fn test_online_node_type_is_configurable() {
    let processor = prepare_processor_with_online_node_type(OnlineNodeType::Browser).await;
    let descriptor = processor.online_node_descriptor_at(get_epoch_ms()).unwrap();

    assert_eq!(descriptor.node_type, OnlineNodeType::Browser);
}

/// Real-transport smoke test: a processor handshake over webrtc-rs reaches admission.
///
/// This runs in every build: in the default build over real webrtc-rs with host-only ICE,
/// serialized by the network lock. It checks what only the real transport can: that the
/// offer/answer SDP, ICE, DTLS and SCTP handshake of a processor completes and is admitted.
/// Protocol logic over an admitted link is tested on the controlled network (`dummy` build).
/// Admission is awaited on activity (the `Connected` event); the hang guard only bounds a hang.
#[tokio::test]
async fn test_processor_create_offer() {
    let _network_guard = network_test_guard().await;
    let callback1 = test_callback();
    let callback2 = test_callback();
    let p1 = prepare_processor().await;
    let p2 = prepare_processor().await;

    p1.swarm.set_callback(callback1.clone()).unwrap();
    p2.swarm.set_callback(callback2.clone()).unwrap();

    let offer = p1.swarm.create_offer(p2.did()).await.unwrap();
    assert!(p1.swarm.peers().is_empty());

    let answer = p2.swarm.answer_offer(offer).await.unwrap();
    p1.swarm.accept_answer(answer).await.unwrap();
    wait_processors_connected(&p1, &p2).await;

    let conn_dids = p1.swarm.peers();
    assert_eq!(conn_dids.len(), 1);
    assert_eq!(conn_dids.first().unwrap().did, p2.did().to_string());
    assert_eq!(conn_dids.first().unwrap().state, "Connected");
}

/// Real-transport smoke test: custom messages cross an admitted webrtc-rs link both ways.
///
/// Beyond [`test_processor_create_offer`], it checks what only the real transport can: that
/// message frames travel over a real SCTP data channel in both directions. Every wait is an
/// activity-woken probe (admission, then each inbound message); the hang guard only bounds a
/// hang. Wire-byte measurement and chunking against a negotiated SCTP `max_message_size` are
/// not covered at node level (see #883).
#[tokio::test]
async fn test_processor_handshake_msg() {
    let _network_guard = network_test_guard().await;
    let callback1 = test_callback();
    let callback2 = test_callback();

    let p1 = prepare_processor().await;
    let p2 = prepare_processor().await;

    p1.swarm.set_callback(callback1.clone()).unwrap();
    p2.swarm.set_callback(callback2.clone()).unwrap();

    let did1 = p1.did();
    let did2 = p2.did();

    let offer = p1.swarm.create_offer(p2.did()).await.unwrap();
    assert!(p1.swarm.peers().is_empty());

    let answer = p2.swarm.answer_offer(offer).await.unwrap();
    p1.swarm.accept_answer(answer).await.unwrap();
    wait_processors_connected(&p1, &p2).await;

    let test_text1 = "test1";
    let test_text2 = "test2";

    p1.send_message(did2, test_text1.as_bytes()).await.unwrap();
    p2.send_message(did1, test_text2.as_bytes()).await.unwrap();

    let got_msg2 = wait_for_inbound_message(
        &callback2,
        |msg| matches!(msg, Message::CustomMessage(custom) if custom.0 == test_text1.as_bytes()),
    )
    .await;
    assert!(matches!(got_msg2, Message::CustomMessage(_)));

    let got_msg1 = wait_for_inbound_message(
        &callback1,
        |msg| matches!(msg, Message::CustomMessage(custom) if custom.0 == test_text2.as_bytes()),
    )
    .await;
    assert!(matches!(got_msg1, Message::CustomMessage(_)));
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_processor_direct_message_reaches_connected_peer() {
    let _network_guard = network_test_guard().await;
    let callback1 = test_callback();
    let callback2 = test_callback();
    let p1 = prepare_processor().await;
    let p2 = prepare_processor().await;

    p1.swarm.set_callback(callback1.clone()).unwrap();
    p2.swarm.set_callback(callback2.clone()).unwrap();
    connect_processors(&p1, &p2).await;

    p1.send_direct_message(p2.did(), b"direct-message")
        .await
        .unwrap();

    let received = wait_for_inbound_message(
        &callback2,
        |message| matches!(message, Message::CustomMessage(custom) if custom.0 == b"direct-message"),
    )
    .await;
    assert!(matches!(received, Message::CustomMessage(_)));
}

#[tokio::test]
async fn test_peer_measurement_is_absent_without_measure_or_observation() {
    let unmeasured = prepare_processor_with_identity_key(SecretKey::random()).await;
    let unseen_did = SecretKey::random().address().into();
    assert!(unmeasured.peer_measurement(unseen_did).await.is_none());

    let measured = prepare_measured_processor().await;
    assert!(measured.peer_measurement(unseen_did).await.is_none());
    assert!(measured.peer_measurements().await.is_empty());
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_provider_exposes_sent_and_received_peer_measurements() {
    let _network_guard = network_test_guard().await;
    let callback1 = test_callback();
    let callback2 = test_callback();
    let p1 = prepare_measured_processor().await;
    let p2 = prepare_measured_processor().await;

    p1.swarm.set_callback(callback1.clone()).unwrap();
    p2.swarm.set_callback(callback2.clone()).unwrap();
    connect_processors(&p1, &p2).await;
    let sent_before = p1.peer_measurement(p2.did()).await.unwrap();
    let received_before = p2.peer_measurement(p1.did()).await.unwrap();
    let sent_bytes_before = sent_before.credit.bytes_sent_to_peer();
    let received_bytes_before = received_before.credit.bytes_received_from_peer();

    p1.swarm
        .send_direct_message(Message::custom(b"measure-provider").unwrap(), p2.did())
        .await
        .unwrap();
    let got_msg2 = wait_for_inbound_message(
        &callback2,
        |msg| matches!(msg, Message::CustomMessage(custom) if custom.0 == b"measure-provider"),
    )
    .await;
    assert!(matches!(got_msg2, Message::CustomMessage(_)));

    let sent = wait_for_peer_measurement(&p1, p2.did(), |measurement| {
        measurement.credit.bytes_sent_to_peer() > sent_bytes_before
    })
    .await;
    let received = wait_for_peer_measurement(&p2, p1.did(), |measurement| {
        measurement.credit.bytes_received_from_peer() > received_bytes_before
    })
    .await;
    assert_eq!(sent.did, p2.did());
    assert_eq!(received.did, p1.did());
    let sent_credit = sent.credit;
    let received_credit = received.credit;
    let sent_delta = sent_credit.bytes_sent_to_peer() - sent_bytes_before;
    let received_delta = received_credit.bytes_received_from_peer() - received_bytes_before;
    assert!(sent_delta > b"measure-provider".len() as u64);
    assert!(received_delta > b"measure-provider".len() as u64);

    let node_info = p1.get_node_info().await.unwrap();
    assert_eq!(node_info.version, crate::util::build_version());
    assert!(node_info.swarm.is_some());

    let provider = Provider::from_processor(Arc::new(p1));
    let provider_measurement = provider.peer_measurement(p2.did()).await.unwrap();
    assert!(provider_measurement.evidence.sent >= 1);

    let rpc_value = provider
        .request(Method::PeerMeasurement, PeerMeasurementRequest {
            did: p2.did().to_string(),
        })
        .await
        .unwrap();
    let rpc_measurement: PeerMeasurementResponse = serde_json::from_value(rpc_value).unwrap();
    let rpc_measurement = rpc_measurement
        .measurement
        .as_ref()
        .expect("peer measurement RPC entry");
    assert!(rpc_measurement.counters.sent >= sent.evidence.sent);
    assert!(
        rpc_measurement
            .credit
            .as_ref()
            .expect("peer credit RPC entry")
            .bytes_sent_to_peer
            >= sent_credit.bytes_sent_to_peer()
    );

    let list_value = provider
        .request(Method::ListPeerMeasurements, ListPeerMeasurementsRequest {
            limit: Some(100),
            cursor: None,
        })
        .await
        .unwrap();
    let list_measurements: ListPeerMeasurementsResponse =
        serde_json::from_value(list_value).unwrap();
    let p2_did_json = serde_json::to_value(p2.did()).unwrap();
    assert!(list_measurements.measurements.iter().any(|measurement| {
        measurement.did == p2_did_json
            && measurement
                .credit
                .as_ref()
                .is_some_and(|credit| credit.bytes_sent_to_peer >= sent_credit.bytes_sent_to_peer())
    }));
    assert!(list_measurements.next_cursor.is_none());
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_processor_e2e_handshake_exchanges_verified_public_keys() {
    let _network_guard = network_test_guard().await;
    let callback1 = test_callback();
    let callback2 = test_callback();

    let p1 = prepare_processor().await;
    let p2 = prepare_processor().await;

    p1.swarm.set_callback(callback1.clone()).unwrap();
    p2.swarm.set_callback(callback2.clone()).unwrap();

    connect_processors(&p1, &p2).await;

    let did1 = p1.did();
    let did2 = p2.did();
    let requester_public_key = p1.swarm.delegator_pubkey().unwrap();
    let responder_public_key = p2.swarm.delegator_pubkey().unwrap();

    p1.send_e2e_handshake(did2).await.unwrap();

    let request = wait_for_inbound_message(&callback2, |msg| {
        matches!(msg, Message::E2eHandshakeRequest(_))
    })
    .await;
    match request {
        Message::E2eHandshakeRequest(request) => {
            assert_eq!(request.requester_public_key, requester_public_key);
            assert_eq!(
                p2.verify_e2e_handshake_request(did1, &request).unwrap(),
                requester_public_key
            );
        }
        msg => panic!("expected E2eHandshakeRequest, got {msg:?}"),
    }

    let response = wait_for_inbound_message(&callback1, |msg| {
        matches!(msg, Message::E2eHandshakeResponse(_))
    })
    .await;
    match response {
        Message::E2eHandshakeResponse(response) => {
            assert_eq!(response.responder_public_key, responder_public_key);
            assert_eq!(
                p1.verify_e2e_handshake_response(did2, &response).unwrap(),
                responder_public_key
            );
        }
        msg => panic!("expected E2eHandshakeResponse, got {msg:?}"),
    }
}

/// Secret of the sender key on [`e2e_test_frame`]; any fixed key serves, since only sequence
/// and finality matter to the predicate under test.
const E2E_TEST_FRAME_SECRET: &str =
    "0101010101010101010101010101010101010101010101010101010101010101";

/// A stream frame at `sequence` with no payload; only sequence and finality matter here, so the
/// stream id and sender key are fixed.
fn e2e_test_frame(sequence: u64, is_final: bool) -> E2eStreamFrame {
    E2eStreamFrame {
        stream_id: uuid::Uuid::nil(),
        sender_public_key: SecretKey::try_from(E2E_TEST_FRAME_SECRET).unwrap().pubkey(),
        sequence,
        is_final,
        ciphertext: Vec::new(),
    }
}

/// Every ordering of `items`.
fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    if items.is_empty() {
        return vec![Vec::new()];
    }
    (0..items.len())
        .flat_map(|chosen| {
            let mut rest = items.to_vec();
            let head = rest.remove(chosen);
            permutations(rest.as_slice())
                .into_iter()
                .map(move |mut tail| {
                    tail.insert(0, head.clone());
                    tail
                })
        })
        .collect()
}

/// `e2e_stream_complete` is insensitive to arrival order, and monotone.
///
/// ```text
/// ∀ π ∈ Perm({0, 1, 2, 3ᶠ}).
///   ∀ k < 4. ¬complete(π[..k])                     (some sequence ≤ 3 is missing)
///   complete(π)
///   complete(π ++ [π₀, 4])                          (a duplicate and a stray frame)
/// ```
///
/// All 24 arrival orders are checked. Every proper prefix of a permutation of four distinct
/// sequences lacks one of them, so it is incomplete whether or not it holds the final frame.
#[test]
fn test_e2e_stream_complete_is_order_insensitive_and_monotone() {
    let stream = [
        e2e_test_frame(0, false),
        e2e_test_frame(1, false),
        e2e_test_frame(2, false),
        e2e_test_frame(3, true),
    ];
    let orders = permutations(stream.as_slice());
    assert_eq!(orders.len(), 24);
    for order in orders {
        for delivered in 0..order.len() {
            assert!(
                !e2e_stream_complete(order.iter().take(delivered)),
                "{delivered} arrivals leave a sequence below the final frame missing"
            );
        }
        assert!(e2e_stream_complete(order.iter()));
        let mut extended = order.clone();
        extended.push(order[0].clone());
        extended.push(e2e_test_frame(4, false));
        assert!(
            e2e_stream_complete(extended.iter()),
            "later duplicates and stray frames keep a complete stream complete"
        );
    }
}

/// Secrets of the two E2E stream test identities; fixed, so the run is reproducible.
#[cfg(feature = "dummy")]
const E2E_STREAM_TEST_SECRETS: [&str; 2] = [
    "0303030303030303030303030303030303030303030303030303030303030303",
    "0404040404040404040404040404040404040404040404040404040404040404",
];

/// E2E streaming on the controlled network, delivered in reverse, then decrypted with the
/// receiver's identity key.
///
/// ```text
/// Admitted ≡ p1 ∈ peers(p2) ∧ p2 ∈ peers(p1)
/// Complete ≡ e2e_stream_complete(inbound(p2, stream))
///
/// connect(p1, p2)                          ⊢ ◇Admitted    (FIFO pump; no clock)
/// Admitted ; pause ; send(p1)              ⊢ all frames queued, none delivered
/// deliver newest-first until Complete      ⊢ Complete, with arrival order ≠ send order
/// ```
///
/// The link makes no ordering guarantee (#738, #784). Here the reordering is not left to
/// chance: with the pump paused, the whole stream is queued and then delivered newest-first,
/// so the final frame arrives before every earlier one. `Complete` is monotone, so it stays
/// true once reached. The shape assertions run on the raw frames in arrival order: exactly one
/// final frame, and the sorted sequences are exactly `0..n` (no gap, duplicate or post-final
/// frame). The test also asserts that arrival order differs from send order, so the
/// reordering really happened, and decrypts in arrival order.
///
/// The run is a deterministic function of the controlled queue: fixed identities, a seeded
/// dummy, and no clock or network (#857, #883).
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_processor_e2e_message_streams_and_decrypts_with_receiver_identity_key() {
    let network_guard = network_test_guard().await;
    let callback1 = test_callback();
    let callback2 = test_callback();
    let [identity1, identity2] =
        E2E_STREAM_TEST_SECRETS.map(|secret| SecretKey::try_from(secret).unwrap());

    let p1 = prepare_processor_with_identity_key(identity1).await;
    let p2 = prepare_processor_with_identity_key(identity2.clone()).await;

    p1.swarm.set_callback(callback1.clone()).unwrap();
    p2.swarm.set_callback(callback2.clone()).unwrap();

    connect_processors(&p1, &p2).await;

    let did1 = p1.did();
    let did2 = p2.did();
    let responder_public_key = p2.swarm.delegator_pubkey().unwrap();
    network_guard.network.pause();
    let stream_id = p1
        .send_e2e_message_with_frame_len(
            did2,
            responder_public_key,
            b"homomorphic-ready streaming body",
            8,
        )
        .await
        .unwrap();
    network_guard
        .network
        .deliver_newest_until("E2E stream complete", || {
            e2e_stream_complete(received_e2e_stream_frames(&callback2, stream_id).iter())
        })
        .await;
    network_guard.network.resume();

    let frames = received_e2e_stream_frames(&callback2, stream_id);
    assert!(
        frames.len() > 1,
        "streaming send should emit more than one frame for this frame size"
    );
    assert_eq!(
        frames.iter().filter(|frame| frame.is_final).count(),
        1,
        "streaming send should emit exactly one final frame"
    );
    let arrival = frames
        .iter()
        .map(|frame| frame.sequence)
        .collect::<Vec<_>>();
    let mut sequences = arrival.clone();
    sequences.sort_unstable();
    let frame_count = u64::try_from(frames.len()).unwrap();
    assert_eq!(sequences, (0..frame_count).collect::<Vec<_>>());
    assert_ne!(
        arrival, sequences,
        "newest-first delivery must reorder the stream"
    );

    let mut decryptor = p2.e2e_stream_decryptor(did1, stream_id, identity2).unwrap();
    let mut plaintext = Vec::new();
    for frame in &frames {
        plaintext.extend_from_slice(&p2.decrypt_e2e_stream_frame(&mut decryptor, frame).unwrap());
    }
    decryptor.finish().unwrap();
    assert_eq!(plaintext, b"homomorphic-ready streaming body");

    assert!(matches!(
        p2.e2e_stream_decryptor(did1, stream_id, SecretKey::random()),
        Err(Error::CoreError(
            rings_core::error::Error::E2ePublicKeyDidMismatch { .. }
        ))
    ));
}
