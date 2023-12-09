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
use rings_node::backend::native::BackendConfig;
use rings_node::backend::native::BackendContext;
use rings_node::backend::Backend;
use rings_node::logging::init_logging;
use rings_node::logging::LogLevel;
use rings_node::measure::PeriodicMeasure;
use rings_node::native::cli::Client;
use rings_node::native::config;
use rings_node::native::endpoint::run_http_api;
use rings_node::prelude::rings_core::dht::Did;
use rings_node::prelude::rings_core::ecc::SecretKey;
use rings_node::prelude::PersistenceStorage;
use rings_node::prelude::SessionSkBuilder;
use rings_node::processor::Processor;
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
        short = 'b',
        help = "Rings node listen address. If not provided, use bind_addr in config file or 127.0.0.1:50000",
        env
    )]
    pub http_addr: Option<String>,

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
        let process_config: ProcessorConfig = c.clone().try_into()?;
        let endpoint_url = self.endpoint_url.as_ref().unwrap_or(&c.endpoint_url);
        let session_sk = process_config.session_sk();
        Client::new(endpoint_url.as_str(), session_sk)
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
    #[command(about = "Sends a raw message.")]
    Raw(SendRawCommand),
    #[command(about = "Sends an HTTP request message.")]
    Http(SendHttpCommand),
    #[command(about = "Sends a simple text message.")]
    PlainText(SendPlainTextCommand),
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

    service: String,

    #[arg(default_value = "GET", long, short = 'X', help = "request method")]
    method: String,

    #[arg(default_value = "/")]
    path: String,

    #[arg(long = "header", short = 'H', action = ArgAction::Append, help = "headers append to the request")]
    headers: Vec<String>,

    #[arg(long, short = 'b', help = "set content of http body")]
    body: Option<String>,

    #[arg(long, default_value = "30000")]
    timeout: u64,

    #[arg(long = "request_id", short = 'i', help = "set request id")]
    rid: Option<String>
}

#[derive(Args, Debug)]
struct PubsubCommand {
    #[command(flatten)]
    client_args: ClientArgs,
    topic: String,
}

#[derive(Args, Debug)]
struct SendPlainTextCommand {
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

#[allow(clippy::too_many_arguments)]
async fn daemon_run(args: RunCommand) -> anyhow::Result<()> {
    let mut c = config::Config::read_fs(args.config_args.config)?;

    if let Some(ice_servers) = args.ice_servers {
        c.ice_servers = ice_servers;
    }
    if let Some(external_ip) = args.external_ip {
        c.external_ip = Some(external_ip);
    }
    if let Some(stabilize_timeout) = args.stabilize_timeout {
        c.stabilize_timeout = stabilize_timeout;
    }
    if let Some(http_addr) = args.http_addr {
        c.http_addr = http_addr;
    }

    let pc = ProcessorConfig::try_from(c.clone())?;
    let bc = BackendConfig::from(c.clone());

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

    let processor = Arc::new(
        ProcessorBuilder::from_config(&pc)?
            .storage(per_data_storage)
            .measure(measure)
            .build()?,
    );
    println!("Did: {}", processor.swarm.did());
    let backend_context = BackendContext::new(bc).await?;
    let backend_service_names = backend_context.service_names();
    let provider = Arc::new(Provider::from_processor(processor.clone()));
    let backend = Arc::new(Backend::new(provider, Box::new(backend_context)));
    processor.swarm.set_callback(backend).unwrap();

    let processor_clone = processor.clone();
    let _ = futures::join!(
        processor.listen(),
        service_loop_register(&processor, backend_service_names),
        run_http_api(c.http_addr, processor_clone),
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
    init_logging(cli.log_level);

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
                    args.service.as_str(),
                    http::Method::from_str(args.method.to_uppercase().as_str())?,
                    args.path.as_str(),
                    args.headers
                        .iter()
                        .map(|x| x.split(':').collect::<Vec<&str>>())
                        .map(|b| {
                            (
                                b[0].trim_start_matches(' ')
                                    .trim_end_matches(' ')
                                    .to_string(),
                                b[1].trim_start_matches(' ')
                                    .trim_end_matches(' ')
                                    .to_string(),
                            )
                        })
                        .collect::<Vec<(_, _)>>(),
                    args.body.map(|x| x.as_bytes().to_vec()),
		    args.rid
                )
                .await?
                .display();
            Ok(())
        }
        Command::Send(SendCommand::PlainText(args)) => {
            args.client_args
                .new_client()
                .await?
                .send_plain_text_message(args.to_did.as_str(), args.text.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Send(SendCommand::Custom(args)) => {
            args.client_args
                .new_client()
                .await?
                .send_custom_message(args.to_did.as_str(), args.data.as_str())
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
            println!("Your config file has saved to: {}", p);
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
