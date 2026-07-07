use super::common::*;
use super::*;

#[tokio::test]
async fn onion_exit_lookup_uses_dedicated_exit_registry() -> Result<()> {
    let processor = prepare_processor().await;
    let relay_only = prepare_processor().await;
    let exit = prepare_processor().await;
    let relay_descriptor = relay_only.online_node_descriptor_at(get_epoch_ms())?;
    let exit_descriptor = onion_exit_descriptor_for_processor(&exit, "web", get_epoch_ms())?;

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

    let exits = processor.lookup_onion_exits("web", false).await?;

    assert_eq!(exits, vec![exit_descriptor]);
    assert!(exits.iter().all(OnionExitDescriptor::verify_signature));
    assert!(!exits
        .iter()
        .any(|descriptor| descriptor.did == relay_only.did()));
    Ok(())
}

#[tokio::test]
async fn onion_exit_lookup_preserves_distinct_services_for_same_did() -> Result<()> {
    let processor = prepare_processor().await;
    let exit = prepare_processor().await;
    let now_ms = get_epoch_ms();
    let older_web = onion_exit_descriptor_for_processor(&exit, "web", now_ms)?;
    let newer_ssh = onion_exit_descriptor_for_processor(&exit, "ssh", now_ms + 1)?;

    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![
            older_web, newer_ssh,
        ])?)
        .await?;

    assert_eq!(processor.lookup_onion_exits("web", false).await?.len(), 1);
    assert_eq!(processor.lookup_onion_exits("ssh", false).await?.len(), 1);
    assert_eq!(processor.lookup_onion_exits("", false).await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn onion_route_builder_uses_presence_relays_without_exit_descriptor() -> Result<()> {
    let processor = prepare_processor().await;
    let first_relay = prepare_processor().await;
    let second_relay = prepare_processor().await;
    let exit = prepare_processor().await;
    let now_ms = get_epoch_ms();
    let exit_descriptor = onion_exit_descriptor_for_processor(&exit, "web", now_ms)?;

    processor
        .storage_store(Processor::online_node_registry_entry(vec![
            online_relay_descriptor_for_processor(&first_relay, now_ms)?,
            online_relay_descriptor_for_processor(&second_relay, now_ms)?,
            exit.online_node_descriptor_at(now_ms)?,
        ])?)
        .await?;
    processor
        .storage_store(Processor::onion_exit_registry_entry(vec![
            exit_descriptor.clone()
        ])?)
        .await?;

    let route = processor
        .build_onion_route("web".to_string(), 3, false)
        .await?;

    assert_eq!(route.hops().len(), 3);
    assert_eq!(route.exit_did(), exit.did());
    assert_eq!(route.hops().last().copied(), Some(exit.did()));
    assert!(route.hops().contains(&first_relay.did()));
    assert!(route.hops().contains(&second_relay.did()));
    assert_eq!(route.exit(), &exit_descriptor);
    Ok(())
}

#[tokio::test]
async fn onion_proxy_route_uses_protocol_service_class() -> Result<()> {
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

    let target = OnionProxyTarget::parse_authority("example.com:443")?;
    let tcp_route = processor
        .build_onion_proxy_route(OnionProxyConfig::tcp_connect(1, false), target.clone())
        .await?;
    let https_route = processor
        .build_onion_proxy_route(OnionProxyConfig::https_proxy(1, false), target)
        .await?;

    assert_eq!(tcp_route.exit_service(), "tcp");
    assert_eq!(tcp_route.exit_did(), tcp_exit.did());
    assert_eq!(https_route.exit_service(), "https");
    assert_eq!(https_route.exit_did(), https_exit.did());
    Ok(())
}

#[tokio::test]
async fn onion_route_rejects_reserved_service_with_wrong_transport() -> Result<()> {
    let processor = prepare_processor().await;
    let exit = prepare_processor().await;
    let descriptor = onion_exit_descriptor_for_processor_with_service(
        &exit,
        OnionExitService::new("https", OnionExitTransport::Tcp)?,
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

    let error = processor
        .build_onion_route("https".to_string(), 1, false)
        .await
        .err()
        .ok_or_else(|| Error::InvalidConfig("expected route failure".to_string()))?;

    assert!(matches!(
        error,
        Error::OnionRouteError(OnionRouteError::NoLiveExit { service })
            if service == "https"
    ));
    Ok(())
}

#[tokio::test]
async fn onion_proxy_route_rejects_reserved_service_with_wrong_transport() -> Result<()> {
    let processor = prepare_processor().await;
    let exit = prepare_processor().await;
    let descriptor = onion_exit_descriptor_for_processor_with_service(
        &exit,
        OnionExitService::new("https", OnionExitTransport::Tcp)?,
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
        .build_onion_proxy_route(OnionProxyConfig::https_proxy(1, false), target)
        .await
        .err()
        .ok_or_else(|| Error::InvalidConfig("expected route failure".to_string()))?;

    assert!(matches!(
        error,
        Error::OnionRouteError(OnionRouteError::NoExitWithTransport { service, transport })
            if service == "https" && transport == OnionExitTransport::Https
    ));
    Ok(())
}

#[tokio::test]
async fn onion_proxy_route_filters_exits_by_target_policy() -> Result<()> {
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

    let target = OnionProxyTarget::parse_authority("example.com:443")?;
    let route = processor
        .build_onion_proxy_route(OnionProxyConfig::https_proxy(1, false), target)
        .await?;

    assert_eq!(route.exit_did(), allowed_exit.did());
    Ok(())
}

#[tokio::test]
async fn onion_proxy_route_reports_policy_denied_target() -> Result<()> {
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
        .build_onion_proxy_route(OnionProxyConfig::https_proxy(1, false), target)
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
