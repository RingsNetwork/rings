//! Rings native node command-line entrypoint.

use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
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
use rings_node::native::api_auth::load_api_token;
use rings_node::native::api_auth::load_api_token_file;
use rings_node::native::api_auth::load_or_create_api_token;
use rings_node::native::api_auth::ApiSecurity;
use rings_node::native::cli::Client;
use rings_node::native::config;
use rings_node::native::endpoint::run_external_api;
use rings_node::native::endpoint::run_internal_api_with_gateway;
use rings_node::native::gateway::NativeGatewayRunner;
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
use rings_node::prelude::StopSource;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use rings_node::util::ensure_parent_dir;
use rings_node::util::expand_home;
use tokio::io;
use tokio::io::AsyncBufReadExt;
use tokio::task::JoinError;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;

const FOREGROUND_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser, Debug)]
#[command(about, version, author)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[arg(long, default_value_t = LogLevel::default(), value_enum, env)]
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
        "https" => OnionExitTransport::Tcp,
        other => {
            return Err(format!(
                "unsupported onion exit transport {other:?}; expected tcp, udp, webtransport, request-response, or https (alias for tcp)"
            ));
        }
    };
    OnionExitService::new(name, transport).map_err(|error| error.to_string())
}

fn parse_onion_service_name(raw: &str) -> Result<OnionServiceName, String> {
    OnionServiceName::parse(raw).map_err(|error| error.to_string())
}

/// Resolves a handshake payload argument that may be `-`, meaning "read it from stdin".
///
/// The base58-check offer/answer strings are long and awkward to pass inline, so the
/// manual-handshake subcommands accept `-` and consume stdin (trimmed) instead.
fn payload_arg_or_stdin(value: &str) -> anyhow::Result<String> {
    if value != "-" {
        return Ok(value.to_string());
    }

    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
        .context("failed to read handshake payload from stdin")?;
    Ok(buf.trim().to_string())
}

fn validate_native_onion_exit_services(services: &[OnionExitService]) -> anyhow::Result<()> {
    for service in services {
        if service.transport != OnionExitTransport::Tcp {
            anyhow::bail!(
                "native onion exits can serve only TCP transport; service {:?} uses {:?}",
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
    #[command(about = "Runs a foreground, composable Rings node.")]
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
        action = ArgAction::SetTrue,
        help = "Enable the native TUN gateway configured by the gateway section",
        env
    )]
    pub gateway: bool,

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
        help = "API Bearer token file. Relative paths are resolved next to the node config file",
        env
    )]
    pub api_token_path: Option<String>,

    #[arg(
        long = "api-allowed-origin",
        action = ArgAction::Append,
        help = "Exact browser origin permitted to call the authenticated API; repeat as needed",
        env,
        value_delimiter = ','
    )]
    pub api_allowed_origins: Vec<String>,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Explicitly permit external_api_addr to bind a non-loopback address",
        env
    )]
    pub allow_remote_external_api: bool,

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
        help = "Stabilization interval in seconds. If not provided, use stabilize_interval in config file or 15",
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
        help = "Exit service in name:transport form, e.g. https:tcp or web:tcp. May be repeated.",
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
        long,
        help = "API Bearer token file. Relative paths are resolved next to the node config file",
        env
    )]
    api_token_path: Option<String>,

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
        let configured_path = self
            .api_token_path
            .as_deref()
            .or(c.api_token_path.as_deref());
        let token = load_api_token(&self.config_args.config, configured_path)?;
        Client::with_api_token(endpoint_url, token.into_secret())
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

        let sig = key.sign(&unsigned_proof)?.to_vec();
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
        if let Some(key) = &self.ecdsa_key {
            return Ok(key.clone());
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
    #[command(
        about = "Creates a manual-handshake offer targeting a peer DID; prints the encoded offer."
    )]
    Offer(ConnectOfferCommand),
    #[command(
        about = "Answers a peer's offer; prints the encoded answer to return to the offerer."
    )]
    Answer(ConnectAnswerCommand),
    #[command(about = "Accepts a peer's answer, completing the manual handshake.")]
    Accept(ConnectAcceptCommand),
}

#[derive(Args, Debug)]
struct ConnectUrlCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    node_url: String,

    #[arg(
        long,
        help = "Bearer token file for the remote peer API",
        env = "RINGS_REMOTE_API_TOKEN_FILE"
    )]
    remote_api_token_file: Option<String>,
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

#[derive(Args, Debug)]
struct ConnectOfferCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    /// DID of the peer this offer targets.
    did: String,
}

#[derive(Args, Debug)]
struct ConnectAnswerCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    /// Encoded offer produced by the peer's `connect offer`, or `-` to read it from stdin.
    offer: String,
}

#[derive(Args, Debug)]
struct ConnectAcceptCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    /// Encoded answer produced by the peer's `connect answer`, or `-` to read it from stdin.
    answer: String,
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
            anyhow::bail!("--gateway requires a gateway section in the node config file");
        };
        gateway.enabled = true;
    }
    if c.advertise_onion_exit {
        validate_native_onion_exit_services(&c.onion_exit_services)?;
    }
    let pc = ProcessorConfig::try_from(c.clone())?;
    let api_security = configure_api_security(
        &config_path,
        &mut c,
        args.api_token_path,
        args.api_allowed_origins,
        args.allow_remote_external_api,
    )?;

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
    let gateway_config = c.gateway.clone().filter(|gateway| gateway.enabled);

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

    let measure = PeriodicMeasure::new(per_measure_storage).await?;

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
    let gateway_runner = gateway_config
        .map(|config| NativeGatewayRunner::new(processor.clone(), onion.clone(), config))
        .transpose()?;
    let gateway_status = gateway_runner
        .as_ref()
        .map(NativeGatewayRunner::status_handle);
    // The Backend decodes inbound custom messages as namespaced envelopes and routes
    // them to the protocol registry.
    let backend = Arc::new(Backend::new(provider));
    processor.swarm.set_callback(backend)?;

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
            let session_sk_path = args.session_args.new_session_then_write_to_fs()?;
            let config = config::Config::new(session_sk_path);
            let p = config.write_fs(&args.location)?;
            let api_token = load_or_create_api_token(&p, config.api_token_path.as_deref())?;
            println!("Your config file has saved to: {p}");
            println!(
                "API authentication token file: {}",
                api_token.path().display()
            );
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

    fn parse_without_log_level_env<const N: usize>(args: [&str; N]) -> Result<Cli, clap::Error> {
        let matches = Cli::command()
            .mut_arg("log_level", |arg| arg.env(None::<&'static str>))
            .try_get_matches_from(args)?;
        Cli::from_arg_matches(&matches)
    }

    #[test]
    fn test_cli_default_log_level_is_error() {
        let parsed =
            parse_without_log_level_env(["rings", "--runtime", "current-thread", "new-session"]);

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
            "new-session",
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
