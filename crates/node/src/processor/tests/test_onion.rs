use std::collections::BTreeSet;

use super::common::*;
use super::*;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionServiceName;

#[tokio::test]
async fn test_onion_exit_lookup_uses_dedicated_exit_registry() -> Result<()> {
    let processor = prepare_processor().await;
    let relay_only = prepare_processor().await;
    let exit = prepare_processor().await;
    let relay_descriptor = relay_only.online_node_descriptor_at(get_epoch_ms())?;
    let exit_descriptor = onion_exit_descriptor_for_processor(&exit, "tcp", get_epoch_ms())?;

    processor
        .storage_store(Processor::online_node_registry_entry(vec![
            relay_descriptor,
        ])?)
        .await?;
    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![
            exit_descriptor.clone()
        ])?)
        .await?;

    let exits = processor.lookup_onion_exits("tcp", false).await?;

    assert_eq!(exits, vec![exit_descriptor]);
    assert!(exits
        .iter()
        .all(|descriptor| descriptor.verify_signature(processor.swarm.network_id())));
    assert!(!exits
        .iter()
        .any(|descriptor| descriptor.did == relay_only.did()));
    Ok(())
}

#[tokio::test]
async fn test_onion_exit_lookup_preserves_distinct_services_for_same_did() -> Result<()> {
    let processor = prepare_processor().await;
    let exit = prepare_processor().await;
    let now_ms = get_epoch_ms();
    let older_tcp = onion_exit_descriptor_for_processor(&exit, "tcp", now_ms)?;
    let newer_https = onion_exit_descriptor_for_processor(&exit, "https", now_ms + 1)?;

    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![
            older_tcp,
            newer_https,
        ])?)
        .await?;

    assert_eq!(processor.lookup_onion_exits("tcp", false).await?.len(), 1);
    assert_eq!(processor.lookup_onion_exits("https", false).await?.len(), 1);
    assert_eq!(processor.lookup_onion_exits("", false).await?.len(), 2);
    Ok(())
}

/// Proxy routing closes a loop over presence relays: relay positions need no exit descriptor,
/// and the exit, registered as a relay at the same epoch, takes the symbol position.
#[tokio::test]
async fn test_onion_proxy_route_uses_presence_relays_without_exit_descriptor() -> Result<()> {
    let processor = prepare_processor().await;
    let exit = prepare_processor().await;
    let exit_descriptor = onion_exit_descriptor_for_processor(&exit, "tcp", get_epoch_ms())?;
    let relays = store_onion_relays(&processor, &[&exit], 3).await?;
    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![
            exit_descriptor.clone()
        ])?)
        .await?;

    let route = processor
        .build_onion_proxy_route(
            OnionProxyConfig::tcp_connect_service(OnionServiceName::tcp())?,
            OnionProxyTarget::parse_authority("example.com:443")?,
        )
        .await?
        .route;
    let dids = route
        .hops()
        .positions()
        .map(|hop| hop.did)
        .collect::<Vec<_>>();
    let relay_dids = relays.iter().map(Processor::did).collect::<BTreeSet<_>>();

    assert_eq!(dids.len(), 5);
    assert_eq!(dids.first(), dids.last());
    assert_eq!(dids.get(2), Some(&exit.did()));
    assert!(dids
        .iter()
        .enumerate()
        .all(|(index, did)| index == 2 || relay_dids.contains(did)));
    assert_eq!(route.exit(), &exit_descriptor);
    Ok(())
}

/// A symbol descriptor from an earlier process of its node is stale and never selected.
#[tokio::test]
async fn test_onion_proxy_route_rejects_exit_with_stale_process_epoch() -> Result<()> {
    let processor = prepare_processor().await;
    let exit = prepare_processor().await;
    let mut restarted = exit.clone();
    restarted.onion_process_epoch = OnionProcessEpoch::new([0x5a; 16]);
    let stale_descriptor = onion_exit_descriptor_for_processor(&exit, "tcp", get_epoch_ms())?;
    store_onion_relays(&processor, &[&restarted], 3).await?;
    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![
            stale_descriptor,
        ])?)
        .await?;

    let error = processor
        .build_onion_proxy_route(
            OnionProxyConfig::tcp_connect(),
            OnionProxyTarget::parse_authority("example.com:443")?,
        )
        .await
        .err()
        .ok_or_else(|| Error::InvalidConfig("expected route failure".to_string()))?;

    assert!(matches!(
        error,
        Error::OnionRouteError(OnionRouteError::NoLiveExit { service }) if service == "tcp"
    ));
    Ok(())
}

#[tokio::test]
async fn test_onion_proxy_route_uses_protocol_service_class() -> Result<()> {
    let processor = prepare_processor().await;
    let tcp_exit = prepare_processor().await;
    let https_exit = prepare_processor().await;
    let now_ms = get_epoch_ms();
    let tcp_exit_descriptor = onion_exit_descriptor_for_processor(&tcp_exit, "tcp", now_ms)?;
    let https_exit_descriptor = onion_exit_descriptor_for_processor(&https_exit, "https", now_ms)?;

    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![
            tcp_exit_descriptor,
            https_exit_descriptor,
        ])?)
        .await?;
    store_onion_relays(&processor, &[&tcp_exit, &https_exit], 2).await?;

    let target = OnionProxyTarget::parse_authority("example.com:443")?;
    let tcp_route = processor
        .build_onion_proxy_route(OnionProxyConfig::tcp_connect(), target.clone())
        .await?;
    let https_route = processor
        .build_onion_proxy_route(OnionProxyConfig::https_proxy(), target)
        .await?;

    assert_eq!(tcp_route.exit_service(), "tcp");
    assert_eq!(tcp_route.exit_did(), tcp_exit.did());
    assert_eq!(https_route.exit_service(), "https");
    assert_eq!(https_route.exit_did(), https_exit.did());
    Ok(())
}

/// HTTPS route construction matches the singular canonical service descriptor.
#[tokio::test]
async fn test_onion_route_accepts_https_service() -> Result<()> {
    let processor = prepare_processor().await;
    let exit = prepare_processor().await;
    let descriptor = onion_exit_descriptor_for_processor_with_service(
        &exit,
        OnionServiceName::https(),
        get_epoch_ms(),
        {
            let mut policy = onion_policy(&["example.com:443"], &[])?;
            policy.max_circuits = 8;
            policy.max_streams_per_circuit = 2;
            policy.max_bytes_per_minute = 4096;
            policy
        },
    )?;

    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![descriptor])?)
        .await?;
    store_onion_relays(&processor, &[&exit], 3).await?;

    let route = processor
        .build_onion_proxy_route(
            OnionProxyConfig::https_proxy(),
            OnionProxyTarget::parse_authority("example.com:443")?,
        )
        .await?
        .route;

    assert_eq!(route.exit_did(), exit.did());
    assert_eq!(route.service(), "https");
    Ok(())
}

/// The proxy route accepts an HTTPS service descriptor without a transport field.
#[tokio::test]
async fn test_onion_proxy_route_accepts_https_service() -> Result<()> {
    let processor = prepare_processor().await;
    let exit = prepare_processor().await;
    let descriptor = onion_exit_descriptor_for_processor_with_service(
        &exit,
        OnionServiceName::https(),
        get_epoch_ms(),
        {
            let mut policy = onion_policy(&["example.com:443"], &[])?;
            policy.max_circuits = 8;
            policy.max_streams_per_circuit = 2;
            policy.max_bytes_per_minute = 4096;
            policy
        },
    )?;

    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![descriptor])?)
        .await?;
    store_onion_relays(&processor, &[&exit], 3).await?;

    let target = OnionProxyTarget::parse_authority("example.com:443")?;
    let route = processor
        .build_onion_proxy_route(OnionProxyConfig::https_proxy(), target)
        .await?;

    assert_eq!(route.exit_did(), exit.did());
    assert_eq!(route.exit_service(), "https");
    Ok(())
}

#[tokio::test]
async fn test_tcp_connect_route_rejects_browser_https_exit_descriptor() -> Result<()> {
    let processor = prepare_processor().await;
    let browser_exit = prepare_processor().await;
    let descriptor = onion_exit_descriptor_for_processor_with_node_type_service(
        &browser_exit,
        OnlineNodeType::Browser,
        OnionServiceName::https(),
        get_epoch_ms(),
        {
            let mut policy = onion_policy(&["example.com:443"], &[])?;
            policy.max_circuits = 8;
            policy.max_streams_per_circuit = 2;
            policy.max_bytes_per_minute = 4096;
            policy
        },
    )?;

    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![descriptor])?)
        .await?;

    let target = OnionProxyTarget::parse_authority("example.com:443")?;
    let error = processor
        .build_onion_proxy_route(
            OnionProxyConfig::tcp_connect_service(OnionServiceName::https())?,
            target,
        )
        .await
        .err()
        .ok_or_else(|| Error::InvalidConfig("expected route failure".to_string()))?;

    assert!(matches!(
        error,
        Error::OnionRouteError(OnionRouteError::NoExitForProxyProtocol { service, protocol })
            if service == "https" && protocol == "tcp-connect"
    ));
    Ok(())
}

#[tokio::test]
async fn test_onion_proxy_route_filters_exits_by_target_policy() -> Result<()> {
    let processor = prepare_processor().await;
    let allowed_exit = prepare_processor().await;
    let denied_exit = prepare_processor().await;
    let now_ms = get_epoch_ms();
    let allowed_descriptor =
        onion_exit_descriptor_for_processor_with_policy(&allowed_exit, "https", now_ms, {
            let mut policy = onion_policy(&["example.com:443"], &[])?;
            policy.max_circuits = 8;
            policy.max_streams_per_circuit = 2;
            policy.max_bytes_per_minute = 4096;
            policy
        })?;
    let denied_descriptor =
        onion_exit_descriptor_for_processor_with_policy(&denied_exit, "https", now_ms, {
            let mut policy = onion_policy(&["example.com:443"], &["example.com:443"])?;
            policy.max_circuits = 8;
            policy.max_streams_per_circuit = 2;
            policy.max_bytes_per_minute = 4096;
            policy
        })?;

    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![
            denied_descriptor,
            allowed_descriptor,
        ])?)
        .await?;
    store_onion_relays(&processor, &[&allowed_exit, &denied_exit], 2).await?;

    let target = OnionProxyTarget::parse_authority("example.com:443")?;
    let route = processor
        .build_onion_proxy_route(OnionProxyConfig::https_proxy(), target)
        .await?;

    assert_eq!(route.exit_did(), allowed_exit.did());
    Ok(())
}

#[tokio::test]
async fn test_onion_proxy_route_reports_policy_denied_target() -> Result<()> {
    let processor = prepare_processor().await;
    let denied_exit = prepare_processor().await;
    let now_ms = get_epoch_ms();
    let denied_descriptor =
        onion_exit_descriptor_for_processor_with_policy(&denied_exit, "https", now_ms, {
            let mut policy = onion_policy(&["other.example.com:443"], &[])?;
            policy.max_circuits = 8;
            policy.max_streams_per_circuit = 2;
            policy.max_bytes_per_minute = 4096;
            policy
        })?;

    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![
            denied_descriptor,
        ])?)
        .await?;

    let target = OnionProxyTarget::parse_authority("example.com:443")?;
    let error = processor
        .build_onion_proxy_route(OnionProxyConfig::https_proxy(), target)
        .await
        .err()
        .ok_or_else(|| Error::InvalidConfig("expected route failure".to_string()))?;

    assert!(matches!(
        error,
        Error::OnionRouteError(OnionRouteError::NoExitAllowsTarget { service, target })
            if service == "https" && target == "example.com:443"
    ));
    Ok(())
}
