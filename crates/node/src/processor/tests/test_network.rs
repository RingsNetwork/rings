use super::common::*;
use super::*;

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
    let sent_bytes_before = sent_before
        .credit
        .expect("periodic measurement exposes prior credit")
        .bytes_sent_to_peer();
    let received_bytes_before = received_before
        .credit
        .expect("periodic measurement exposes prior credit")
        .bytes_received_from_peer();

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
        measurement
            .credit
            .is_some_and(|credit| credit.bytes_sent_to_peer() > sent_bytes_before)
    })
    .await;
    let received = wait_for_peer_measurement(&p2, p1.did(), |measurement| {
        measurement
            .credit
            .is_some_and(|credit| credit.bytes_received_from_peer() > received_bytes_before)
    })
    .await;
    assert_eq!(sent.did, p2.did());
    assert_eq!(received.did, p1.did());
    let sent_credit = sent.credit.expect("periodic measurement exposes credit");
    let received_credit = received
        .credit
        .expect("periodic measurement exposes credit");
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
    let requester_public_key = p1.swarm.account_pubkey().unwrap();
    let responder_public_key = p2.swarm.account_pubkey().unwrap();

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
    let responder_public_key = p2.swarm.account_pubkey().unwrap();
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
    let mut delivered_frames = frames.clone();
    delivered_frames.reverse();
    for frame in &delivered_frames {
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
