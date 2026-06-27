use std::path::Path;
use std::sync::Arc;

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

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum Command {
    #[command(about = "Initializes a node with the given configuration.")]
    Init(InitCommand),
    #[command(about = "Creates a new session secret key.")]
    NewSession(NewSessionCommand),
    #[command(about = "Starts a long-running node daemon.")]
    Run(RunCommand),
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
        long,
        default_value = "2592000",
        help = "The ttl of session file in seconds"
    )]
    pub ttl: u64,
}

impl SessionArgs {
    fn new_session_then_write_to_fs(&self) -> anyhow::Result<&std::path::Path> {
        let key = self.ecdsa_key.unwrap_or_else(|| {
            let rand_key = SecretKey::random();
            println!("Your random ecdsa key is: {}", rand_key.to_string());
            rand_key
        });
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

    let pc = ProcessorConfig::try_from(c.clone())?;

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
    rings_node::extension::protocols::relay::RelayHandle::install(&provider.extensions())?;
    // SNARK is a namespaced protocol now; register it so the daemon can prove/verify.
    #[cfg(feature = "snark")]
    rings_node::extension::snark::SNARKBehaviour::default().register(provider.as_ref())?;
    // The Backend decodes inbound custom messages as namespaced envelopes and routes
    // them to the protocol registry.
    let backend = Arc::new(Backend::new(provider));
    processor.swarm.set_callback(backend)?;

    let processor_clone1 = processor.clone();
    let processor_clone2 = processor.clone();
    let _ = futures::join!(
        processor.listen(),
        run_internal_api(c.internal_api_port, processor_clone2),
        run_external_api(c.external_api_addr, processor_clone1),
    );

    Ok(())
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
        Command::Run(args) => daemon_run(args).await,
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
