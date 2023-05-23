use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use clap::ArgAction;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use futures::StreamExt;
use futures_timer::Delay;
use rings_node::backend::service::Backend;
use rings_node::cli::Client;
use rings_node::config;
use rings_node::endpoint::run_http_api;
use rings_node::logging::node::init_logging;
use rings_node::logging::node::LogLevel;
use rings_node::measure::PeriodicMeasure;
use rings_node::prelude::http;
use rings_node::prelude::rings_core::dht::Did;
use rings_node::prelude::rings_core::dht::Stabilization;
use rings_node::prelude::rings_core::ecc::SecretKey;
use rings_node::prelude::PersistenceStorage;
use rings_node::prelude::SwarmBuilder;
use rings_node::processor::Processor;
use tokio::io;
use tokio::io::AsyncBufReadExt;

#[derive(Parser, Debug)]
#[command(about, version, author)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[arg(long, default_value_t = LogLevel::Info, value_enum, env)]
    log_level: LogLevel,
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum Command {
    #[command(about = "Initializes a node with the given configuration.")]
    Init(InitCommand),
    #[command(about = "Starts a long-running node daemon.")]
    Run(RunCommand),
    #[command(about = "Provides chat room-like functionality on the Rings Network.")]
    Pubsub(PubsubCommand),
    #[command(about = "Connects to a remote peer.", subcommand)]
    Connect(ConnectCommand),
    #[command(about = "Manages peers on the network.", subcommand)]
    Peer(PeerCommand),
    #[command(about = "Manages pending peers on the network.", subcommand)]
    Pending(PendingCommand),
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
    #[arg(
        long,
        default_value = "~/.rings/config.yaml",
        help = "The location of config file"
    )]
    pub location: String,

    #[arg(
        long = "key",
        short = 'k',
        help = "Your ecdsa_key. If not provided, a new key will be generated"
    )]
    pub ecdsa_key: Option<SecretKey>,
}

#[derive(Args, Debug)]
struct RunCommand {
    #[arg(
        long,
        short = 'b',
        help = "Rings node listen address. If not provided, use bind_addr in config file or 127.0.0.1:50000",
        env
    )]
    pub http_addr: Option<String>,

    #[arg(
        long,
        short = 's',
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
        help = "Stabilize service timeout. If not provided, use stabilize_timeout in config file or 3",
        env
    )]
    pub stabilize_timeout: Option<usize>,

    #[arg(long, help = "external ip address", env)]
    pub external_ip: Option<String>,

    #[arg(
        long,
        help = "Storage files location. If not provided, use storage.path in config file or ~/.local/share/rings",
        env
    )]
    pub storage_path: Option<String>,

    #[arg(
        long,
        default_value = "200000000",
        help = "Storage capcity. If not provider, use storage.capacity in config file or 200000000",
        env
    )]
    pub storage_capacity: Option<usize>,

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
        let c = config::Config::read_fs(self.config_args.config.as_str())?;

        let endpoint_url = self.endpoint_url.as_ref().unwrap_or(&c.endpoint_url);
        let ecdsa_key = self.ecdsa_key.unwrap_or(c.ecdsa_key);

        Client::new(
            endpoint_url.as_str(),
            Processor::generate_signature(&ecdsa_key).as_str(),
        )
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
enum PendingCommand {
    #[command(about = "List pending peers")]
    List(PendingListCommand),
    #[command(about = "Close pending peer")]
    Close(PendingCloseTransportCommand),
}

#[derive(Args, Debug)]
struct PendingListCommand {
    #[command(flatten)]
    client_args: ClientArgs,
}

#[derive(Args, Debug)]
struct PendingCloseTransportCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    transport_id: String,
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum SendCommand {
    #[command(about = "Sends a raw message.")]
    Raw(SendRawCommand),
    #[command(about = "Sends an HTTP request message.")]
    Http(SendHttpCommand),
    #[command(about = "Sends a simple text message.")]
    SimpleText(SendSimpleTextCommand),
    #[command(about = "Sends a custom message.")]
    Custom(SendCustomMessageCommand),
}

#[derive(Args, Debug)]
struct SendRawCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    to_did: String,

    text: String,
}

#[derive(Args, Debug)]
struct SendHttpCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    to_did: String,

    name: String,

    #[arg(default_value = "get", long, short = 'X', help = "request method")]
    method: String,

    #[arg(default_value = "/")]
    path: String,

    #[arg(long = "header", short = 'H', action = ArgAction::Append, help = "headers append to the request")]
    headers: Vec<String>,

    #[arg(long, short = 'b', help = "set content of http body")]
    body: Option<String>,

    #[arg(long, default_value = "30000")]
    timeout: u64,
}

#[derive(Args, Debug)]
struct PubsubCommand {
    #[command(flatten)]
    client_args: ClientArgs,
    topic: String,
}

#[derive(Args, Debug)]
struct SendSimpleTextCommand {
    #[command(flatten)]
    client_args: ClientArgs,
    to_did: String,
    text: String,
}

#[derive(Args, Debug)]
struct SendCustomMessageCommand {
    #[command(flatten)]
    client_args: ClientArgs,
    to_did: String,
    message_type: u16,
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

fn get_value<V>(value: Option<V>, default_value: V) -> V {
    value.unwrap_or(default_value)
}

#[allow(clippy::too_many_arguments)]
async fn daemon_run(args: RunCommand) -> anyhow::Result<()> {
    let c = config::Config::read_fs(args.config_args.config)?;

    let key = get_value(args.ecdsa_key, c.ecdsa_key);
    let did: Did = key.address().into();
    println!("Did: {}", did);

    let (data_storage, measure_storage) = if let Some(storage_path) = args.storage_path {
        let storage_path = Path::new(&storage_path);
        let data_path = storage_path.join("data");
        let measure_path = storage_path.join("measure");
        let capacity = args
            .storage_capacity
            .unwrap_or(config::DEFAULT_STORAGE_CAPACITY);
        (
            config::StorageConfig::new(data_path.to_str().unwrap(), capacity),
            config::StorageConfig::new(measure_path.to_str().unwrap(), capacity),
        )
    } else {
        (c.data_storage, c.measure_storage)
    };

    let per_data_storage =
        PersistenceStorage::new_with_cap_and_path(data_storage.capacity, data_storage.path).await?;
    let per_measure_storage =
        PersistenceStorage::new_with_cap_and_path(measure_storage.capacity, measure_storage.path)
            .await?;

    let measure = PeriodicMeasure::new(per_measure_storage);

    let stuns = get_value(args.ice_servers, c.ice_servers);

    let external_ip = args.external_ip.map(Some).unwrap_or(c.external_ip);

    let swarm = Arc::new(
        SwarmBuilder::new(stuns.as_str(), per_data_storage)
            .key(key)
            .external_address(external_ip)
            .measure(Box::new(measure))
            .build()?,
    );

    let (sender, receiver) = tokio::sync::broadcast::channel(1024);
    let backend_config = c.backend.into();
    let backend = Backend::new(backend_config, sender);
    let backend_service_names = backend.service_names();

    let stabilize_timeout = get_value(args.stabilize_timeout, c.stabilize_timeout);
    let stabilize = Arc::new(Stabilization::new(swarm.clone(), stabilize_timeout));

    let processor = Arc::new(Processor::from((swarm, stabilize)));
    let processor_clone = processor.clone();

    let pubkey = Arc::new(key.pubkey());
    println!("Signautre: {}", Processor::generate_signature(&key));

    let bind_addr = get_value(args.http_addr, c.http_addr);

    let _ = futures::join!(
        processor.listen(Some(Box::new(backend))),
        service_loop_register(&processor, backend_service_names),
        run_http_api(bind_addr, processor_clone, pubkey, receiver,),
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
                let line = line?.expect("stdin closed");
                client.publish_message_to_topic(&topic, &line).await?;
            }
            msg = stream.next() => {
                let msg = msg.expect("sub stream closed");
                println!("{}", msg);
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();

    let cli = Cli::parse();
    init_logging(cli.log_level.into());

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
        Command::Pending(PendingCommand::List(args)) => {
            args.client_args
                .new_client()
                .await?
                .list_pendings()
                .await?
                .display();
            Ok(())
        }
        Command::Pending(PendingCommand::Close(args)) => {
            args.client_args
                .new_client()
                .await?
                .close_pending_transport(args.transport_id.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Send(SendCommand::Raw(args)) => {
            args.client_args
                .new_client()
                .await?
                .send_message(args.to_did.as_str(), args.text.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Send(SendCommand::Http(args)) => {
            args.client_args
                .new_client()
                .await?
                .send_http_request_message(
                    args.to_did.as_str(),
                    args.name.as_str(),
                    http::Method::from_str(args.method.as_str())?,
                    args.path.as_str(),
                    args.timeout.into(),
                    &args
                        .headers
                        .iter()
                        .map(|x| x.split(':').collect::<Vec<&str>>())
                        .map(|b| {
                            (
                                b[0].trim_start_matches(' ').trim_end_matches(' '),
                                b[1].trim_start_matches(' ').trim_end_matches(' '),
                            )
                        })
                        .collect::<Vec<(_, _)>>(),
                    args.body,
                )
                .await?
                .display();
            Ok(())
        }
        Command::Send(SendCommand::SimpleText(args)) => {
            args.client_args
                .new_client()
                .await?
                .send_simple_text_message(args.to_did.as_str(), args.text.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Send(SendCommand::Custom(args)) => {
            args.client_args
                .new_client()
                .await?
                .send_custom_message(args.to_did.as_str(), args.message_type, args.data.as_str())
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
            let config = if let Some(key) = args.ecdsa_key {
                config::Config::new_with_key(key)
            } else {
                config::Config::default()
            };
            let p = config.write_fs(args.location.as_str())?;
            println!("Your config file has saved to: {}", p);
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

async fn register_services(processor: &Processor, names: Vec<String>) -> anyhow::Result<()> {
    let jobs = names.iter().map(|n| processor.register_service(n));
    let results = futures::future::join_all(jobs).await;

    for r in results {
        if let Err(e) = r {
            tracing::error!("register service error: {}", e);
        }
    }

    Ok(())
}

async fn service_loop_register(processor: &Processor, names: Vec<String>) {
    loop {
        let timeout = Delay::new(Duration::from_secs(30)).fuse();
        pin_mut!(timeout);
        select! {
            _ = timeout => register_services(processor, names.clone()).await.unwrap_or_else(|e| eprintln!("Error: {}", e)),
        }
    }
}
