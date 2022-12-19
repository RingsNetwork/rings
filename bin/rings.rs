use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use clap::ArgAction;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use rings_core::message::CallbackFn;
use rings_node::backend::Backend;
use rings_node::backend::BackendConfig;
use rings_node::cli::Client;
use rings_node::logging::node::init_logging;
use rings_node::logging::node::LogLevel;
use rings_node::prelude::rings_core::dht::Stabilization;
use rings_node::prelude::rings_core::dht::TStabilize;
use rings_node::prelude::rings_core::ecc::SecretKey;
use rings_node::prelude::rings_core::storage::PersistenceStorage;
use rings_node::prelude::rings_core::swarm::SwarmBuilder;
use rings_node::prelude::rings_core::types::message::MessageListener;
use rings_node::processor::Processor;
use rings_node::service::run_service;
use rings_node::util;
use rings_node::util::loader::ResourceLoader;

#[derive(Parser, Debug)]
#[command(about, version, author)]
struct Cli {
    #[arg(long, default_value_t = LogLevel::Info, value_enum, env)]
    log_level: LogLevel,

    #[arg(long, short = 'c', value_parser)]
    config_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum Command {
    NewSecretKey,
    #[command(about = "Start a long-running node daemon")]
    Daemon(DaemonCommand),
    #[command(subcommand)]
    Connect(ConnectCommand),
    #[command(subcommand)]
    Sdp(SdpCommand),
    #[command(subcommand)]
    Peer(PeerCommand),
    #[command(subcommand)]
    Pending(PendingCommand),
    #[command(subcommand)]
    Send(SendCommand),
    #[command(about = "Like a chat room but on the Rings Network")]
    Pubsub(PubsubCommand),
}

#[derive(Args, Debug)]
struct DaemonCommand {
    #[arg(long, short = 'b', default_value = "127.0.0.1:50000", env)]
    pub http_addr: String,

    #[arg(
        long,
        short = 's',
        default_value = "stun://stun.l.google.com:19302",
        env
    )]
    pub ice_servers: String,

    #[arg(long = "key", short = 'k', env)]
    pub ecdsa_key: SecretKey,

    #[arg(long, default_value = "20", env)]
    pub stabilize_timeout: usize,

    #[arg(long, env, help = "external ip address")]
    pub external_ip: Option<String>,

    #[arg(long, env, help = "backend service config")]
    pub backend: Option<String>,
}

#[derive(Args, Debug)]
struct ClientArgs {
    #[arg(
        long,
        short = 'u',
        default_value = "http://127.0.0.1:50000",
        help = "rings-node endpoint url.",
        env
    )]
    endpoint_url: String,

    #[arg(long = "key", short = 'k', env)]
    pub ecdsa_key: SecretKey,
}

impl ClientArgs {
    async fn new_client(&self) -> anyhow::Result<Client> {
        Client::new(
            self.endpoint_url.as_str(),
            Processor::generate_signature(&self.ecdsa_key).as_str(),
        )
        .await
    }
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum ConnectCommand {
    #[command(about = "Connect with Node url")]
    Node(ConnectUrlCommand),
    #[command(about = "Connect with Did via DHT")]
    Did(ConnectWithDidCommand),
    #[command(about = "Connect with seed from url or file")]
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
enum SdpCommand {
    Offer(SdpOfferCommand),
    Answer(SdpAnswerCommand),
    AcceptAnswer(SdpAcceptAnswerCommand),
}

#[derive(Args, Debug)]
struct SdpOfferCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    #[arg(
        long,
        short = 's',
        default_value = "stun://stun.l.google.com:19302",
        env
    )]
    pub ice_server: String,
}

#[derive(Args, Debug)]
struct SdpAnswerCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    ice: String,
}

#[derive(Args, Debug)]
struct SdpAcceptAnswerCommand {
    #[command(flatten)]
    client_args: ClientArgs,

    #[arg(help = "transport_id of pending transport.")]
    transport_id: String,

    #[arg(help = "ice from remote.")]
    ice: String,
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum PeerCommand {
    List(PeerListCommand),
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
    List(PendingListCommand),
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
    Raw(SendRawCommand),
    Http(SendHttpCommand),
    SimpleText(SendSimpleTextCommand),
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

    #[arg(default_value = "get")]
    method: String,

    #[arg(default_value = "/")]
    path: String,

    #[arg(long = "header", short = 'H', action = ArgAction::Append, help = "headers append to the request")]
    headers: Vec<String>,

    #[arg(long, help = "set content of http body")]
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

async fn daemon_run(
    http_addr: String,
    key: SecretKey,
    stuns: &str,
    stabilize_timeout: usize,
    external_ip: Option<String>,
    backend: Option<String>,
) -> anyhow::Result<()> {
    let storage = PersistenceStorage::new().await?;

    let swarm = Arc::new(
        SwarmBuilder::new(stuns, storage)
            .key(key)
            .external_address(external_ip)
            .build()?,
    );

    let callback: Option<CallbackFn> = if let Some(backend) = backend {
        let config = BackendConfig::load(&backend).await?;
        let backend = Backend::new(config);
        Some(Box::new(backend))
    } else {
        None
    };

    let listen_event = Arc::new(swarm.create_message_handler(callback, None));
    let stabilize = Arc::new(Stabilization::new(swarm.clone(), stabilize_timeout));
    let swarm_clone = swarm.clone();
    let pubkey = Arc::new(key.pubkey());

    let (_, _, _) = futures::join!(
        listen_event.listen(),
        run_service(http_addr.to_owned(), swarm_clone, stabilize.clone(), pubkey),
        stabilize.wait(),
    );

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();

    util::load_config();

    let cli = Cli::parse();
    init_logging(cli.log_level.into());
    // if config file was set, it should override existing .env
    if let Some(conf) = cli.config_file {
        dotenv::from_path(std::path::Path::new(&conf)).ok();
    }

    match cli.command {
        Command::Daemon(args) => {
            daemon_run(
                args.http_addr,
                args.ecdsa_key,
                args.ice_servers.as_str(),
                args.stabilize_timeout,
                args.external_ip,
                args.backend,
            )
            .await
        }
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
        Command::Sdp(SdpCommand::Offer(args)) => {
            args.client_args
                .new_client()
                .await?
                .create_offer()
                .await?
                .display();
            Ok(())
        }
        Command::Sdp(SdpCommand::Answer(args)) => {
            args.client_args
                .new_client()
                .await?
                .answer_offer(args.ice.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Sdp(SdpCommand::AcceptAnswer(args)) => {
            args.client_args
                .new_client()
                .await?
                .accept_answer(args.transport_id.as_str(), args.ice.as_str())
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
        Command::NewSecretKey => {
            let k = SecretKey::random();
            println!("New secretKey: {}", k.to_string());
            Ok(())
        }
        Command::Pubsub(args) => {
            dbg!(args);
            Ok(())
        }
    }
}
