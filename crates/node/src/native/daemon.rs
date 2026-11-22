//! Foreground native node lifecycle shared by `rings run` and external daemon binaries.
//!
//! Opens authoritative persistent [`NodeStores`], builds a [`Processor`], installs optional
//! [`NativeDaemonHooks`] (DHT registrations, protocol/Kleisli backends, cooperative shutdown),
//! starts the application service, delegation signing-horizon task, internal/external JSON-RPC
//! HTTP endpoints, [`BootstrapSupervisor`], optional DNS relay, optional TUN gateway, and
//! optional onion HTTP proxy, and performs cooperative shutdown on `SIGINT`/`SIGTERM`.

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use rings_core::chunk::ReassemblyLimits;
use rings_core::utils::get_epoch_ms;
use rings_gateway::GatewayStatusHandle;
use tokio::task::JoinError;
use tokio::task::JoinSet;

use crate::dns_relay;
use crate::dns_relay::ServiceOffer;
use crate::dns_relay::ServicesConfig;
use crate::extension::protocols::relay::RelayHandle;
use crate::measure::EvidenceCollectorIdentity;
use crate::measure::PeriodicMeasure;
use crate::native::api_auth::load_or_create_api_token;
use crate::native::api_auth::ApiSecurity;
use crate::native::bootstrap::BootstrapSupervisor;
use crate::native::bootstrap::BootstrapTargets;
use crate::native::config;
use crate::native::endpoint::run_external_api;
use crate::native::endpoint::run_internal_api_with_gateway;
use crate::native::gateway::NativeGatewayRunner;
use crate::native::horizon::run_signing_horizon;
use crate::native::horizon::DelegationRenewalPaths;
use crate::native::stores::NodeStores;
use crate::onion::proxy::http::run_onion_http_proxy;
use crate::onion::proxy::http::OnionHttpProxyOptions;
use crate::prelude::StopSource;
use crate::prelude::StopToken;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;
use crate::provider::Provider;

/// Maximum wall-clock bound allotted to each foreground shutdown step (`gateway`, `processor`
/// listener drain, and [`Processor::close`]).
pub const FOREGROUND_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Runtime options for [`run_foreground_node`].
#[derive(Clone, Debug)]
pub struct NativeNodeRunOptions {
    /// Filesystem path of the node configuration file (used to resolve relative API token paths).
    pub config_path: String,
    /// Parsed native node configuration (after any CLI overrides have been applied).
    pub config: config::Config,
    /// Optional root directory overriding both `data_storage` and `measure_storage`.
    pub storage_path: Option<String>,
    /// Optional capacity override applied when `storage_path` is set.
    pub storage_capacity: Option<u32>,
    /// Inbound chunk reassembly bounds for the underlying [`Processor`].
    pub reassembly_limits: ReassemblyLimits,
    /// Optional override for `config.api_token_path`.
    pub api_token_path_override: Option<String>,
    /// Optional override for `config.api_allowed_origins` when non-empty.
    pub api_allowed_origins_override: Vec<String>,
    /// When `true`, forces `config.allow_remote_external_api = true`.
    pub allow_remote_external_api_override: bool,
}

impl NativeNodeRunOptions {
    /// Construct foreground run options over `config_path` and `config` with default overrides.
    pub fn new(config_path: impl Into<String>, config: config::Config) -> Self {
        Self {
            config_path: config_path.into(),
            config,
            storage_path: None,
            storage_capacity: None,
            reassembly_limits: ReassemblyLimits::default(),
            api_token_path_override: None,
            api_allowed_origins_override: Vec::new(),
            allow_remote_external_api_override: false,
        }
    }
}

/// Extension hooks allowing external service binaries to attach custom DHT registrations,
/// [`Provider`] protocols or onion Kleisli backends, and cooperative shutdown steps onto the
/// standard native node daemon lifecycle.
#[async_trait]
pub trait NativeDaemonHooks: Send + 'static {
    /// Customize [`ProcessorBuilder`] (e.g. via [`ProcessorBuilder::register_task`]) before the
    /// [`Processor`] is constructed.
    fn configure_builder(&mut self, builder: ProcessorBuilder) -> anyhow::Result<ProcessorBuilder> {
        Ok(builder)
    }

    /// Install protocols, custom local RPC methods, or onion Kleisli backends on `provider`
    /// before its application inbox is claimed and the listener starts.
    async fn configure_provider(&mut self, _provider: &Arc<Provider>) -> anyhow::Result<()> {
        Ok(())
    }

    /// Run service-specific cleanup after the foreground node listener and [`Processor::close`]
    /// have completed.
    async fn shutdown(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

impl NativeDaemonHooks for () {}

/// Run a full foreground native Rings node until `SIGINT`/`SIGTERM` or a fatal task exit.
pub async fn run_foreground_node<H: NativeDaemonHooks>(
    options: NativeNodeRunOptions,
    mut hooks: H,
) -> anyhow::Result<()> {
    let mut c = options.config;
    let pc = ProcessorConfig::try_from(c.clone())?;
    let api_security = configure_api_security(
        &options.config_path,
        &mut c,
        options.api_token_path_override,
        options.api_allowed_origins_override,
        options.allow_remote_external_api_override,
    )?;
    let onion_http_proxy = onion_http_proxy_options(&c)?;
    let (data_storage, measure_storage) =
        run_storage_configs(options.storage_path, options.storage_capacity, &c);
    let service_offer = c.services.as_ref().map(ServicesConfig::offer).transpose()?;
    let processor = build_processor(
        &pc,
        data_storage,
        measure_storage,
        options.reassembly_limits,
        service_offer,
        &mut hooks,
    )
    .await?;
    println!("Did: {}", processor.swarm.did());
    let provider = Arc::new(Provider::from_processor(processor.clone()));
    hooks.configure_provider(&provider).await?;
    // The relay is an opt-in extension owning its own engine: it serves TCP/UDP tunnels, the
    // ports this node serves through DNS relays, and the ingress of a DNS relay it runs.
    let relay = RelayHandle::install(&provider.extensions())?;
    let (services, relaying) = (c.services.as_ref(), c.dns_relay.as_ref());
    let dns_relay = dns_relay::open(services, relaying, &processor, &relay).await?;
    let needs_onion_runtime = processor.onion_role().registers_relay()
        || c.enabled_gateway().is_some()
        || onion_http_proxy.is_some();
    let onion = needs_onion_runtime
        .then(|| provider.onion_circuit())
        .transpose()?;
    let gateway_runner = match (c.enabled_gateway().cloned(), onion.as_ref()) {
        (Some(config), Some(onion)) => Some(NativeGatewayRunner::new(
            processor.clone(),
            onion.clone(),
            config,
        )?),
        _ => None,
    };
    let gateway_status = gateway_runner
        .as_ref()
        .map(NativeGatewayRunner::status_handle);
    // Managed bootstrap targets fail fast on invalid configuration; their supervisor is spawned
    // with the other run-owned tasks below and its evidence is the backend's observer.
    let renewal = DelegationRenewalPaths::from_config(&c)?;
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
    // Renew the delegation in process before its signing horizon when `delegator_key` is
    // configured (#1153), and end at the horizon otherwise (#1063).
    let horizon_processor = processor.clone();
    tasks.spawn(async move {
        Err(
            run_signing_horizon(horizon_processor, renewal, get_epoch_ms)
                .await
                .into(),
        )
    });
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
    match (onion_http_proxy, onion) {
        (Some(proxy_options), Some(onion)) => {
            let proxy_processor = processor.clone();
            tasks.spawn(async move {
                run_onion_http_proxy(proxy_options, proxy_processor, onion)
                    .await
                    .context("Onion HTTP proxy stopped")
            });
        }
        (_, onion) => drop(onion),
    }
    drop((relay, provider));
    await_foreground_exit(
        stop,
        tasks,
        gateway_task,
        processor_task,
        processor,
        gateway_configured,
        &mut hooks,
    )
    .await
}

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
/// DNS relays when there is one and applying `hooks`.
async fn build_processor<H: NativeDaemonHooks>(
    pc: &ProcessorConfig,
    data_storage: config::StorageConfig,
    measure_storage: config::StorageConfig,
    reassembly_limits: ReassemblyLimits,
    service_offer: Option<ServiceOffer>,
    hooks: &mut H,
) -> anyhow::Result<Arc<Processor>> {
    let stores = NodeStores::open(&data_storage, &measure_storage).await?;
    let collector =
        EvidenceCollectorIdentity::new(pc.network_id(), pc.delegatee_key().delegator_did());
    let measure = PeriodicMeasure::new_with_evidence_storage_and_clock(
        stores.measure,
        stores.evidence,
        collector,
        pc.clock().clone(),
    )
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
    let builder = hooks.configure_builder(builder)?;
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
#[doc(hidden)]
pub struct OwnedTask<T>(JoinSet<T>);

impl<T: Send + 'static> OwnedTask<T> {
    /// Run `task` on the current runtime, owned by the returned handle; dropping the handle
    /// aborts it.
    pub fn spawn(task: impl Future<Output = T> + Send + 'static) -> Self {
        let mut set = JoinSet::new();
        set.spawn(task);
        Self(set)
    }

    /// Abort the task; awaiting it then yields a cancelled [`JoinError`], unless it had ended.
    pub fn abort(&mut self) {
        self.0.abort_all();
    }
}

impl<T: 'static> Future for OwnedTask<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        match self.0.poll_join_next(cx) {
            Poll::Ready(Some(result)) => Poll::Ready(result),
            Poll::Ready(None) | Poll::Pending => Poll::Pending,
        }
    }
}

/// Start the gateway task, if one is configured, and wait for its startup barrier.
#[doc(hidden)]
pub async fn start_gateway(
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
#[doc(hidden)]
pub fn spawn_processor(
    processor: Arc<Processor>,
    stop: StopToken,
) -> OwnedTask<anyhow::Result<()>> {
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
/// the gateway, the processor listener, `Processor::close` (`leave`, `swarm.close`, and
/// measurement flush), and finally [`NativeDaemonHooks::shutdown`].
async fn await_foreground_exit<H: NativeDaemonHooks>(
    stop: StopSource,
    tasks: JoinSet<anyhow::Result<()>>,
    gateway_task: OwnedTask<anyhow::Result<()>>,
    processor_task: OwnedTask<anyhow::Result<()>>,
    processor: Arc<Processor>,
    gateway_configured: bool,
    hooks: &mut H,
) -> anyhow::Result<()> {
    await_foreground_exit_with_signal(
        shutdown_signal(),
        stop,
        tasks,
        gateway_task,
        processor_task,
        processor,
        gateway_configured,
        hooks,
    )
    .await
}

/// Testable signal-injected variant of `await_foreground_exit`.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn await_foreground_exit_with_signal<H: NativeDaemonHooks>(
    shutdown: impl Future<Output = anyhow::Result<()>>,
    stop: StopSource,
    mut tasks: JoinSet<anyhow::Result<()>>,
    mut gateway_task: OwnedTask<anyhow::Result<()>>,
    mut processor_task: OwnedTask<anyhow::Result<()>>,
    processor: Arc<Processor>,
    gateway_configured: bool,
    hooks: &mut H,
) -> anyhow::Result<()> {
    enum ForegroundExit {
        Signal(anyhow::Result<()>),
        Service(Option<Result<anyhow::Result<()>, JoinError>>),
        Gateway(Result<anyhow::Result<()>, JoinError>),
        Processor(Result<anyhow::Result<()>, JoinError>),
    }
    let exit = tokio::select! {
        signal = shutdown => ForegroundExit::Signal(signal),
        service = tasks.join_next() => ForegroundExit::Service(service),
        gateway = &mut gateway_task => ForegroundExit::Gateway(gateway),
        processor = &mut processor_task => ForegroundExit::Processor(processor),
    };
    stop.request_stop();
    // Stop request-serving tasks immediately, but keep the processor task separate so it can
    // flush measurements through its cooperative `listen_with` shutdown path.
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}

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
    let processor_close = await_processor_close(&processor).await;
    let hooks_cleanup = hooks.shutdown().await;
    combine_foreground_results(
        primary,
        gateway_cleanup,
        processor_cleanup,
        processor_close,
        hooks_cleanup,
    )
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
    println!("API authentication token file initialized.");
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

/// Wait for `gateway_task` to pass its startup barrier when `configured`.
#[doc(hidden)]
pub async fn await_gateway_startup(
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

/// Await `task` up to [`FOREGROUND_CLEANUP_TIMEOUT`], aborting on timeout.
#[doc(hidden)]
pub async fn await_task_cleanup(
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

async fn await_processor_close(processor: &Processor) -> anyhow::Result<()> {
    match tokio::time::timeout(FOREGROUND_CLEANUP_TIMEOUT, processor.close()).await {
        Ok(result) => result.context("node processor close failed"),
        Err(_) => Err(anyhow::anyhow!(
            "node processor close did not finish within {FOREGROUND_CLEANUP_TIMEOUT:?}"
        )),
    }
}

fn combine_foreground_results(
    primary: anyhow::Result<()>,
    gateway_cleanup: anyhow::Result<()>,
    processor_cleanup: anyhow::Result<()>,
    processor_close: anyhow::Result<()>,
    hooks_cleanup: anyhow::Result<()>,
) -> anyhow::Result<()> {
    let failures = [
        primary.err().map(|error| format!("foreground: {error}")),
        gateway_cleanup
            .err()
            .map(|error| format!("gateway cleanup: {error}")),
        processor_cleanup
            .err()
            .map(|error| format!("processor cleanup: {error}")),
        processor_close
            .err()
            .map(|error| format!("processor close: {error}")),
        hooks_cleanup
            .err()
            .map(|error| format!("daemon hook shutdown: {error}")),
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
