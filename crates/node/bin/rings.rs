use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use clap::ArgAction;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use futures::pin_mut;
use futures::StreamExt;
use rings_node::extension::Backend;
use rings_node::logging::init_logging;
use rings_node::logging::LogLevel;
use rings_node::measure::PeriodicMeasure;
use rings_node::native::cli::Client;
use rings_node::native::config;
use rings_node::native::endpoint::run_external_api;
use rings_node::native::endpoint::run_internal_api;
use rings_node::onion::proxy::http::run_onion_http_proxy;
use rings_node::onion::proxy::http::OnionHttpProxyOptions;
use rings_node::onion::tcp::NativeOnionCircuitHandle;
use rings_node::onion::tcp::NativeOnionTcpExitConfig;
use rings_node::onion::OnionExitService;
use rings_node::onion::OnionExitTarget;
use rings_node::onion::OnionExitTransport;
use rings_node::onion::OnionServiceName;
use rings_node::prelude::rings_core::chunk::ReassemblyLimits;
use rings_node::prelude::rings_core::dht::Did;
use rings_node::prelude::rings_core::ecc::SecretKey;
use rings_node::prelude::rings_core::storage::sled::SledStorage;
use rings_node::prelude::SessionSkBuilder;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use rings_node::util::ensure_parent_dir;
use rings_node::util::expand_home;
use tokio::io;
use tokio::io::AsyncBufReadExt;

#[derive(Parser, Debug)]
#[command(about, version, author)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[arg(long, default_value_t = LogLevel::Info, value_enum, env)]
    log_level: LogLevel,

    #[arg(
        long,
        value_enum,
        default_value = "multi-thread",
        env,
        help = "Tokio runtime scheduler for this process"
    )]
    runtime: RuntimeFlavor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RuntimeFlavor {
    MultiThread,
    CurrentThread,
}

impl RuntimeFlavor {
    fn build(self) -> std::io::Result<tokio::runtime::Runtime> {
        match self {
            Self::MultiThread => tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build(),
            Self::CurrentThread => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ReassemblyProfile {
    Production,
    Constrained,
}

impl ReassemblyProfile {
    fn limits(self) -> ReassemblyLimits {
        match self {
            Self::Production => ReassemblyLimits::production(),
            Self::Constrained => ReassemblyLimits::constrained(),
        }
    }
}

fn parse_onion_exit_service(raw: &str) -> Result<OnionExitService, String> {
    let (name, transport) = raw
        .split_once(':')
        .map_or((raw, raw), |(name, transport)| (name, transport));
    let name = name.trim();
    if name.is_empty() {
        return Err("onion exit service name must not be empty".to_string());
    }
    let transport = match transport.trim().to_ascii_lowercase().as_str() {
        "tcp" => OnionExitTransport::Tcp,
        "udp" => OnionExitTransport::Udp,
        "webtransport" | "web-transport" => OnionExitTransport::WebTransport,
        "requestresponse" | "request-response" => OnionExitTransport::RequestResponse,
        "https" => OnionExitTransport::Https,
        other => {
            return Err(format!(
                "unsupported onion exit transport {other:?}; expected tcp, udp, webtransport, request-response, or https"
            ));
        }
    };
    OnionExitService::new(name, transport).map_err(|error| error.to_string())
}

fn parse_onion_service_name(raw: &str) -> Result<OnionServiceName, String> {
    OnionServiceName::parse(raw).map_err(|error| error.to_string())
}

fn validate_native_onion_exit_services(services: &[OnionExitService]) -> anyhow::Result<()> {
    for service in services {
        if service.transport != OnionExitTransport::Tcp {
            anyhow::bail!(
                "native onion exits can serve only TCP transport; service {:?} uses {:?}. Use a browser node for HTTPS exits.",
                service.name,
                service.transport
            );
        }
    }
    Ok(())
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum Command {
    #[command(about = "Initializes a node with the given configuration.")]
    Init(InitCommand),
    #[command(about = "Creates a new session secret key.")]
    NewSession(NewSessionCommand),
    #[command(about = "Starts a long-running node daemon.")]
    Run(Box<RunCommand>),
    #[command(about = "Provides chat room-like functionality on the Rings Network.")]
    Pubsub(PubsubCommand),
    #[command(about = "Connects to a remote peer.", subcommand)]
    Connect(ConnectCommand),
    #[command(about = "Manages peers on the network.", subcommand)]
    Peer(PeerCommand),
    #[command(about = "Sends a message to another peer.", subcommand)]
    Send(SendCommand),
    #[command(about = "Registers or looks up a service on the network.", subcommand)]
    Service(ServiceCommand),
    #[command(
        about = "Show information of swarm. Include transport table, successors, predecessor, and finger table."
    )]
    Inspect(InspectCommand),
}

#[derive(Args, Debug)]
struct ConfigArgs {
    #[arg(
        long,
        short = 'c',
        env,
        default_value = "~/.rings/config.yaml",
        help = "Config file location"
    )]
    pub config: String,
}

#[derive(Args, Debug)]
struct InitCommand {
    #[command(flatten)]
    session_args: SessionArgs,

    #[arg(
        long,
        default_value = "~/.rings/config.yaml",
        help = "The location of config file"
    )]
    pub location: String,
}

#[derive(Args, Debug)]
struct NewSessionCommand {
    #[command(flatten)]
    session_args: SessionArgs,
}

#[derive(Args, Debug)]
struct RunCommand {
    #[arg(
        long,
        help = "Rings node external api listen address. If not provided, use external_api_addr in config file or 127.0.0.1:50001",
        env
    )]
    pub external_api_addr: Option<String>,

    #[arg(
        long,
        help = "Rings node internal api listen port. If not provided, use internal_api_port in config file or 50000"
    )]
    pub internal_api_port: Option<u16>,

    #[arg(
        long,
        help = "ICE server list. If not provided, use ice_servers in config file or stun://stun.l.google.com:19302",
        env
    )]
    pub ice_servers: Option<String>,

    #[arg(
        long = "key",
        short = 'k',
        help = "Your ECDSA key. If not provided, use ECDSA_KEY in env or ecdsa_key in config file",
        env
    )]
    pub ecdsa_key: Option<SecretKey>,

    #[arg(
        long,
        help = "Stabilization interval in seconds. If not provided, use stabilize_interval in config file or 3",
        env
    )]
    pub stabilize_interval: Option<u64>,

    #[arg(long, help = "external ip address", env)]
    pub external_ip: Option<String>,

    #[arg(
        long,
        help = "Minimum UDP port used by native WebRTC ICE gathering. Must be paired with --webrtc-udp-port-max.",
        env
    )]
    pub webrtc_udp_port_min: Option<u16>,

    #[arg(
        long,
        help = "Maximum UDP port used by native WebRTC ICE gathering. Must be paired with --webrtc-udp-port-min.",
        env
    )]
    pub webrtc_udp_port_max: Option<u16>,

    #[arg(
        long,
        help = "Storage files location. If not provided, use storage.path in config file or ~/.local/share/rings",
        env
    )]
    pub storage_path: Option<String>,

    #[arg(
        long,
        default_value = "200000000",
        help = "Storage capacity. If not provider, use storage.capacity in config file or 200000000",
        env
    )]
    pub storage_capacity: Option<u32>,

    #[arg(
        long,
        value_enum,
        default_value = "production",
        env,
        help = "Inbound chunk reassembly memory profile"
    )]
    pub reassembly_profile: ReassemblyProfile,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Advertise this node as an onion relay in the online-node registry",
        env
    )]
    pub advertise_onion_relay: bool,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Publish this node as an onion exit in the application-layer exit registry",
        env
    )]
    pub advertise_onion_exit: bool,

    #[arg(
        long,
        value_parser = parse_onion_exit_service,
        help = "Exit service in name:transport form, e.g. https:https or web:tcp. May be repeated.",
        env
    )]
    pub onion_exit_service: Vec<OnionExitService>,

    #[arg(
        long,
        help = "Allow-list target for onion exit policy. May be repeated.",
        env
    )]
    pub onion_exit_allow_target: Vec<String>,

    #[arg(
        long,
        help = "Deny-list target for onion exit policy. May be repeated.",
        env
    )]
    pub onion_exit_deny_target: Vec<String>,

    #[arg(long, help = "Maximum onion circuits this exit will serve", env)]
    pub onion_exit_max_circuits: Option<u32>,

    #[arg(
        long,
        help = "Maximum streams per onion circuit this exit will serve",
        env
    )]
    pub onion_exit_max_streams_per_circuit: Option<u32>,

    #[arg(long, help = "Maximum bytes per minute this exit will serve", env)]
    pub onion_exit_max_bytes_per_minute: Option<u64>,

    #[arg(long, help = "Onion-exit registry heartbeat interval in seconds", env)]
    pub onion_exit_heartbeat_interval_secs: Option<u64>,

    #[arg(long, help = "Onion-exit registry descriptor TTL in seconds", env)]
    pub onion_exit_ttl_secs: Option<u64>,

    #[arg(
        long,
        help = "Bind a local HTTP CONNECT proxy that routes client TCP streams through onion exits, e.g. 127.0.0.1:18080",
        env
    )]
    pub onion_http_proxy_addr: Option<String>,

    #[arg(
        long,
        value_parser = parse_onion_service_name,
        help = "TCP onion-exit service used by the local HTTP CONNECT proxy, e.g. tcp or web",
        env
    )]
    pub onion_http_proxy_service: Option<OnionServiceName>,

    #[arg(
        long,
        help = "Desired hop count for the local onion HTTP proxy. 0 uses node default.",
        env
    )]
    pub onion_http_proxy_hop_count: Option<usize>,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Allow the local onion HTTP proxy to use shorter routes when too few relays are live",
        env
    )]
    pub onion_http_proxy_allow_short_paths: bool,

    #[arg(
        long,
        help = "Maximum seconds to wait for one HTTP CONNECT header",
        env
    )]
    pub onion_http_proxy_header_timeout_secs: Option<u64>,

    #[arg(
        long,
        help = "Maximum concurrent local HTTP CONNECT proxy connections",
        env
    )]
    pub onion_http_proxy_max_connections: Option<usize>,

    #[command(flatten)]
    config_args: ConfigArgs,
}

#[derive(Args, Debug)]
struct ClientArgs {
    #[arg(
        long,
        short = 'u',
        help = "rings-node endpoint url. If not provided, use endpoint_url in config file or http://127.0.0.1:50000",
        env
    )]
    endpoint_url: Option<String>,

    #[arg(
        long = "key",
        short = 'k',
        env,
        help = "Your ECDSA key. If not provided, use ECDSA_KEY in env or ecdsa_key in config file"
    )]
    pub ecdsa_key: Option<SecretKey>,

    #[command(flatten)]
    config_args: ConfigArgs,
}

impl ClientArgs {
    async fn new_client(&self) -> anyhow::Result<Client> {
        let c = config::Config::read_fs(&self.config_args.config)?;
        let endpoint_url = self.endpoint_url.as_ref().unwrap_or(&c.endpoint_url);
        Client::new(endpoint_url)
    }
}

#[derive(Args, Debug)]
struct SessionArgs {
    #[arg(
        long,
        short = 's',
        default_value = "~/.rings/session_sk",
        help = "The location of session_sk file"
    )]
    pub session_sk: String,

    #[arg(
        long,
        short = 'k',
        help = "Your ecdsa_key. If not provided, a random key will be used"
    )]
    pub ecdsa_key: Option<SecretKey>,

    #[arg(
        long = "key-file",
        value_name = "FILE",
        conflicts_with = "ecdsa_key",
        help = "Read your ECDSA key from a file instead of passing it on the command line"
    )]
    pub ecdsa_key_file: Option<String>,

    #[arg(
        long,
        default_value = "2592000",
        help = "The ttl of session file in seconds"
    )]
    pub ttl: u64,
}

impl SessionArgs {
    fn new_session_then_write_to_fs(&self) -> anyhow::Result<&std::path::Path> {
        let key = self.load_or_create_key()?;
        let key_did: Did = key.address().into();

        let ssk_builder = SessionSkBuilder::new(key_did.to_string(), "secp256k1".to_string())
            .set_ttl(self.ttl * 1000);
        let unsigned_proof = ssk_builder.unsigned_proof();

        let sig = key.sign(&unsigned_proof).to_vec();
        let ssk_builder = ssk_builder.set_session_sig(sig);

        let ssk = ssk_builder.build()?;
        let ssk_dump = ssk.dump()?;

        let ssk_path = std::path::Path::new(&self.session_sk);
        ensure_parent_dir(ssk_path)?;
        std::fs::write(expand_home(ssk_path)?, ssk_dump)?;
        println!("Your session_sk file has saved to: {}", ssk_path.display());

        Ok(ssk_path)
    }

    fn load_or_create_key(&self) -> anyhow::Result<SecretKey> {
        if let Some(key) = self.ecdsa_key {
            return Ok(key);
        }

        if let Some(key_file) = &self.ecdsa_key_file {
            return read_secret_key_file(key_file);
        }

        let rand_key = SecretKey::random();
        println!("Your random ecdsa key is: {}", rand_key.to_string());
        Ok(rand_key)
    }
}

fn read_secret_key_file(path: &str) -> anyhow::Result<SecretKey> {
    let path = expand_home(path)?;
    let raw = std::fs::read_to_string(path)?;
    let Some(key) = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
    else {
        anyhow::bail!("ECDSA key file contains no key entries");
    };
    let key = key.strip_prefix("0x").unwrap_or(key);
    SecretKey::from_str(key).map_err(|_| anyhow::anyhow!("ECDSA key file contains an invalid key"))
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum ConnectCommand {
    #[command(about = "Connects to a node using its URL.")]
    Node(ConnectUrlCommand),
    #[command(about = "Connects to a node using its DID via DHT.")]
    Did(ConnectWithDidCommand),
    #[command(about = "Connects to a node using its seed from a URL or file.")]
    Seed(ConnectWithSeedCommand),
}

#[derive(Args, Debug)]
struct ConnectUrlCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    node_url: String,
}

#[derive(Args, Debug)]
struct ConnectWithDidCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    did: String,
}

#[derive(Args, Debug)]
struct ConnectWithSeedCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    source: String,
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum PeerCommand {
    #[command(about = "List peers")]
    List(PeerListCommand),
    #[command(about = "Disconnect peer")]
    Disconnect(PeerDisconnectCommand),
}

#[derive(Args, Debug)]
struct PeerListCommand {
    #[command(flatten)]
    client_args: ClientArgs,
}

#[derive(Args, Debug)]
struct PeerDisconnectCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    address: String,
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum SendCommand {
    #[command(about = "Sends a namespaced message to a peer.")]
    Message(SendMessageCommand),
}

#[derive(Args, Debug)]
struct PubsubCommand {
    #[command(flatten)]
    client_args: ClientArgs,
    topic: String,
}

#[derive(Args, Debug)]
struct SendMessageCommand {
    #[command(flatten)]
    client_args: ClientArgs,
    to_did: String,
    namespace: String,
    data: String,
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum ServiceCommand {
    Register(ServiceRegisterCommand),
    Lookup(ServiceLookupCommand),
}

#[derive(Args, Debug)]
struct ServiceRegisterCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    name: String,
}

#[derive(Args, Debug)]
struct ServiceLookupCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    name: String,
}

#[derive(Args, Debug)]
struct InspectCommand {
    #[command(flatten)]
    client_args: ClientArgs,
}

#[allow(clippy::too_many_arguments)]
async fn daemon_run(args: RunCommand) -> anyhow::Result<()> {
    let mut c = config::Config::read_fs(args.config_args.config)?;

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
    if c.advertise_onion_exit {
        validate_native_onion_exit_services(&c.onion_exit_services)?;
    }

    let pc = ProcessorConfig::try_from(c.clone())?;
    let onion_session_sk = pc.session_sk();
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
        SledStorage::new_with_cap_and_path(data_storage.capacity, data_storage.path).await?,
    );
    let per_measure_storage = Box::new(
        SledStorage::new_with_cap_and_path(measure_storage.capacity, measure_storage.path).await?,
    );

    let measure = PeriodicMeasure::new(per_measure_storage);

    let processor = Arc::new(
        ProcessorBuilder::from_config(&pc)?
            .storage(per_data_storage)
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
        onion_session_sk,
        advertise_onion_relay,
        onion_exit_config,
    )?;
    // SNARK is a namespaced protocol now; register it so the daemon can prove/verify.
    #[cfg(feature = "snark")]
    rings_node::extension::snark::SNARKBehaviour::default().register(provider.as_ref())?;
    // The Backend decodes inbound custom messages as namespaced envelopes and routes
    // them to the protocol registry.
    let backend = Arc::new(Backend::new(provider));
    processor.swarm.set_callback(backend)?;

    let processor_clone1 = processor.clone();
    let processor_clone2 = processor.clone();
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
        let _ = futures::join!(
            processor.listen(),
            run_internal_api(c.internal_api_port, processor_clone2),
            run_external_api(c.external_api_addr, processor_clone1),
            run_onion_http_proxy(proxy_options, processor.clone(), onion),
        );
    } else {
        let _ = futures::join!(
            processor.listen(),
            run_internal_api(c.internal_api_port, processor_clone2),
            run_external_api(c.external_api_addr, processor_clone1),
        );
    }

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
    dotenv::dotenv().ok();

    let cli = Cli::parse();
    init_logging(cli.log_level.clone());
    let runtime = cli.runtime.build()?;
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Run(args) => daemon_run(*args).await,
        Command::Pubsub(args) => pubsub_run(args.client_args, args.topic).await,
        Command::Connect(ConnectCommand::Node(args)) => {
            args.client_args
                .new_client()
                .await?
                .connect_peer_via_http(args.node_url.as_str())
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
            let session_sk_path = args.session_args.new_session_then_write_to_fs()?;
            let config = config::Config::new(session_sk_path);
            let p = config.write_fs(&args.location)?;
            println!("Your config file has saved to: {p}");
            Ok(())
        }
        Command::NewSession(args) => {
            args.session_args.new_session_then_write_to_fs()?;
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
