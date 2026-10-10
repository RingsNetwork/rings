//! Rings native node command-line entrypoint.

// The `dummy` feature selects rings-core's in-memory test transport, over which a node cannot
// reach any peer; it exists for the node's own test suite only.
#[cfg(all(feature = "dummy", not(test)))]
compile_error!("the `dummy` test transport cannot run a node; build `rings` without it");

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use cli::payload_arg_or_stdin;
use cli::Cli;
use cli::ClientArgs;
use cli::Command;
use cli::ConnectCommand;
use cli::PeerCommand;
use cli::RunCommand;
use cli::SendCommand;
use cli::ServiceCommand;
use futures::pin_mut;
use futures::StreamExt;
use rings_node::extension::Backend;
use rings_node::logging::init_logging;
use rings_node::measure::EvidenceCollectorIdentity;
use rings_node::measure::PeriodicMeasure;
use rings_node::native::api_auth::load_api_token_file;
use rings_node::native::api_auth::load_or_create_api_token;
use rings_node::native::api_auth::ApiSecurity;
use rings_node::native::bootstrap::BootstrapSupervisor;
use rings_node::native::bootstrap::BootstrapTargets;
use rings_node::native::config;
use rings_node::native::endpoint::run_external_api;
use rings_node::native::endpoint::run_internal_api_with_gateway;
use rings_node::native::gateway::NativeGatewayRunner;
use rings_node::onion::native::NativeOnionCircuitHandle;
use rings_node::onion::proxy::http::run_onion_http_proxy;
use rings_node::onion::proxy::http::OnionHttpProxyOptions;
use rings_node::onion::tcp::NativeOnionTcpExitConfig;
use rings_node::onion::OnionEntryGuardStorage;
use rings_node::onion::OnionExitTarget;
use rings_node::prelude::rings_core::storage::file::FileStorage;
use rings_node::prelude::rings_core::storage::RecordAuthority;
use rings_node::prelude::StopSource;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use rings_node::seed::Seed;
use rings_node::util::loader::ResourceLoader;
use tokio::io;
use tokio::io::AsyncBufReadExt;
use tokio::task::JoinError;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;

const FOREGROUND_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
const ONION_ENTRY_GUARD_STORAGE_CAPACITY: u32 = 64 * 1024;
/// Byte budget of the native replay store: every record of a full store
/// (`rings_core::message::TRANSACTION_REPLAY_STORE_MAX_BYTES`, about 27 MiB), with room left
/// for the former shared-stream snapshot until the first load after the #898 upgrade deletes
/// it. The store is authoritative and evicts nothing: a write beyond the budget fails, which
/// fails that transition closed, so the budget must never be reached.
const TRANSACTION_REPLAY_STORAGE_CAPACITY: u32 = 40 * 1024 * 1024;

fn onion_entry_guard_storage_path(data_storage_path: &str) -> String {
    let data_path = Path::new(data_storage_path);
    let parent = data_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent
        .join("onion-entry-guards")
        .to_string_lossy()
        .to_string()
}

fn transaction_replay_storage_path(data_storage_path: &str) -> String {
    let data_path = Path::new(data_storage_path);
    let parent = data_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent
        .join("transaction-replay")
        .to_string_lossy()
        .to_string()
}

fn provisional_evidence_storage_path(measure_storage_path: &str) -> String {
    let measure_path = Path::new(measure_storage_path);
    let parent = measure_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent
        .join("provisional-evidence")
        .to_string_lossy()
        .to_string()
}

/// The command line: the top-level parser, its subcommands and their arguments.
mod cli;

#[allow(clippy::too_many_arguments)]
async fn foreground_run(args: RunCommand) -> anyhow::Result<()> {
    let config_path = args.config_args.config.clone();
    let mut c = config::Config::read_fs(&config_path)?;

    if let Some(ice_servers) = args.ice_servers {
        c.ice_servers = ice_servers;
    }
    if let Some(external_ip) = args.external_ip {
        c.external_ip = Some(external_ip);
    }
    if args.webrtc_udp_port_min.is_some() {
        c.webrtc_udp_port_min = args.webrtc_udp_port_min;
    }
    if args.webrtc_udp_port_max.is_some() {
        c.webrtc_udp_port_max = args.webrtc_udp_port_max;
    }
    if let Some(stabilize_interval) = args.stabilize_interval {
        c.stabilize_interval = stabilize_interval;
    }
    if let Some(source) = args.bootstrap_seed {
        let seed = Seed::load(source.as_str())
            .await
            .with_context(|| format!("loading bootstrap seed {source}"))?;
        c.bootstrap.peers.extend(seed.peers);
    }
    if let Some(external_api_addr) = args.external_api_addr {
        c.external_api_addr = external_api_addr;
    }
    if let Some(internal_api_port) = args.internal_api_port {
        c.internal_api_port = internal_api_port;
    }
    if args.advertise_onion_relay {
        c.advertise_onion_relay = true;
    }
    if args.advertise_onion_exit {
        c.advertise_onion_exit = true;
    }
    if !args.onion_exit_service.is_empty() {
        c.onion_exit_services = args.onion_exit_service;
    }
    if !args.onion_exit_allow_target.is_empty() {
        c.onion_exit_policy.allowed_targets =
            parse_onion_exit_targets(args.onion_exit_allow_target)?;
    }
    if !args.onion_exit_deny_target.is_empty() {
        c.onion_exit_policy.denied_targets = parse_onion_exit_targets(args.onion_exit_deny_target)?;
    }
    if let Some(max_circuits) = args.onion_exit_max_circuits {
        c.onion_exit_policy.max_circuits = max_circuits;
    }
    if let Some(max_streams_per_circuit) = args.onion_exit_max_streams_per_circuit {
        c.onion_exit_policy.max_streams_per_circuit = max_streams_per_circuit;
    }
    if let Some(max_bytes_per_minute) = args.onion_exit_max_bytes_per_minute {
        c.onion_exit_policy.max_bytes_per_minute = max_bytes_per_minute;
    }
    if let Some(interval_secs) = args.onion_exit_heartbeat_interval_secs {
        c.onion_exit_heartbeat_interval_secs = interval_secs;
    }
    if let Some(ttl_secs) = args.onion_exit_ttl_secs {
        c.onion_exit_ttl_secs = ttl_secs;
    }
    if let Some(addr) = args.onion_http_proxy_addr {
        c.onion_http_proxy_addr = Some(addr);
    }
    if let Some(service) = args.onion_http_proxy_service {
        c.onion_http_proxy_service = service;
    }
    if let Some(hop_count) = args.onion_http_proxy_hop_count {
        c.onion_http_proxy_hop_count = hop_count;
    }
    if args.onion_http_proxy_allow_short_paths {
        c.onion_http_proxy_allow_short_paths = true;
    }
    if let Some(timeout_secs) = args.onion_http_proxy_header_timeout_secs {
        c.onion_http_proxy_header_timeout_secs = timeout_secs;
    }
    if let Some(max_connections) = args.onion_http_proxy_max_connections {
        c.onion_http_proxy_max_connections = max_connections;
    }
    if args.gateway {
        let Some(gateway) = c.gateway.as_mut() else {
            anyhow::bail!(
                "--gateway requires a gateway section in {config_path}; `rings init` writes one \
                 into a new config file, or append this to the existing file:\n{}",
                config::NativeGatewayConfig::disabled_default().to_yaml_section()?
            );
        };
        gateway.enabled = true;
    }
    let pc = ProcessorConfig::try_from(c.clone())?;
    let api_security = configure_api_security(
        &config_path,
        &mut c,
        args.api_token_path,
        args.api_allowed_origins,
        args.allow_remote_external_api,
    )?;

    let onion_delegatee_key = pc.delegatee_key();
    let advertise_onion_relay = c.advertise_onion_relay;
    let advertise_onion_exit = c.advertise_onion_exit;
    let onion_exit_services = c.onion_exit_services.clone();
    let onion_exit_policy = c.onion_exit_policy.clone();
    let onion_http_proxy_addr = c.onion_http_proxy_addr.clone();
    let onion_http_proxy_service = c.onion_http_proxy_service.clone();
    let onion_http_proxy_hop_count = c.onion_http_proxy_hop_count;
    let onion_http_proxy_allow_short_paths = c.onion_http_proxy_allow_short_paths;
    let onion_http_proxy_header_timeout_secs = c.onion_http_proxy_header_timeout_secs;
    let onion_http_proxy_max_connections = c.onion_http_proxy_max_connections;
    let gateway_config = c.enabled_gateway().cloned();

    let (data_storage, measure_storage) = if let Some(storage_path) = args.storage_path {
        let storage_path = Path::new(&storage_path);
        let data_path = storage_path.join("data").to_string_lossy().to_string();
        let measure_path = storage_path.join("measure").to_string_lossy().to_string();
        let capacity = args
            .storage_capacity
            .unwrap_or(config::DEFAULT_STORAGE_CAPACITY);
        (
            config::StorageConfig::new(&data_path, capacity),
            config::StorageConfig::new(&measure_path, capacity),
        )
    } else {
        (c.data_storage, c.measure_storage)
    };

    let per_data_storage = Box::new(
        FileStorage::new_with_cap_and_path(data_storage.capacity, data_storage.path.clone())
            .await?,
    );
    let per_measure_storage = Box::new(
        FileStorage::new_with_cap_and_path(measure_storage.capacity, measure_storage.path.clone())
            .await?,
    );
    let provisional_evidence_path = provisional_evidence_storage_path(&measure_storage.path);
    let per_evidence_storage = Box::new(
        FileStorage::new_with_cap_and_path(measure_storage.capacity, provisional_evidence_path)
            .await?,
    );
    let onion_entry_guard_path = onion_entry_guard_storage_path(&data_storage.path);
    let per_onion_entry_guard_storage: OnionEntryGuardStorage = Box::new(
        FileStorage::new_with_cap_and_path(
            ONION_ENTRY_GUARD_STORAGE_CAPACITY,
            onion_entry_guard_path,
        )
        .await?,
    );
    // The replay store is the only copy of its streams' state: its writes are flushed and an
    // undecodable record is reported and kept, so replay fails closed on it (#909).
    let per_transaction_replay_storage = Box::new(
        FileStorage::new_with_cap_path_and_authority(
            TRANSACTION_REPLAY_STORAGE_CAPACITY,
            transaction_replay_storage_path(&data_storage.path),
            RecordAuthority::Authoritative,
        )
        .await?,
    );

    let collector =
        EvidenceCollectorIdentity::new(pc.network_id(), pc.delegatee_key().delegator_did());
    let measure = PeriodicMeasure::new_with_evidence_storage(
        per_measure_storage,
        per_evidence_storage,
        collector,
    )
    .await?;

    let processor = Arc::new(
        ProcessorBuilder::from_config(&pc)?
            .storage(per_data_storage)
            .onion_entry_guard_storage(per_onion_entry_guard_storage)
            .replay_storage(per_transaction_replay_storage)
            .measure(measure)
            .reassembly_limits(args.reassembly_profile.limits())
            .build()?,
    );
    println!("Did: {}", processor.swarm.did());
    let provider = Arc::new(Provider::from_processor(processor.clone()));
    // The relay is an opt-in extension owning its own engine; install it so the daemon can
    // serve TCP/UDP tunnels. The handle is unused server-side — the engine lives on inside the
    // registered interpreters.
    let _relay =
        rings_node::extension::protocols::relay::RelayHandle::install(&provider.extensions())?;
    let onion_exit_config = advertise_onion_exit
        .then(|| NativeOnionTcpExitConfig::new(onion_exit_services, onion_exit_policy.clone()))
        .transpose()?;
    let onion = NativeOnionCircuitHandle::install(
        &provider.extensions(),
        onion_delegatee_key,
        pc.network_id(),
        advertise_onion_relay,
        onion_exit_config,
    )?;
    let gateway_runner = gateway_config
        .map(|config| NativeGatewayRunner::new(processor.clone(), onion.clone(), config))
        .transpose()?;
    let gateway_status = gateway_runner
        .as_ref()
        .map(NativeGatewayRunner::status_handle);
    // Managed bootstrap targets fail fast on invalid configuration; their supervisor is spawned
    // with the other run-owned tasks below and its evidence is the backend's observer.
    let bootstrap_targets = BootstrapTargets::from_config(c.bootstrap, processor.did())?;
    let bootstrap = BootstrapSupervisor::over_processor(bootstrap_targets, processor.clone());
    // The Backend decodes inbound custom messages as namespaced envelopes and routes
    // them to the protocol registry.
    let backend = Backend::new(provider);
    let backend = match bootstrap.as_ref() {
        Some(supervisor) => backend.observed_by(supervisor.observer()),
        None => backend,
    };
    processor.swarm.set_callback(Arc::new(backend))?;

    let stop = StopSource::new();
    let gateway_configured = gateway_runner.is_some();
    let gateway_stop = stop.token();
    let (gateway_started, gateway_startup) = tokio::sync::oneshot::channel();
    let mut gateway_task = tokio::spawn(async move {
        match gateway_runner {
            Some(runner) => {
                runner
                    .run_with_startup_barrier(gateway_stop, gateway_started)
                    .await
            }
            None => std::future::pending::<anyhow::Result<()>>().await,
        }
    });
    await_gateway_startup(gateway_configured, gateway_startup, &mut gateway_task).await?;

    let mut tasks = JoinSet::new();
    let processor_task = processor.clone();
    let processor_stop = stop.token();
    let mut processor_task = tokio::spawn(async move {
        processor_task.listen_with(processor_stop.clone()).await;
        if processor_stop.should_stop() {
            Ok(())
        } else {
            anyhow::bail!("node processor listener stopped unexpectedly")
        }
    });
    let internal_processor = processor.clone();
    let internal_gateway = gateway_status.clone();
    let internal_security = api_security.clone();
    tasks.spawn(async move {
        run_internal_api_with_gateway(
            c.internal_api_port,
            internal_processor,
            internal_gateway,
            internal_security,
        )
        .await
        .context("internal API stopped")
    });
    let external_processor = processor.clone();
    tasks.spawn(async move {
        run_external_api(c.external_api_addr, external_processor, api_security)
            .await
            .context("external API stopped")
    });
    if let Some(supervisor) = bootstrap {
        let bootstrap_stop = stop.token();
        tasks.spawn(async move {
            // Returns only on stop, which the select below requests before aborting the set.
            supervisor.run(bootstrap_stop).await;
            Ok(())
        });
    }
    if let Some(onion_http_proxy_addr) = onion_http_proxy_addr {
        let onion_http_proxy_addr = onion_http_proxy_addr.parse::<SocketAddr>()?;
        let proxy_options = OnionHttpProxyOptions {
            listen_addr: onion_http_proxy_addr,
            service: onion_http_proxy_service,
            hop_count: onion_http_proxy_hop_count,
            allow_short_paths: onion_http_proxy_allow_short_paths,
            max_connections: onion_http_proxy_max_connections,
            header_timeout: Duration::from_secs(onion_http_proxy_header_timeout_secs),
        };
        tasks.spawn(async move {
            run_onion_http_proxy(proxy_options, processor, onion)
                .await
                .context("Onion HTTP proxy stopped")
        });
    }

    enum ForegroundExit {
        Signal(anyhow::Result<()>),
        Service(Option<Result<anyhow::Result<()>, JoinError>>),
        Gateway(Result<anyhow::Result<()>, JoinError>),
        Processor(Result<anyhow::Result<()>, JoinError>),
    }
    let exit = tokio::select! {
        signal = shutdown_signal() => ForegroundExit::Signal(signal),
        service = tasks.join_next() => ForegroundExit::Service(service),
        gateway = &mut gateway_task => ForegroundExit::Gateway(gateway),
        processor = &mut processor_task => ForegroundExit::Processor(processor),
    };
    stop.request_stop();
    // Stop request-serving tasks immediately, but keep the processor task separate so it can
    // flush measurements through its cooperative `listen_with` shutdown path.
    tasks.abort_all();

    let (primary, gateway_finished, processor_finished) = match exit {
        ForegroundExit::Signal(result) => (result, false, false),
        ForegroundExit::Service(result) => (joined_service_result(result), false, false),
        ForegroundExit::Gateway(result) => (joined_task_result(result, "gateway"), true, false),
        ForegroundExit::Processor(result) => {
            (joined_task_result(result, "node processor"), false, true)
        }
    };
    let gateway_cleanup = if gateway_configured && !gateway_finished {
        await_task_cleanup(&mut gateway_task, "gateway route cleanup").await
    } else {
        if !gateway_finished {
            gateway_task.abort();
        }
        Ok(())
    };
    let processor_cleanup = if processor_finished {
        Ok(())
    } else {
        await_task_cleanup(&mut processor_task, "node processor shutdown").await
    };
    combine_foreground_results(primary, gateway_cleanup, processor_cleanup)
}

fn configure_api_security(
    config_path: &str,
    config: &mut config::Config,
    token_path_override: Option<String>,
    allowed_origin_overrides: Vec<String>,
    allow_remote_override: bool,
) -> anyhow::Result<Arc<ApiSecurity>> {
    if let Some(token_path) = token_path_override {
        config.api_token_path = Some(token_path);
    }
    if !allowed_origin_overrides.is_empty() {
        config.api_allowed_origins = allowed_origin_overrides;
    }
    if allow_remote_override {
        config.allow_remote_external_api = true;
    }
    let token = load_or_create_api_token(config_path, config.api_token_path.as_deref())?;
    println!("API authentication token file: {}", token.path().display());
    ApiSecurity::new(
        token.into_secret(),
        &config.api_allowed_origins,
        config.allow_remote_external_api,
    )
    .map(Arc::new)
    .map_err(Into::into)
}

fn joined_service_result(
    result: Option<Result<anyhow::Result<()>, JoinError>>,
) -> anyhow::Result<()> {
    match result {
        Some(Ok(result)) => result,
        Some(Err(error)) => Err(anyhow::anyhow!("foreground service task failed: {error}")),
        None => Err(anyhow::anyhow!("all foreground service tasks stopped")),
    }
}

fn joined_task_result(
    result: Result<anyhow::Result<()>, JoinError>,
    task: &'static str,
) -> anyhow::Result<()> {
    match result {
        Ok(result) => result,
        Err(error) => Err(anyhow::anyhow!("{task} task failed: {error}")),
    }
}

async fn await_gateway_startup(
    configured: bool,
    startup: tokio::sync::oneshot::Receiver<()>,
    gateway_task: &mut JoinHandle<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    if !configured {
        return Ok(());
    }
    match startup.await {
        Ok(()) => Ok(()),
        Err(_) => joined_task_result(gateway_task.await, "gateway startup"),
    }
}

async fn await_task_cleanup(
    task: &mut JoinHandle<anyhow::Result<()>>,
    operation: &'static str,
) -> anyhow::Result<()> {
    match tokio::time::timeout(FOREGROUND_CLEANUP_TIMEOUT, &mut *task).await {
        Ok(result) => joined_task_result(result, operation),
        Err(_) => {
            task.abort();
            Err(anyhow::anyhow!(
                "{operation} did not finish within {FOREGROUND_CLEANUP_TIMEOUT:?}"
            ))
        }
    }
}

fn combine_foreground_results(
    primary: anyhow::Result<()>,
    gateway_cleanup: anyhow::Result<()>,
    processor_cleanup: anyhow::Result<()>,
) -> anyhow::Result<()> {
    let failures = [
        primary.err().map(|error| format!("foreground: {error}")),
        gateway_cleanup
            .err()
            .map(|error| format!("gateway cleanup: {error}")),
        processor_cleanup
            .err()
            .map(|error| format!("processor cleanup: {error}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(failures.join("; ")))
    }
}

async fn shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn parse_onion_exit_targets(targets: Vec<String>) -> anyhow::Result<Vec<OnionExitTarget>> {
    let mut parsed = Vec::with_capacity(targets.len());
    for target in targets {
        parsed.push(OnionExitTarget::parse(target)?);
    }
    Ok(parsed)
}

async fn pubsub_run(client_args: ClientArgs, topic: String) -> anyhow::Result<()> {
    let mut stdin = io::BufReader::new(io::stdin()).lines();

    let client = client_args.new_client().await?;
    let stream = client.subscribe_topic(topic.clone()).await;
    pin_mut!(stream);

    loop {
        tokio::select! {
            line = stdin.next_line() => {
                match line? {
                    Some(line) => {
                        client.publish_message_to_topic(&topic, &line).await?;
                    }
                    None => return Ok(()),
                }
            }
            msg = stream.next() => {
                match msg {
                    Some(msg) => println!("{msg}"),
                    None => return Ok(()),
                }
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    init_logging(cli.log_level);
    let runtime = cli.runtime.build()?;
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Run(args) => foreground_run(*args).await,
        Command::Pubsub(args) => pubsub_run(args.client_args, args.topic).await,
        Command::Connect(ConnectCommand::Node(args)) => {
            let remote_api_token = args
                .remote_api_token_file
                .as_deref()
                .map(load_api_token_file)
                .transpose()?
                .map(|token| token.into_secret());
            args.client_args
                .new_client()
                .await?
                .connect_peer_via_http_with_token(args.node_url.as_str(), remote_api_token)
                .await?
                .display();
            Ok(())
        }
        Command::Connect(ConnectCommand::Did(args)) => {
            args.client_args
                .new_client()
                .await?
                .connect_with_did(args.did.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Connect(ConnectCommand::Seed(args)) => {
            args.client_args
                .new_client()
                .await?
                .connect_with_seed(args.source.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Connect(ConnectCommand::Offer(args)) => {
            args.client_args
                .new_client()
                .await?
                .create_offer(args.did.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Connect(ConnectCommand::Answer(args)) => {
            let offer = payload_arg_or_stdin(args.offer.as_str())?;
            args.client_args
                .new_client()
                .await?
                .answer_offer(offer.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Connect(ConnectCommand::Accept(args)) => {
            let answer = payload_arg_or_stdin(args.answer.as_str())?;
            args.client_args
                .new_client()
                .await?
                .accept_answer(answer.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Peer(PeerCommand::List(args)) => {
            args.client_args
                .new_client()
                .await?
                .list_peers()
                .await?
                .display();
            Ok(())
        }
        Command::Peer(PeerCommand::Disconnect(args)) => {
            args.client_args
                .new_client()
                .await?
                .disconnect(args.address.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Send(SendCommand::Message(args)) => {
            args.client_args
                .new_client()
                .await?
                .send_message(
                    args.to_did.as_str(),
                    args.namespace.as_str(),
                    args.data.as_str(),
                )
                .await?
                .display();
            Ok(())
        }
        Command::Service(ServiceCommand::Register(args)) => {
            args.client_args
                .new_client()
                .await?
                .register_service(args.name.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Service(ServiceCommand::Lookup(args)) => {
            args.client_args
                .new_client()
                .await?
                .lookup_service(args.name.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Init(args) => {
            let delegatee_key_path = args.delegation_args.new_delegation_then_write_to_fs()?;
            let config = config::Config::new(delegatee_key_path);
            let p = config.write_fs(&args.location)?;
            load_or_create_api_token(&p, config.api_token_path.as_deref())?;
            println!("Your config file has saved to: {p}");
            println!("API authentication token file initialized.");
            Ok(())
        }
        Command::NewDelegation(args) => {
            args.delegation_args.new_delegation_then_write_to_fs()?;
            Ok(())
        }
        Command::Inspect(args) => {
            args.client_args
                .new_client()
                .await?
                .inspect()
                .await?
                .display();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use clap::CommandFactory;
    use clap::FromArgMatches;
    use rings_node::logging::LogLevel;
    use rings_node::prelude::rings_core::message::TRANSACTION_REPLAY_STORE_MAX_BYTES;

    use super::await_gateway_startup;
    use super::await_task_cleanup;
    use super::onion_entry_guard_storage_path;
    use super::provisional_evidence_storage_path;
    use super::transaction_replay_storage_path;
    use super::Cli;
    use super::TRANSACTION_REPLAY_STORAGE_CAPACITY;

    fn parse_without_log_level_env<const N: usize>(args: [&str; N]) -> Result<Cli, clap::Error> {
        let matches = Cli::command()
            .mut_arg("log_level", |arg| arg.env(None::<&'static str>))
            .try_get_matches_from(args)?;
        Cli::from_arg_matches(&matches)
    }

    #[test]
    fn test_cli_default_log_level_is_error() {
        let parsed =
            parse_without_log_level_env(["rings", "--runtime", "current-thread", "new-delegation"]);

        assert!(matches!(
            parsed,
            Ok(Cli {
                log_level: LogLevel::Error,
                ..
            })
        ));
    }

    #[test]
    fn test_cli_explicit_log_level_overrides_default() {
        let parsed = parse_without_log_level_env([
            "rings",
            "--log-level",
            "debug",
            "--runtime",
            "current-thread",
            "new-delegation",
        ]);

        assert!(matches!(
            parsed,
            Ok(Cli {
                log_level: LogLevel::Debug,
                ..
            })
        ));
    }

    #[test]
    fn test_entry_guard_storage_is_sibling_of_data_storage() {
        assert_eq!(
            onion_entry_guard_storage_path(".rings/data"),
            ".rings/onion-entry-guards"
        );
        assert_eq!(
            onion_entry_guard_storage_path("/tmp/rings/data"),
            "/tmp/rings/onion-entry-guards"
        );
    }

    /// A full replay store always fits the native replay store's budget, so the stream-count bound,
    /// not the store's budget, is what fails closed.
    #[test]
    fn test_transaction_replay_storage_holds_a_full_store() -> Result<(), std::num::TryFromIntError>
    {
        // The shared-stream snapshot a pre-#898 node left: 4096 streams, each a 91-byte key
        // with its 10-byte last sequence and a 91-byte key with its 1066-byte window, plus
        // length prefixes, and its file record's key framing.
        const SHARED_STREAM_SNAPSHOT_MAX_BYTES: usize = 12 + 4096 * (2 * 91 + 10 + 1066) + 64;
        let capacity = usize::try_from(TRANSACTION_REPLAY_STORAGE_CAPACITY)?;
        assert!(TRANSACTION_REPLAY_STORE_MAX_BYTES + SHARED_STREAM_SNAPSHOT_MAX_BYTES <= capacity);
        Ok(())
    }

    #[test]
    fn test_transaction_replay_storage_is_sibling_of_data_storage() {
        assert_eq!(
            transaction_replay_storage_path(".rings/data"),
            ".rings/transaction-replay"
        );
        assert_eq!(
            transaction_replay_storage_path("/tmp/rings/data"),
            "/tmp/rings/transaction-replay"
        );
    }

    #[test]
    fn test_provisional_evidence_storage_is_sibling_of_measure_storage() {
        assert_eq!(
            provisional_evidence_storage_path(".rings/measure"),
            ".rings/provisional-evidence"
        );
        assert_eq!(
            provisional_evidence_storage_path("/tmp/rings/measure"),
            "/tmp/rings/provisional-evidence"
        );
    }

    #[tokio::test]
    async fn processor_cleanup_gets_a_cooperative_stop_window() {
        let stop = rings_node::prelude::StopSource::new();
        let token = stop.token();
        let flushed = Arc::new(AtomicBool::new(false));
        let task_flushed = Arc::clone(&flushed);
        let mut task = tokio::spawn(async move {
            token.stopped().await;
            task_flushed.store(true, Ordering::Release);
            Ok(())
        });
        stop.request_stop();

        let cleanup = await_task_cleanup(&mut task, "test processor cleanup").await;

        assert!(cleanup.is_ok());
        assert!(flushed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn gateway_startup_barrier_reports_early_failure() {
        let (started, startup) = tokio::sync::oneshot::channel();
        drop(started);
        let mut task = tokio::spawn(async {
            Err(anyhow::anyhow!("gateway activation failed before startup"))
        });

        let result = await_gateway_startup(true, startup, &mut task).await;

        assert!(matches!(
            result,
            Err(error) if error.to_string().contains("activation failed before startup")
        ));
    }
}
