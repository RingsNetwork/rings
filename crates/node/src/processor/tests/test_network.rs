use super::common::*;
use super::*;

#[tokio::test]
async fn online_node_registry_lists_two_publishers_over_network() -> Result<()> {
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

    assert!(nodes.iter().all(OnlineNodeDescriptor::verify_signature));
    assert!(other_nodes
        .iter()
        .all(OnlineNodeDescriptor::verify_signature));
    Ok(())
}

#[tokio::test]
async fn online_node_type_is_configurable() {
    let processor = prepare_processor_with_online_node_type(OnlineNodeType::Browser).await;
    let descriptor = processor.online_node_descriptor_at(get_epoch_ms()).unwrap();

    assert_eq!(descriptor.node_type, OnlineNodeType::Browser);
}

#[tokio::test]
async fn test_processor_create_offer() {
    let peer_did = SecretKey::random().address().into();
    let processor = prepare_processor().await;
    processor.swarm.create_offer(peer_did).await.unwrap();
    let conn_dids = processor.swarm.peers();
    assert_eq!(conn_dids.len(), 1);
    assert_eq!(conn_dids.first().unwrap().did, peer_did.to_string());
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
    assert_eq!(
        p1.swarm
            .peers()
            .into_iter()
            .find(|peer| peer.did == p2.did().to_string())
            .unwrap()
            .state,
        "New"
    );

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
async fn peer_measurement_is_absent_without_measure_or_observation() {
    let unmeasured = prepare_processor_with_identity_key(SecretKey::random()).await;
    let unseen_did = SecretKey::random().address().into();
    assert!(unmeasured.peer_measurement(unseen_did).await.is_none());

    let measured = prepare_measured_processor().await;
    assert!(measured.peer_measurement(unseen_did).await.is_none());
    assert!(measured.peer_measurements().await.is_empty());
}

#[tokio::test]
async fn provider_exposes_sent_and_received_peer_measurements() {
    let _network_guard = network_test_guard().await;
    let callback1 = test_callback();
    let callback2 = test_callback();
    let p1 = prepare_measured_processor().await;
    let p2 = prepare_measured_processor().await;

    p1.swarm.set_callback(callback1.clone()).unwrap();
    p2.swarm.set_callback(callback2.clone()).unwrap();
    connect_processors(&p1, &p2, &callback1, &callback2).await;

    p1.send_message(p2.did(), b"measure-provider")
        .await
        .unwrap();
    let got_msg2 = wait_for_inbound_message(
        &callback2,
        |msg| matches!(msg, Message::CustomMessage(custom) if custom.0 == b"measure-provider"),
    )
    .await;
    assert!(matches!(got_msg2, Message::CustomMessage(_)));

    let sent =
        wait_for_peer_measurement(&p1, p2.did(), |measurement| measurement.evidence.sent >= 1)
            .await;
    let received = wait_for_peer_measurement(&p2, p1.did(), |measurement| {
        measurement.evidence.received >= 1
    })
    .await;
    assert_eq!(sent.did, p2.did());
    assert_eq!(received.did, p1.did());

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
    assert!(rpc_measurement
        .measurement
        .as_ref()
        .is_some_and(|measurement| measurement.counters.sent >= 1));

    let list_value = provider
        .request(Method::ListPeerMeasurements, ListPeerMeasurementsRequest {})
        .await
        .unwrap();
    let list_measurements: ListPeerMeasurementsResponse =
        serde_json::from_value(list_value).unwrap();
    let p2_did_json = serde_json::to_value(p2.did()).unwrap();
    assert!(list_measurements
        .measurements
        .iter()
        .any(|measurement| measurement.did == p2_did_json && measurement.counters.sent >= 1));
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
    let p2 = prepare_processor_with_identity_key(identity2).await;

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
