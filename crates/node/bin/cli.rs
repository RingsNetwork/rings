//! The command line of the native node: the top-level parser, its subcommands, and the
//! arguments each takes.

use std::str::FromStr;

use anyhow::Context;
use clap::ArgAction;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use rings_node::logging::LogLevel;
use rings_node::native::api_auth::load_api_token;
use rings_node::native::cli::Client;
use rings_node::native::config;
use rings_node::onion::OnionServiceName;
use rings_node::prelude::rings_core::chunk::ReassemblyLimits;
use rings_node::prelude::rings_core::dht::Did;
use rings_node::prelude::rings_core::ecc::SecretKey;
use rings_node::prelude::DelegationBuilder;
use rings_node::util::ensure_parent_dir;
use rings_node::util::expand_home;

#[derive(Parser, Debug)]
#[command(about, version, author)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,

    #[arg(long, default_value_t = LogLevel::default(), value_enum, env)]
    pub(crate) log_level: LogLevel,

    #[arg(
        long,
        value_enum,
        default_value = "multi-thread",
        env,
        help = "Tokio runtime scheduler for this process"
    )]
    pub(crate) runtime: RuntimeFlavor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum RuntimeFlavor {
    MultiThread,
    CurrentThread,
}

impl RuntimeFlavor {
    pub(crate) fn build(self) -> std::io::Result<tokio::runtime::Runtime> {
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
pub(crate) enum ReassemblyProfile {
    Production,
    Constrained,
}

impl ReassemblyProfile {
    pub(crate) fn limits(self) -> ReassemblyLimits {
        match self {
            Self::Production => ReassemblyLimits::production(),
            Self::Constrained => ReassemblyLimits::constrained(),
        }
    }
}

/// Parse a singular onion-exit service name from the run command.
pub(crate) fn parse_onion_exit_service(raw: &str) -> Result<OnionServiceName, String> {
    OnionServiceName::parse(raw).map_err(|error| error.to_string())
}

/// Parse a canonical service name for client-side onion proxy options.
pub(crate) fn parse_onion_service_name(raw: &str) -> Result<OnionServiceName, String> {
    OnionServiceName::parse(raw).map_err(|error| error.to_string())
}

/// Resolves a handshake payload argument that may be `-`, meaning "read it from stdin".
///
/// The base58-check offer/answer strings are long and awkward to pass inline, so the
/// manual-handshake subcommands accept `-` and consume stdin (trimmed) instead.
pub(crate) fn payload_arg_or_stdin(value: &str) -> anyhow::Result<String> {
    if value != "-" {
        return Ok(value.to_string());
    }

    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
        .context("failed to read handshake payload from stdin")?;
    Ok(buf.trim().to_string())
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
pub(crate) enum Command {
    #[command(about = "Initializes a node with the given configuration.")]
    Init(InitCommand),
    #[command(about = "Creates a new delegatee signing key.")]
    NewDelegation(NewDelegationCommand),
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
pub(crate) struct ConfigArgs {
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
pub(crate) struct InitCommand {
    #[command(flatten)]
    pub(crate) delegation_args: DelegationArgs,

    #[arg(
        long,
        default_value = "~/.rings/config.yaml",
        help = "The location of config file"
    )]
    pub location: String,
}

#[derive(Args, Debug)]
pub(crate) struct NewDelegationCommand {
    #[command(flatten)]
    pub(crate) delegation_args: DelegationArgs,
}

#[derive(Args, Debug)]
pub(crate) struct RunCommand {
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Start the native TUN gateway from the config's gateway section for this run; the section alone never starts it (rings init writes it with enabled: false)",
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
        help = "API Bearer token file guarding the internal API and the external status and registry reads; the external handshake (nodeDid, answerOffer) is public. Relative paths are resolved next to the node config file",
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
        long,
        help = "Stabilization interval in seconds. If not provided, use stabilize_interval in config file or 15",
        env
    )]
    pub stabilize_interval: Option<u64>,

    #[arg(
        long,
        help = "Seed document URL (file:// or http(s)://) whose peers join the managed bootstrap targets of the config's bootstrap section; the run redials each target through its HTTP endpoint whenever it stops being reachable through the overlay",
        env
    )]
    pub bootstrap_seed: Option<String>,

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
        help = "TCP-backed exit service name, e.g. https or web. May be repeated.",
        env
    )]
    pub onion_exit_service: Vec<OnionServiceName>,

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
    pub(crate) config_args: ConfigArgs,
}

#[derive(Args, Debug)]
pub(crate) struct ClientArgs {
    #[arg(
        long,
        short = 'u',
        help = "rings-node endpoint url. If not provided, use endpoint_url in config file or http://127.0.0.1:50000",
        env
    )]
    pub(crate) endpoint_url: Option<String>,

    #[arg(
        long,
        help = "API Bearer token file of the local node's internal API. Relative paths are resolved next to the node config file",
        env
    )]
    pub(crate) api_token_path: Option<String>,

    #[command(flatten)]
    pub(crate) config_args: ConfigArgs,
}

impl ClientArgs {
    pub(crate) async fn new_client(&self) -> anyhow::Result<Client> {
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
pub(crate) struct DelegationArgs {
    #[arg(
        long,
        short = 's',
        default_value = "~/.rings/delegatee_key",
        help = "The location of delegatee_key file"
    )]
    pub delegatee_key: String,

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
        help = "The ttl of delegation file in seconds"
    )]
    pub ttl: u64,
}

impl DelegationArgs {
    pub(crate) fn new_delegation_then_write_to_fs(&self) -> anyhow::Result<&std::path::Path> {
        let key = self.load_or_create_key()?;
        let key_did: Did = key.address().into();

        let ssk_builder = DelegationBuilder::new(key_did.to_string(), "secp256k1".to_string())
            .set_ttl(self.ttl * 1000);
        let unsigned_proof = ssk_builder.unsigned_proof();

        let sig = key.sign(&unsigned_proof)?.to_vec();
        let ssk_builder = ssk_builder.set_delegator_signature(sig);

        let ssk = ssk_builder.build()?;
        let ssk_dump = ssk.dump()?;

        let ssk_path = std::path::Path::new(&self.delegatee_key);
        ensure_parent_dir(ssk_path)?;
        std::fs::write(expand_home(ssk_path)?, ssk_dump)?;
        println!(
            "Your delegatee_key file has saved to: {}",
            ssk_path.display()
        );

        Ok(ssk_path)
    }

    pub(crate) fn load_or_create_key(&self) -> anyhow::Result<SecretKey> {
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

pub(crate) fn read_secret_key_file(path: &str) -> anyhow::Result<SecretKey> {
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
pub(crate) enum ConnectCommand {
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
pub(crate) struct ConnectUrlCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,

    pub(crate) node_url: String,

    #[arg(
        long,
        help = "Bearer token file for a remote peer that gates its handshake behind its token; not needed by default, since the external handshake (nodeDid, answerOffer) is public",
        env = "RINGS_REMOTE_API_TOKEN_FILE"
    )]
    pub(crate) remote_api_token_file: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct ConnectWithDidCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,

    pub(crate) did: String,
}

#[derive(Args, Debug)]
pub(crate) struct ConnectWithSeedCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,

    pub(crate) source: String,
}

#[derive(Args, Debug)]
pub(crate) struct ConnectOfferCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,

    /// DID of the peer this offer targets.
    pub(crate) did: String,
}

#[derive(Args, Debug)]
pub(crate) struct ConnectAnswerCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,

    /// Encoded offer produced by the peer's `connect offer`, or `-` to read it from stdin.
    pub(crate) offer: String,
}

#[derive(Args, Debug)]
pub(crate) struct ConnectAcceptCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,

    /// Encoded answer produced by the peer's `connect answer`, or `-` to read it from stdin.
    pub(crate) answer: String,
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
pub(crate) enum PeerCommand {
    #[command(about = "List peers")]
    List(PeerListCommand),
    #[command(about = "Disconnect peer")]
    Disconnect(PeerDisconnectCommand),
}

#[derive(Args, Debug)]
pub(crate) struct PeerListCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,
}

#[derive(Args, Debug)]
pub(crate) struct PeerDisconnectCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,

    pub(crate) address: String,
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
pub(crate) enum SendCommand {
    #[command(about = "Sends a namespaced message to a peer.")]
    Message(SendMessageCommand),
}

#[derive(Args, Debug)]
pub(crate) struct PubsubCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,
    pub(crate) topic: String,
}

#[derive(Args, Debug)]
pub(crate) struct SendMessageCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,
    pub(crate) to_did: String,
    pub(crate) namespace: String,
    pub(crate) data: String,
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
pub(crate) enum ServiceCommand {
    Register(ServiceRegisterCommand),
    Lookup(ServiceLookupCommand),
}

#[derive(Args, Debug)]
pub(crate) struct ServiceRegisterCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,

    pub(crate) name: String,
}

#[derive(Args, Debug)]
pub(crate) struct ServiceLookupCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,

    pub(crate) name: String,
}

#[derive(Args, Debug)]
pub(crate) struct InspectCommand {
    #[command(flatten)]
    pub(crate) client_args: ClientArgs,
}
