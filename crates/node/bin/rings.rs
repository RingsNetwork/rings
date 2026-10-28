//! Rings native node command-line entrypoint.

// The `dummy` feature selects rings-core's in-memory test transport, over which a node cannot
// reach any peer; it exists for the node's own test suite only.
#[cfg(all(feature = "dummy", not(test)))]
compile_error!("the `dummy` test transport cannot run a node; build `rings` without it");

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
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
use rings_gateway::GatewayStatusHandle;
use rings_node::dns_relay;
use rings_node::dns_relay::ServiceOffer;
use rings_node::dns_relay::ServicesConfig;
use rings_node::extension::protocols::relay::RelayHandle;
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
use rings_node::native::horizon::signing_horizon;
use rings_node::native::stores::NodeStores;
use rings_node::onion::proxy::http::run_onion_http_proxy;
use rings_node::onion::proxy::http::OnionHttpProxyOptions;
use rings_node::onion::OnionExitTarget;
use rings_node::prelude::rings_core::chunk::ReassemblyLimits;
use rings_node::prelude::rings_core::utils::get_epoch_ms;
use rings_node::prelude::StopSource;
use rings_node::prelude::StopToken;
use rings_node::processor::Processor;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use rings_node::seed::Seed;
use rings_node::util::loader::ResourceLoader;
use tokio::io;
use tokio::io::AsyncBufReadExt;
use tokio::task::JoinError;
use tokio::task::JoinSet;

const FOREGROUND_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
/// The command line: the top-level parser, its subcommands and their arguments.
mod cli;

/// Run `service`, the application inbox's server stopping on `stop`, to completion; the swarm
/// going away first, which no running node reaches, is an error.
async fn serve_application(
    service: impl Future<Output = ()>,
    stop: StopToken,
) -> anyhow::Result<()> {
    service.await;
    if stop.should_stop() {
        Ok(())
    } else {
        anyhow::bail!("application inbox closed unexpectedly")
    }
}

/// Run the node in the foreground until a signal or a service stops it.
async fn foreground_run(mut args: RunCommand) -> anyhow::Result<()> {
    let config_path = args.config_args.config.clone();
    let mut c = config::Config::read_fs(&config_path)?;
    apply_run_overrides(&mut c, &mut args, &config_path).await?;
    let pc = ProcessorConfig::try_from(c.clone())?;
    let api_security = configure_api_security(
        &config_path,
        &mut c,
        args.api_token_path,
        args.api_allowed_origins,
        args.allow_remote_external_api,
    )?;
    let onion_http_proxy = onion_http_proxy_options(&c)?;
    let (data_storage, measure_storage) =
        run_storage_configs(args.storage_path, args.storage_capacity, &c);
    let service_offer = c.services.as_ref().map(ServicesConfig::offer).transpose()?;
    let processor = build_processor(
        &pc,
        data_storage,
        measure_storage,
        args.reassembly_profile.limits(),
        service_offer,
    )
    .await?;
    println!("Did: {}", processor.swarm.did());
    let provider = Arc::new(Provider::from_processor(processor.clone()));
    // The relay is an opt-in extension owning its own engine: it serves TCP/UDP tunnels, the
    // ports this node serves through DNS relays, and the ingress of a DNS relay it runs.
    let relay = RelayHandle::install(&provider.extensions())?;
    let (services, relaying) = (c.services.as_ref(), c.dns_relay.as_ref());
    let dns_relay = dns_relay::open(services, relaying, &processor, &relay).await?;
    let onion = provider.onion_circuit()?;
    let gateway_runner = c
        .enabled_gateway()
        .cloned()
        .map(|config| NativeGatewayRunner::new(processor.clone(), onion.clone(), config))
        .transpose()?;
    let gateway_status = gateway_runner
        .as_ref()
        .map(NativeGatewayRunner::status_handle);
    // Managed bootstrap targets fail fast on invalid configuration; their supervisor is spawned
    // with the other run-owned tasks below and its evidence is the backend's observer.
    let bootstrap_targets = BootstrapTargets::from_config(c.bootstrap, processor.did())?;
    let bootstrap = BootstrapSupervisor::over_processor(bootstrap_targets, processor.clone());
    let stop = StopSource::new();
    // The Backend decodes inbound custom messages as namespaced envelopes and routes them to
    // the protocol registry. Its inbox is claimed here, before the processor listens, so no
    // application data is discarded unclaimed.
    let application = provider.application_service(
        bootstrap.as_ref().map(BootstrapSupervisor::observer),
        stop.token(),
    )?;
    let gateway_configured = gateway_runner.is_some();
    let gateway_task = start_gateway(gateway_runner, stop.token()).await?;

    let mut tasks = JoinSet::new();
    tasks.spawn(serve_application(application, stop.token()));
    // The node can send nothing from its delegation's signing horizon on: end there, so the
    // supervisor restarts it with a fresh delegatee key instead of leaving it mute (#1063).
    let horizon = processor.signing_horizon();
    tasks.spawn(async move { Err(signing_horizon(horizon, get_epoch_ms).await.into()) });
    let processor_task = spawn_processor(processor.clone(), stop.token());
    spawn_apis(
        &mut tasks,
        c.internal_api_port,
        c.external_api_addr,
        processor.clone(),
        gateway_status,
        api_security,
    );
    if let Some(supervisor) = bootstrap {
        let bootstrap_stop = stop.token();
        tasks.spawn(async move {
            // Returns only on stop, which the select below requests before aborting the set.
            supervisor.run(bootstrap_stop).await;
            Ok(())
        });
    }
    if let Some(dns_relay) = dns_relay {
        let dns_relay_stop = stop.token();
        tasks.spawn(async move {
            dns_relay
                .run(dns_relay_stop.stopped())
                .await
                .context("DNS relay stopped")
        });
    }
    if let Some(proxy_options) = onion_http_proxy {
        tasks.spawn(async move {
            run_onion_http_proxy(proxy_options, processor, onion)
                .await
                .context("Onion HTTP proxy stopped")
        });
    }
    await_foreground_exit(
        stop,
        tasks,
        gateway_task,
        processor_task,
        gateway_configured,
    )
    .await
}

/// Apply the run's command-line overrides to the configuration read from `config_path`.
async fn apply_run_overrides(
    c: &mut config::Config,
    args: &mut RunCommand,
    config_path: &str,
) -> anyhow::Result<()> {
    if let Some(ice_servers) = args.ice_servers.take() {
        c.ice_servers = ice_servers;
    }
    if let Some(external_ip) = args.external_ip.take() {
        c.external_ip = Some(external_ip);
    }
    if args.webrtc_udp_port_min.is_some() {
        c.webrtc_udp_port_min = args.webrtc_udp_port_min;
    }
    if args.webrtc_udp_port_max.is_some() {
        c.webrtc_udp_port_max = args.webrtc_udp_port_max;
    }
    if let Some(stabilize_interval) = args.stabilize_interval.take() {
        c.stabilize_interval = stabilize_interval;
    }
    if let Some(source) = args.bootstrap_seed.take() {
        let seed = Seed::load(source.as_str())
            .await
            .with_context(|| format!("loading bootstrap seed {source}"))?;
        c.bootstrap.peers.extend(seed.peers);
    }
    if let Some(external_api_addr) = args.external_api_addr.take() {
        c.external_api_addr = external_api_addr;
    }
    if let Some(internal_api_port) = args.internal_api_port.take() {
        c.internal_api_port = internal_api_port;
    }
    if args.advertise_onion_relay {
        c.advertise_onion_relay = true;
    }
    if args.advertise_onion_exit {
        c.advertise_onion_exit = true;
    }
    if !args.onion_exit_service.is_empty() {
        c.onion_exit_services = std::mem::take(&mut args.onion_exit_service);
    }
    if !args.onion_exit_allow_target.is_empty() {
        c.onion_exit_policy.allowed_targets =
            parse_onion_exit_targets(std::mem::take(&mut args.onion_exit_allow_target))?;
    }
    if !args.onion_exit_deny_target.is_empty() {
        c.onion_exit_policy.denied_targets =
            parse_onion_exit_targets(std::mem::take(&mut args.onion_exit_deny_target))?;
    }
    if let Some(max_sessions) = args.onion_exit_max_sessions.take() {
        c.onion_exit_policy.max_sessions = max_sessions;
    }
    if let Some(max_bytes_per_minute) = args.onion_exit_max_bytes_per_minute.take() {
        c.onion_exit_policy.max_bytes_per_minute = max_bytes_per_minute;
    }
    if let Some(interval_secs) = args.onion_exit_heartbeat_interval_secs.take() {
        c.onion_exit_heartbeat_interval_secs = interval_secs;
    }
    if let Some(ttl_secs) = args.onion_exit_ttl_secs.take() {
        c.onion_exit_ttl_secs = ttl_secs;
    }
    if let Some(addr) = args.onion_http_proxy_addr.take() {
        c.onion_http_proxy_addr = Some(addr);
    }
    if let Some(timeout_secs) = args.onion_http_proxy_header_timeout_secs.take() {
        c.onion_http_proxy_header_timeout_secs = timeout_secs;
    }
    if let Some(max_connections) = args.onion_http_proxy_max_connections.take() {
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
    Ok(())
}

/// The data and measure storage of the run: under `storage_path` when given, else as configured.
fn run_storage_configs(
    storage_path: Option<String>,
    storage_capacity: Option<u32>,
    c: &config::Config,
) -> (config::StorageConfig, config::StorageConfig) {
    let Some(storage_path) = storage_path else {
        return (c.data_storage.clone(), c.measure_storage.clone());
    };
    let storage_path = Path::new(&storage_path);
    let data_path = storage_path.join("data").to_string_lossy().to_string();
    let measure_path = storage_path.join("measure").to_string_lossy().to_string();
    let capacity = storage_capacity.unwrap_or(config::DEFAULT_STORAGE_CAPACITY);
    (
        config::StorageConfig::new(&data_path, capacity),
        config::StorageConfig::new(&measure_path, capacity),
    )
}

/// Open the run's stores and build its processor over them, announcing `service_offer` through
/// DNS relays when there is one.
async fn build_processor(
    pc: &ProcessorConfig,
    data_storage: config::StorageConfig,
    measure_storage: config::StorageConfig,
    reassembly_limits: ReassemblyLimits,
    service_offer: Option<ServiceOffer>,
) -> anyhow::Result<Arc<Processor>> {
    let stores = NodeStores::open(&data_storage, &measure_storage).await?;
    let collector =
        EvidenceCollectorIdentity::new(pc.network_id(), pc.delegatee_key().delegator_did());
    let measure =
        PeriodicMeasure::new_with_evidence_storage(stores.measure, stores.evidence, collector)
            .await?;

    let builder = ProcessorBuilder::from_config(pc)?
        .storage(stores.data)
        .relay_storage(stores.relay)
        .removal_storage(stores.removals)
        .onion_entry_guard_storage(stores.guards)
        .replay_storage(stores.replay)
        .measure(measure)
        .stores_released(stores.released)
        .reassembly_limits(reassembly_limits);
    let builder = match service_offer {
        Some(offer) => builder.announce_services(offer),
        None => builder,
    };
    Ok(Arc::new(builder.build()?))
}

/// The onion HTTP proxy the configuration asks for, if any.
fn onion_http_proxy_options(c: &config::Config) -> anyhow::Result<Option<OnionHttpProxyOptions>> {
    let Some(addr) = c.onion_http_proxy_addr.as_deref() else {
        return Ok(None);
    };
    Ok(Some(OnionHttpProxyOptions {
        listen_addr: addr.parse::<SocketAddr>()?,
        max_connections: c.onion_http_proxy_max_connections,
        header_timeout: Duration::from_secs(c.onion_http_proxy_header_timeout_secs),
    }))
}

/// One task the run owns, awaited or aborted by its owner: a [`JoinSet`] of that one task, so
/// the run spawns nothing detached (the node crate forbids detached spawns, `clippy.toml`).
struct OwnedTask<T>(JoinSet<T>);

impl<T: Send + 'static> OwnedTask<T> {
    /// Run `task` on the current runtime, owned by the returned handle; dropping the handle
    /// aborts it.
    fn spawn(task: impl Future<Output = T> + Send + 'static) -> Self {
        let mut set = JoinSet::new();
        set.spawn(task);
        Self(set)
    }

    /// Abort the task; awaiting it then yields a cancelled [`JoinError`], unless it had ended.
    fn abort(&mut self) {
        self.0.abort_all();
    }
}

impl<T: 'static> Future for OwnedTask<T> {
    type Output = Result<T, JoinError>;

    /// The task's result, once; a handle whose result was taken never yields again.
    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        match self.0.poll_join_next(cx) {
            Poll::Ready(Some(result)) => Poll::Ready(result),
            Poll::Ready(None) | Poll::Pending => Poll::Pending,
        }
    }
}

/// Start the gateway task, if one is configured, and wait for its startup barrier.
async fn start_gateway(
    runner: Option<NativeGatewayRunner>,
    stop: StopToken,
) -> anyhow::Result<OwnedTask<anyhow::Result<()>>> {
    let configured = runner.is_some();
    let (started, startup) = tokio::sync::oneshot::channel();
    let mut task = OwnedTask::spawn(async move {
        match runner {
            Some(runner) => runner.run_with_startup_barrier(stop, started).await,
            None => std::future::pending::<anyhow::Result<()>>().await,
        }
    });
    await_gateway_startup(configured, startup, &mut task).await?;
    Ok(task)
}

/// Serve the internal API on port `internal` and the external API at `external`.
fn spawn_apis(
    tasks: &mut JoinSet<anyhow::Result<()>>,
    internal: u16,
    external: String,
    processor: Arc<Processor>,
    gateway_status: Option<GatewayStatusHandle>,
    api_security: Arc<ApiSecurity>,
) {
    let internal_processor = processor.clone();
    let internal_security = api_security.clone();
    tasks.spawn(async move {
        run_internal_api_with_gateway(
            internal,
            internal_processor,
            gateway_status,
            internal_security,
        )
        .await
        .context("internal API stopped")
    });
    tasks.spawn(async move {
        run_external_api(external, processor, api_security)
            .await
            .context("external API stopped")
    });
}

/// Run the processor's listener until `stop`; stopping before it is an error.
fn spawn_processor(processor: Arc<Processor>, stop: StopToken) -> OwnedTask<anyhow::Result<()>> {
    OwnedTask::spawn(async move {
        processor.listen_with(stop.clone()).await;
        if stop.should_stop() {
            Ok(())
        } else {
            anyhow::bail!("node processor listener stopped unexpectedly")
        }
    })
}

/// Wait for a signal or the first service to exit, then stop everything: the services at once,
/// the gateway and the processor through their cleanup.
async fn await_foreground_exit(
    stop: StopSource,
    mut tasks: JoinSet<anyhow::Result<()>>,
    mut gateway_task: OwnedTask<anyhow::Result<()>>,
    mut processor_task: OwnedTask<anyhow::Result<()>>,
    gateway_configured: bool,
) -> anyhow::Result<()> {
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
    gateway_task: &mut OwnedTask<anyhow::Result<()>>,
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
    task: &mut OwnedTask<anyhow::Result<()>>,
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

    use super::await_gateway_startup;
    use super::await_task_cleanup;
    use super::Cli;
    use super::OwnedTask;

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

    #[tokio::test]
    async fn processor_cleanup_gets_a_cooperative_stop_window() {
        let stop = rings_node::prelude::StopSource::new();
        let token = stop.token();
        let flushed = Arc::new(AtomicBool::new(false));
        let task_flushed = Arc::clone(&flushed);
        let mut task = OwnedTask::spawn(async move {
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
        let mut task = OwnedTask::spawn(async {
            Err(anyhow::anyhow!("gateway activation failed before startup"))
        });

        let result = await_gateway_startup(true, startup, &mut task).await;

        assert!(matches!(
            result,
            Err(error) if error.to_string().contains("activation failed before startup")
        ));
    }
}
