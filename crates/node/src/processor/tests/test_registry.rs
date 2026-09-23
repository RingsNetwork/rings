use rings_core::message::MessageSigner;

use super::common::*;
use super::*;

struct StoppedRegistration;

struct CountingRegistration(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl RegistrationTask for StoppedRegistration {
    fn name(&self) -> &'static str {
        "stopped-test"
    }

    fn interval(&self) -> Duration {
        Duration::from_millis(20)
    }

    async fn register_once(&self, _context: &RegistrationContext<'_>) -> Result<()> {
        Err(Error::RegistrationStopped)
    }
}

#[async_trait]
impl RegistrationTask for CountingRegistration {
    fn name(&self) -> &'static str {
        "counting-test"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    async fn register_once(&self, _context: &RegistrationContext<'_>) -> Result<()> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(())
    }
}

#[tokio::test]
async fn test_registration_daemon_treats_expected_stop_as_terminal() {
    let processor = prepare_processor().await;
    let task = StoppedRegistration;

    tokio::time::timeout(
        Duration::from_millis(100),
        processor.registration_task_daemon_with(&task, StopToken::never(), StopToken::never()),
    )
    .await
    .expect("RegistrationStopped should terminate the registration daemon");
}

#[tokio::test]
async fn test_registration_daemon_stops_after_sibling_maintenance_exits() {
    let processor = prepare_processor().await;
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let task = CountingRegistration(std::sync::Arc::clone(&calls));
    let sibling_stop_source = StopSource::new();
    let daemon = processor.registration_task_daemon_with(
        &task,
        StopToken::never(),
        sibling_stop_source.token(),
    );
    tokio::pin!(daemon);
    while calls.load(std::sync::atomic::Ordering::Acquire) == 0 {
        tokio::select! {
            () = tokio::task::yield_now() => {},
            () = &mut daemon => panic!("registration daemon exited before sibling stop"),
        }
    }

    sibling_stop_source.request_stop();
    tokio::time::timeout(Duration::from_millis(200), &mut daemon)
        .await
        .expect("sibling exit should cooperatively stop registration daemon");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Acquire), 1);
}

#[tokio::test]
async fn test_online_node_descriptor_publishes_and_lists_signed_self() -> Result<()> {
    let processor = prepare_processor().await;
    let published = processor.publish_online_node_descriptor().await?;
    let nodes = processor.lookup_online_nodes(false).await?;

    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].did, processor.did());
    assert_eq!(nodes[0].did, published.did);
    assert_eq!(nodes[0].network_id, processor.swarm.network_id());
    assert_eq!(
        nodes[0].storage_redundancy,
        processor.swarm.storage_redundancy()
    );
    assert_eq!(
        nodes[0].dht_virtual_nodes,
        processor.swarm.dht_virtual_nodes()
    );
    assert!(nodes[0].verify_signature(processor.swarm.network_id()));
    assert!(!nodes[0].is_expired_at(get_epoch_ms()));
    Ok(())
}

#[tokio::test]
async fn test_online_node_descriptor_refresh_replaces_previous_self_record() -> Result<()> {
    let processor = prepare_processor().await;
    let first = processor.publish_online_node_descriptor().await?;
    rings_runtime::sleep(std::time::Duration::from_millis(1)).await?;
    let second = processor.publish_online_node_descriptor().await?;
    let entry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
    processor.storage_fetch(entry_key).await?;
    let entry = processor
        .storage_check_cache(entry_key)
        .await
        .expect("online node registry entry should be cached after publish");
    let stored = Processor::online_node_descriptors_from_entry(&entry);
    let nodes = processor.lookup_online_nodes(false).await?;

    assert_eq!(stored.len(), 1);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].did, processor.did());
    assert!(second.heartbeat_at_ms >= first.heartbeat_at_ms);
    assert_eq!(nodes[0].heartbeat_at_ms, second.heartbeat_at_ms);
    Ok(())
}

#[tokio::test]
async fn test_online_node_concurrent_publish_keeps_one_self_record() -> Result<()> {
    let processor = prepare_processor().await;
    let processor_clone = processor.clone();

    let (first, second) = futures::try_join!(
        processor.publish_online_node_descriptor(),
        processor_clone.publish_online_node_descriptor(),
    )?;
    let entry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
    processor.storage_fetch(entry_key).await?;
    let entry = processor
        .storage_check_cache(entry_key)
        .await
        .expect("online node registry entry should be cached after publish");
    let stored = Processor::online_node_descriptors_from_entry(&entry);
    let nodes = processor.lookup_online_nodes(false).await?;

    assert_eq!(stored.len(), 1);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].did, processor.did());
    assert!(
        nodes[0].heartbeat_at_ms == first.heartbeat_at_ms
            || nodes[0].heartbeat_at_ms == second.heartbeat_at_ms
    );
    Ok(())
}

#[tokio::test]
async fn test_online_node_publish_replaces_observed_self_records() -> Result<()> {
    let processor = prepare_processor().await;
    let other = prepare_processor().await;
    let now_ms = get_epoch_ms();
    let stale_self = processor.online_node_descriptor_at(now_ms.saturating_sub(30_000))?;
    let other_descriptor = other.online_node_descriptor_at(now_ms)?;
    let expired_other = other.online_node_descriptor_at(now_ms.saturating_sub(120_000))?;
    assert!(expired_other.is_expired_at(now_ms));

    processor
        .storage_store(Processor::online_node_registry_entry(vec![
            stale_self,
            other_descriptor.clone(),
            expired_other,
        ])?)
        .await?;

    let published = processor.publish_online_node_descriptor().await?;
    let entry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
    processor.storage_fetch(entry_key).await?;
    let entry = processor
        .storage_check_cache(entry_key)
        .await
        .expect("online node registry entry should be cached after publish");
    let stored = Processor::online_node_descriptors_from_entry(&entry);

    assert_eq!(stored.len(), 2);
    assert!(entry.crdt.register.is_some());
    assert!(entry.crdt.tombstones.is_empty());
    assert_eq!(entry.crdt.dots.len(), stored.len());
    assert_eq!(
        stored
            .iter()
            .filter(|descriptor| descriptor.did == processor.did())
            .count(),
        1
    );
    assert!(stored.iter().any(|descriptor| descriptor == &published));
    assert!(stored
        .iter()
        .any(|descriptor| descriptor == &other_descriptor));
    Ok(())
}

#[tokio::test]
async fn test_onion_exit_publish_replaces_observed_self_records() -> Result<()> {
    let processor = prepare_processor().await;
    let other = prepare_processor().await;
    let now_ms = get_epoch_ms();
    let mut policy = onion_policy(&["example.com:443"], &[])?;
    policy.max_circuits = 8;
    policy.max_streams_per_circuit = 2;
    policy.max_bytes_per_minute = 4096;
    let stale_tcp = onion_exit_descriptor_for_processor_with_service(
        &processor,
        OnionServiceName::tcp(),
        now_ms.saturating_sub(30_000),
        policy.clone(),
    )?;
    let stale_https = onion_exit_descriptor_for_processor_with_service(
        &processor,
        OnionServiceName::https(),
        now_ms.saturating_sub(20_000),
        policy.clone(),
    )?;
    let stale_api = onion_exit_descriptor_for_processor_with_service(
        &processor,
        OnionServiceName::parse("api")?,
        now_ms.saturating_sub(10_000),
        policy.clone(),
    )?;
    let other_https = onion_exit_descriptor_for_processor_with_service(
        &other,
        OnionServiceName::https(),
        now_ms,
        policy.clone(),
    )?;
    let expired_other_https = onion_exit_descriptor_for_processor_with_service(
        &other,
        OnionServiceName::https(),
        now_ms.saturating_sub(120_000),
        policy.clone(),
    )?;
    assert!(expired_other_https.is_expired_at(now_ms));

    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![
            stale_tcp,
            stale_https,
            stale_api,
            other_https.clone(),
            expired_other_https,
        ])?)
        .await?;

    let registration = OnionExitRegistration::new(
        Duration::from_secs(30),
        Duration::from_secs(90),
        default_online_node_type(),
        vec![OnionServiceName::https()],
        policy,
        processor.onion_exit_epoch(),
    );
    let published = registration
        .publish_descriptors(&processor.registration_context())
        .await?;
    let published_descriptor = published
        .into_iter()
        .next()
        .ok_or_else(|| Error::InvalidConfig("expected one onion-exit descriptor".to_string()))?;
    assert_eq!(
        published_descriptor.process_epoch,
        processor.onion_exit_epoch()
    );
    let entry_key = entry::Entry::gen_did(ONION_EXITS_TOPIC)?;
    processor.storage_fetch(entry_key).await?;
    let entry = processor
        .storage_check_cache(entry_key)
        .await
        .expect("onion exit registry entry should be cached after publish");
    let stored = Processor::onion_exit_descriptors_from_entry(&entry);

    assert_eq!(stored.len(), 2);
    assert_eq!(
        stored
            .iter()
            .filter(|descriptor| descriptor.did == processor.did())
            .count(),
        1
    );
    assert!(stored
        .iter()
        .any(|descriptor| descriptor == &published_descriptor));
    assert!(stored.iter().any(|descriptor| descriptor == &other_https));
    Ok(())
}

#[tokio::test]
async fn test_online_node_lookup_filters_expired_descriptors_by_default() -> Result<()> {
    let processor = prepare_processor().await;
    let expired_processor = prepare_processor().await;
    let now_ms = get_epoch_ms();
    let live = processor.online_node_descriptor_at(now_ms)?;
    let expired = OnlineNodeDescriptor::new_signed(
        OnlineNodeDescriptorBody {
            did: expired_processor.did(),
            public_key: expired_processor
                .swarm
                .delegator_verification_pubkey()
                .map_err(Error::CoreError)?,
            delegatee_public_key: expired_processor.delegatee_key.delegatee_public_key(),
            node_type: default_online_node_type(),
            network_id: expired_processor.swarm.network_id(),
            storage_redundancy: expired_processor.swarm.storage_redundancy(),
            dht_virtual_nodes: expired_processor.swarm.dht_virtual_nodes(),
            capabilities: Vec::new(),
            endpoint_hint: None,
            started_at_ms: now_ms.saturating_sub(120_000),
            heartbeat_at_ms: now_ms.saturating_sub(90_000),
            expires_at_ms: now_ms.saturating_sub(30_000),
            version: crate::util::build_version(),
        },
        MessageSigner::new(
            &expired_processor.delegatee_key,
            expired_processor.swarm.network_id(),
        ),
    )
    .map_err(Error::CoreError)?;

    processor
        .storage_store(Processor::online_node_registry_entry(vec![
            live.clone(),
            expired.clone(),
        ])?)
        .await?;

    let live_nodes = processor.lookup_online_nodes(false).await?;
    assert_eq!(live_nodes, vec![live]);

    let all_nodes = processor.lookup_online_nodes(true).await?;
    assert_eq!(all_nodes.len(), 2);
    assert!(all_nodes
        .iter()
        .any(|descriptor| descriptor.did == processor.did()));
    assert!(all_nodes
        .iter()
        .any(|descriptor| descriptor.did == expired_processor.did()));
    assert!(all_nodes.iter().any(|descriptor| descriptor == &expired));
    Ok(())
}

#[tokio::test]
async fn test_online_node_lookup_filters_other_network_descriptors() -> Result<()> {
    let processor = prepare_processor_with_network(0).await;
    let foreign = prepare_processor_with_network(1).await;
    let now_ms = get_epoch_ms();
    let local_descriptor = processor.online_node_descriptor_at(now_ms)?;
    let foreign_descriptor = foreign.online_node_descriptor_at(now_ms)?;

    processor
        .storage_store(Processor::online_node_registry_entry(vec![
            local_descriptor.clone(),
            foreign_descriptor,
        ])?)
        .await?;

    let nodes = processor.lookup_online_nodes(true).await?;
    assert_eq!(nodes, vec![local_descriptor]);
    Ok(())
}

#[tokio::test]
async fn test_online_node_lookup_filters_other_dht_virtual_node_modes() -> Result<()> {
    let processor = prepare_processor_with_network_and_virtual_nodes(0, 2).await;
    let foreign = prepare_processor_with_network_and_virtual_nodes(0, 3).await;
    let now_ms = get_epoch_ms();
    let local_descriptor = processor.online_node_descriptor_at(now_ms)?;
    let foreign_descriptor = foreign.online_node_descriptor_at(now_ms)?;

    processor
        .storage_store(Processor::online_node_registry_entry(vec![
            local_descriptor.clone(),
            foreign_descriptor,
        ])?)
        .await?;

    let nodes = processor.lookup_online_nodes(true).await?;
    assert_eq!(nodes, vec![local_descriptor]);
    Ok(())
}

#[tokio::test]
async fn test_online_node_lookup_filters_other_storage_redundancy_modes() -> Result<()> {
    let processor = prepare_processor_with_network(0).await;
    let foreign = prepare_processor_with_network(0).await;
    let now_ms = get_epoch_ms();
    let local_descriptor = processor.online_node_descriptor_at(now_ms)?;
    let foreign_descriptor = OnlineNodeDescriptor::new_signed(
        OnlineNodeDescriptorBody {
            did: foreign.did(),
            public_key: foreign
                .swarm
                .delegator_verification_pubkey()
                .map_err(Error::CoreError)?,
            delegatee_public_key: foreign.delegatee_key.delegatee_public_key(),
            node_type: default_online_node_type(),
            network_id: foreign.swarm.network_id(),
            storage_redundancy: mismatched_storage_redundancy(foreign.swarm.storage_redundancy()),
            dht_virtual_nodes: foreign.swarm.dht_virtual_nodes(),
            capabilities: Vec::new(),
            endpoint_hint: None,
            started_at_ms: now_ms,
            heartbeat_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(60_000),
            version: crate::util::build_version(),
        },
        MessageSigner::new(&foreign.delegatee_key, foreign.swarm.network_id()),
    )
    .map_err(Error::CoreError)?;

    processor
        .storage_store(Processor::online_node_registry_entry(vec![
            local_descriptor.clone(),
            foreign_descriptor,
        ])?)
        .await?;

    let nodes = processor.lookup_online_nodes(true).await?;
    assert_eq!(nodes, vec![local_descriptor]);
    Ok(())
}

#[tokio::test]
async fn test_online_node_registry_lists_multiple_nodes() -> Result<()> {
    let processor = prepare_processor().await;
    let other = prepare_processor().await;
    let other_descriptor = other.online_node_descriptor_at(get_epoch_ms())?;

    processor
        .storage_append_data(
            ONLINE_NODES_TOPIC,
            other_descriptor.encode().map_err(Error::CoreError)?,
        )
        .await?;
    let published = processor.publish_online_node_descriptor().await?;
    let mut nodes = processor.lookup_online_nodes(false).await?;
    nodes.sort_by_key(|descriptor| descriptor.did);

    assert_eq!(nodes.len(), 2);
    assert!(nodes
        .iter()
        .any(|descriptor| descriptor.did == published.did));
    assert!(nodes.iter().any(|descriptor| descriptor.did == other.did()));
    assert!(nodes
        .iter()
        .all(|descriptor| descriptor.verify_signature(processor.swarm.network_id())));
    Ok(())
}
