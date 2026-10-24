use bytes::Bytes;
use rings_core::message::MessageSigner;

use super::common::*;
use super::*;
use crate::descriptor::registry_lease;
use crate::descriptor::RegistryElement;
use crate::registration::DhtRegistrationPublisher;
use crate::registration::LeaseTerms;
use crate::tests::exit_descriptor::ExitDescriptorFixture;

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

/// The signed bytes of `descriptors`, which a reader's heard time does not change.
fn elements<D: RegistryElement>(descriptors: &[D]) -> Result<Vec<Bytes>> {
    descriptors
        .iter()
        .map(|descriptor| descriptor.to_element().map_err(Error::CoreError))
        .collect()
}

/// Heartbeat law (#918): a second heartbeat re-appends the lease's bytes, so the registry holds
/// one element, written without a remove, whose dot the heartbeat raised.
#[tokio::test]
async fn test_online_node_heartbeat_reappends_its_lease_without_a_remove() -> Result<()> {
    let processor = prepare_processor().await;
    let first = processor.publish_online_node_descriptor().await?;
    let second = processor.publish_online_node_descriptor().await?;
    let entry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
    processor.storage_fetch(entry_key).await?;
    let entry = processor
        .storage_check_cache(entry_key)
        .await
        .expect("online node registry entry should be cached after publish");
    let stored = OnlineNodeRegistration::descriptors_from_entry(&entry);
    let nodes = processor.lookup_online_nodes(false).await?;

    assert_eq!(
        elements(std::slice::from_ref(&first))?,
        elements(std::slice::from_ref(&second))?
    );
    assert_eq!(elements(&stored)?, elements(std::slice::from_ref(&second))?);
    assert!(entry.crdt.tombstones.is_empty());
    assert_eq!(nodes.len(), 1);
    assert!(nodes[0].heartbeat_at_ms >= second.heartbeat_at_ms);
    Ok(())
}

/// Two concurrent heartbeats serialize on the held lease and append the same bytes.
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
    let stored = OnlineNodeRegistration::descriptors_from_entry(&entry);
    let nodes = processor.lookup_online_nodes(false).await?;

    assert_eq!(
        elements(std::slice::from_ref(&first))?,
        elements(std::slice::from_ref(&second))?
    );
    assert_eq!(stored.len(), 1);
    assert!(entry.crdt.tombstones.is_empty());
    assert_eq!(nodes.len(), 1);
    assert!(nodes[0].heartbeat_at_ms >= first.heartbeat_at_ms.max(second.heartbeat_at_ms));
    Ok(())
}

/// A heartbeat removes nothing it observes: an older lease of its own and the descriptors of
/// other registrants stay, and the reader lists the newest lease per DID.
#[tokio::test]
async fn test_online_node_heartbeat_removes_nothing_and_the_newest_lease_wins() -> Result<()> {
    let processor = prepare_processor().await;
    let other = prepare_processor().await;
    let now_ms = get_epoch_ms();
    let stale_self = processor.online_node_descriptor_at(now_ms.saturating_sub(30_000))?;
    let other_descriptor = other.online_node_descriptor_at(now_ms)?;
    let expired_other = other.online_node_descriptor_at(now_ms.saturating_sub(120_000))?;
    assert!(expired_other.is_expired_at(now_ms));

    processor
        .storage_store(Processor::registry_entry(ONLINE_NODES_TOPIC, vec![
            stale_self,
            other_descriptor.clone(),
            expired_other,
        ])?)
        .await?;
    let entry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
    processor.storage_fetch(entry_key).await?;
    let fixture_register = processor
        .storage_check_cache(entry_key)
        .await
        .expect("online node registry fixture should be cached after store")
        .crdt
        .register;

    let published = processor.publish_online_node_descriptor().await?;
    processor.storage_fetch(entry_key).await?;
    let entry = processor
        .storage_check_cache(entry_key)
        .await
        .expect("online node registry entry should be cached after publish");
    let stored = OnlineNodeRegistration::descriptors_from_entry(&entry);
    let mut nodes = processor.lookup_online_nodes(false).await?;
    nodes.sort_by_key(|descriptor| descriptor.did == processor.did());

    assert_eq!(stored.len(), 4);
    assert_eq!(entry.crdt.register, fixture_register);
    assert!(entry.crdt.tombstones.is_empty());
    assert_eq!(elements(&nodes)?, elements(&[other_descriptor, published])?);
    Ok(())
}

/// The exit registry follows the same law: the published lease outranks this exit's older one
/// for the service it offers, and nothing is removed.
#[tokio::test]
async fn test_onion_exit_newest_lease_outranks_stale_self_records() -> Result<()> {
    let processor = prepare_processor().await;
    let other = prepare_processor().await;
    let now_ms = get_epoch_ms();
    let policy = bounded_policy(&["example.com:443"], &[])?;
    let stale_https = ExitDescriptorFixture::live(
        &processor,
        OnionServiceName::https(),
        policy.clone(),
        now_ms.saturating_sub(20_000),
    )
    .sign()?;
    let other_https =
        ExitDescriptorFixture::live(&other, OnionServiceName::https(), policy.clone(), now_ms)
            .sign()?;

    processor
        .storage_store(Processor::registry_entry(ONION_EXITS_TOPIC, vec![
            stale_https,
            other_https.clone(),
        ])?)
        .await?;

    let registration = OnionExitRegistration::new(
        Duration::from_secs(30),
        Duration::from_secs(90),
        default_online_node_type(),
        OnionExitOffer::new(vec![OnionServiceName::https()], policy)?,
        processor.onion_process_epoch(),
    );
    let published = registration
        .publish_descriptors(&processor.registration_context())
        .await?;
    assert!(published
        .iter()
        .all(|descriptor| descriptor.process_epoch == processor.onion_process_epoch.get()));
    let entry_key = entry::Entry::gen_did(ONION_EXITS_TOPIC)?;
    processor.storage_fetch(entry_key).await?;
    let entry = processor
        .storage_check_cache(entry_key)
        .await
        .expect("onion exit registry entry should be cached after publish");
    let mut exits = processor.lookup_onion_exits("https", false).await?;
    exits.sort_by_key(|descriptor| descriptor.did == processor.did());

    assert_eq!(
        OnionExitRegistration::descriptors_from_entry(&entry).len(),
        3
    );
    assert!(entry.crdt.tombstones.is_empty());
    let mut expected = vec![other_https];
    expected.extend(published);
    assert_eq!(elements(&exits)?, elements(&expected)?);
    Ok(())
}

/// A lease is renewed only once fewer than two heartbeats remain in it, or when a signed field
/// changes; until then every heartbeat appends the same bytes.
#[tokio::test]
async fn test_registry_lease_renews_near_its_end_or_on_a_change() -> Result<()> {
    let processor = prepare_processor().await;
    let context = processor.registration_context();
    let interval = Duration::from_secs(30);
    let publisher = DhtRegistrationPublisher::<[OnlineNodeDescriptor; 1]>::new(
        ONLINE_NODES_TOPIC,
        interval,
        Duration::from_secs(90),
    );
    let registration = &processor.online_node_registration;
    let version = std::cell::Cell::new("one");
    let sign = |terms: LeaseTerms| {
        registration
            .descriptor_at(&context, terms)
            .map(|mut descriptor| {
                descriptor.version = version.get().to_string();
                [descriptor]
            })
    };
    let t0 = get_epoch_ms();
    let lease = registry_lease().as_millis();
    let margin = 2 * interval.as_millis();

    let [first] = publisher.publish_lease(&context, t0, sign).await?;
    assert_eq!(first.expires_at_ms, t0 + lease);
    let [kept] = publisher
        .publish_lease(&context, t0 + lease - margin - 1, sign)
        .await?;
    assert_eq!(elements(&[kept])?, elements(std::slice::from_ref(&first))?);
    let [renewed] = publisher
        .publish_lease(&context, t0 + lease - margin, sign)
        .await?;
    assert_eq!(renewed.expires_at_ms, t0 + 2 * lease - margin);

    version.set("two");
    let [changed] = publisher.publish_lease(&context, t0 + lease, sign).await?;
    assert_eq!(changed.version, "two");
    assert_eq!(changed.expires_at_ms, t0 + 2 * lease);
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
            capabilities: OnlineNodeCapabilities::default(),
            endpoint_hint: None,
            started_at_ms: now_ms.saturating_sub(120_000),
            fresh_for_ms: 60_000,
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
        .storage_store(Processor::registry_entry(ONLINE_NODES_TOPIC, vec![
            live.clone(),
            expired.clone(),
        ])?)
        .await?;

    let live_nodes = processor.lookup_online_nodes(false).await?;
    assert_eq!(elements(&live_nodes)?, elements(&[live])?);

    let all_nodes = processor.lookup_online_nodes(true).await?;
    assert_eq!(all_nodes.len(), 2);
    assert!(all_nodes
        .iter()
        .any(|descriptor| descriptor.did == processor.did()));
    assert!(all_nodes
        .iter()
        .any(|descriptor| descriptor.did == expired_processor.did()));
    assert!(elements(&all_nodes)?.contains(&expired.to_element().map_err(Error::CoreError)?));
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
        .storage_store(Processor::registry_entry(ONLINE_NODES_TOPIC, vec![
            local_descriptor.clone(),
            foreign_descriptor,
        ])?)
        .await?;

    let nodes = processor.lookup_online_nodes(true).await?;
    assert_eq!(elements(&nodes)?, elements(&[local_descriptor])?);
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
        .storage_store(Processor::registry_entry(ONLINE_NODES_TOPIC, vec![
            local_descriptor.clone(),
            foreign_descriptor,
        ])?)
        .await?;

    let nodes = processor.lookup_online_nodes(true).await?;
    assert_eq!(elements(&nodes)?, elements(&[local_descriptor])?);
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
            capabilities: OnlineNodeCapabilities::default(),
            endpoint_hint: None,
            started_at_ms: now_ms,
            fresh_for_ms: 60_000,
            expires_at_ms: now_ms.saturating_add(60_000),
            version: crate::util::build_version(),
        },
        MessageSigner::new(&foreign.delegatee_key, foreign.swarm.network_id()),
    )
    .map_err(Error::CoreError)?;

    processor
        .storage_store(Processor::registry_entry(ONLINE_NODES_TOPIC, vec![
            local_descriptor.clone(),
            foreign_descriptor,
        ])?)
        .await?;

    let nodes = processor.lookup_online_nodes(true).await?;
    assert_eq!(elements(&nodes)?, elements(&[local_descriptor])?);
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
            other_descriptor.to_element().map_err(Error::CoreError)?,
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
