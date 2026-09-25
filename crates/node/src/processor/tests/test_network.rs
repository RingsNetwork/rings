use super::common::*;
use super::*;
use crate::consts::DATA_REDUNDANT;

const LISTENER_START_YIELD: Duration = Duration::from_millis(100);
const LISTENER_STOP_TIMEOUT: Duration = Duration::from_secs(2);

#[tokio::test]
async fn test_listen_with_pre_stopped_token_returns_before_first_tick() {
    let processor = prepare_processor().await;
    let stop = StopSource::new();
    stop.request_stop();

    tokio::time::timeout(
        Duration::from_millis(100),
        processor.listen_with(stop.token()),
    )
    .await
    .expect("pre-stopped listen token should exit before the first stabilization tick");
}

#[tokio::test]
async fn test_provider_listen_with_pre_stopped_token_returns_before_first_tick() {
    let processor = prepare_processor().await;
    let provider = Provider::from_processor(Arc::new(processor));
    let stop = StopSource::new();
    stop.request_stop();

    tokio::time::timeout(
        Duration::from_millis(100),
        provider.listen_with(stop.token()),
    )
    .await
    .expect("pre-stopped provider listen token should exit before the first stabilization tick");
}

#[tokio::test]
async fn test_provider_listen_with_started_token_returns_after_stop() {
    let processor = prepare_processor().await;
    let provider = Provider::from_processor(Arc::new(processor));
    let stop = StopSource::new();
    let listen = provider.listen_with(stop.token());
    let stopper = async {
        tokio::time::sleep(LISTENER_START_YIELD).await;
        stop.request_stop();
    };

    tokio::time::timeout(LISTENER_STOP_TIMEOUT, async {
        futures::join!(listen, stopper);
    })
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
    let queued = tokio::spawn(async move {
        queued_processor
            .listen_with_started(queued_token, move || {
                queued_started_in_task.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await;
    });
    // Cancelling before ownership does not bypass the queue or let cleanup overlap.
    queued_stop.request_stop();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!queued_started.load(std::sync::atomic::Ordering::SeqCst));

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
    let (restart_started_tx, restart_started_rx) = tokio::sync::oneshot::channel();
    let restart_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let restart_finished_in_task = restart_finished.clone();
    let restart_processor = processor.clone();
    let restart_token = restart_stop.token();
    let restart = tokio::spawn(async move {
        restart_processor
            .listen_with_started(restart_token, move || {
                let _sent = restart_started_tx.send(());
            })
            .await;
        restart_finished_in_task.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    restart_started_rx
        .await
        .expect("a new generation should start after cleanup");
    first_stop.request_stop();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!restart_finished.load(std::sync::atomic::Ordering::SeqCst));
    restart_stop.request_stop();
    tokio::time::timeout(LISTENER_STOP_TIMEOUT, restart)
        .await
        .expect("the restarted generation should clean up")
        .expect("the restarted listener task should not panic");
    assert!(restart_finished.load(std::sync::atomic::Ordering::SeqCst));
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
    let active = tokio::spawn(async move {
        original_provider.listen_with(active_token).await;
    });

    // Wait until the first provider has acquired the lifecycle lock before queuing
    // starts through the clone and the independently constructed provider.
    tokio::time::timeout(LISTENER_STOP_TIMEOUT, async {
        loop {
            if processor
                .listener_lifecycle_lock_for_test()
                .try_lock()
                .is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first provider should acquire processor listener ownership");

    let cloned_stop = StopSource::new();
    cloned_stop.request_stop();
    let cloned_token = cloned_stop.token();
    let mut cloned = tokio::spawn(async move {
        cloned_provider.listen_with(cloned_token).await;
    });

    let independent_stop = StopSource::new();
    independent_stop.request_stop();
    let independent_token = independent_stop.token();
    let mut independent = tokio::spawn(async move {
        independent_provider.listen_with(independent_token).await;
    });

    assert!(tokio::time::timeout(Duration::from_millis(20), &mut cloned)
        .await
        .is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut independent)
            .await
            .is_err()
    );

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

#[tokio::test]
async fn test_online_node_registry_lists_two_publishers_over_network() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let (publisher, owner) = prepare_online_node_registry_pair(42).await?;
    let callback = test_callback();
    let other_callback = test_callback();
    publisher.swarm.set_callback(callback.clone()).unwrap();
    owner.swarm.set_callback(other_callback.clone()).unwrap();
    connect_processors(&publisher, &owner, &callback, &other_callback).await;
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
    let other_nodes =
        wait_for_online_node_dids(&owner, &expected, "owner sees both publishers").await?;
    let nodes =
        wait_for_online_node_dids(&publisher, &expected, "publisher sees both publishers").await?;

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
    wait_processors_connected(&p1, &p2, &callback1, &callback2).await;

    let conn_dids = p1.swarm.peers();
    assert_eq!(conn_dids.len(), 1);
    assert_eq!(conn_dids.first().unwrap().did, p2.did().to_string());
    assert_eq!(conn_dids.first().unwrap().state, "Connected");
}

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
    wait_processors_connected(&p1, &p2, &callback1, &callback2).await;

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

#[tokio::test]
async fn test_processor_direct_message_reaches_connected_peer() {
    let _network_guard = network_test_guard().await;
    let callback1 = test_callback();
    let callback2 = test_callback();
    let p1 = prepare_processor().await;
    let p2 = prepare_processor().await;

    p1.swarm.set_callback(callback1.clone()).unwrap();
    p2.swarm.set_callback(callback2.clone()).unwrap();
    connect_processors(&p1, &p2, &callback1, &callback2).await;

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

#[tokio::test]
async fn test_provider_exposes_sent_and_received_peer_measurements() {
    let _network_guard = network_test_guard().await;
    let callback1 = test_callback();
    let callback2 = test_callback();
    let p1 = prepare_measured_processor().await;
    let p2 = prepare_measured_processor().await;

    p1.swarm.set_callback(callback1.clone()).unwrap();
    p2.swarm.set_callback(callback2.clone()).unwrap();
    connect_processors(&p1, &p2, &callback1, &callback2).await;
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

#[tokio::test]
async fn test_processor_e2e_handshake_exchanges_verified_public_keys() {
    let _network_guard = network_test_guard().await;
    let callback1 = test_callback();
    let callback2 = test_callback();

    let p1 = prepare_processor().await;
    let p2 = prepare_processor().await;

    p1.swarm.set_callback(callback1.clone()).unwrap();
    p2.swarm.set_callback(callback2.clone()).unwrap();

    connect_processors(&p1, &p2, &callback1, &callback2).await;

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

/// E2E streaming over a real link, decrypted with the receiver's identity key.
///
/// ```text
/// Admitted ≡ p1 ∈ peers(p2) ∧ p2 ∈ peers(p1)
/// Complete ≡ e2e_stream_complete(inbound(p2, stream))
///
/// connect(p1, p2)       ⊢ ◇Admitted     awaited on `connected_notify`
/// Admitted ; send(p1)   ⊢ ◇Complete     awaited on `inbound_notify`
/// ```
///
/// Both notifies are `notify_one`, which stores a permit when no task waits, so a wake-up
/// between a scan and the next wait is not lost. `Complete` is stable: frames are only
/// appended, and the predicate is monotone. Arrival order is irrelevant to it, since the link
/// does not guarantee order.
///
/// The helper returns the raw frames in arrival order, so the shape assertions below are
/// about what the sender emitted, among the frames that arrived by completion: exactly one
/// final frame, and the sorted sequences are exactly `0..n`, which rules out gaps, and any
/// duplicate or post-final frame that arrived before completion. A stray frame that arrives
/// after completion is not observed; the processor has no end-of-stream signal to await for
/// it. The frames are then decrypted in reverse arrival order.
///
/// The fixtures use host-only ICE, so the handshake depends on no external server. The link is
/// still real WebRTC under the helpers' 5 s deadline; moving this protocol test onto a
/// controlled transport is tracked in #883.
#[tokio::test]
async fn test_processor_e2e_message_streams_and_decrypts_with_receiver_identity_key() {
    let _network_guard = network_test_guard().await;
    let callback1 = test_callback();
    let callback2 = test_callback();
    let identity1 = SecretKey::random();
    let identity2 = SecretKey::random();

    let p1 = prepare_processor_with_identity_key(identity1).await;
    let p2 = prepare_processor_with_identity_key(identity2.clone()).await;

    p1.swarm.set_callback(callback1.clone()).unwrap();
    p2.swarm.set_callback(callback2.clone()).unwrap();

    connect_processors(&p1, &p2, &callback1, &callback2).await;

    let did1 = p1.did();
    let did2 = p2.did();
    let responder_public_key = p2.swarm.delegator_pubkey().unwrap();
    let stream_id = p1
        .send_e2e_message_with_frame_len(
            did2,
            responder_public_key,
            b"homomorphic-ready streaming body",
            8,
        )
        .await
        .unwrap();

    let frames = wait_for_e2e_stream_frames(&callback2, stream_id).await;
    assert!(
        frames.len() > 1,
        "streaming send should emit more than one frame for this frame size"
    );
    assert_eq!(
        frames.iter().filter(|frame| frame.is_final).count(),
        1,
        "streaming send should emit exactly one final frame"
    );

    let mut sequences = frames
        .iter()
        .map(|frame| frame.sequence)
        .collect::<Vec<_>>();
    sequences.sort_unstable();
    let frame_count = u64::try_from(frames.len()).unwrap();
    assert_eq!(sequences, (0..frame_count).collect::<Vec<_>>());

    let mut decryptor = p2.e2e_stream_decryptor(did1, stream_id, identity2).unwrap();
    let mut plaintext = Vec::new();
    for frame in frames.iter().rev() {
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
