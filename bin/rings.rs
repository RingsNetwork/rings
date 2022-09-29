use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use clap::Parser;
use clap::Subcommand;
use rings_core::message::CallbackFn;
use rings_node::backend::Backend;
use rings_node::backend::BackendConfig;
use rings_node::cli::Client;
use rings_node::logger::LogLevel;
use rings_node::logger::Logger;
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
#[clap(about, version, author)]
struct Cli {
    #[clap(long, default_value_t = LogLevel::Info, arg_enum, env)]
    log_level: LogLevel,

    #[clap(long, short = 'c', parse(from_os_str))]
    config_file: Option<PathBuf>,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
#[clap(rename_all = "kebab-case")]
enum Command {
    #[clap()]
    Daemon(Daemon),
    #[clap(subcommand)]
    Connect(ConnectCommand),
    #[clap(subcommand)]
    Sdp(SdpCommand),
    #[clap(subcommand)]
    Peer(PeerCommand),
    #[clap(subcommand)]
    Pending(PendingCommand),
    Send(Send),
    NewSecretKey,
}

#[derive(Args, Debug)]
#[clap(about)]
struct Daemon {
    #[clap(long, short = 'b', default_value = "127.0.0.1:50000", env)]
    pub http_addr: String,

    #[clap(
        long,
        short = 's',
        default_value = "stun://stun.l.google.com:19302",
        env
    )]
    pub ice_servers: String,

    #[clap(long = "key", short = 'k', env)]
    pub ecdsa_key: SecretKey,

    #[clap(long, default_value = "20", env)]
    pub stabilize_timeout: usize,

    #[clap(long, env, help = "external ip address")]
    pub external_ip: Option<String>,

    #[clap(long, env, help = "backend service config")]
    pub backend: Option<String>,
}

#[derive(Args, Debug)]
struct ClientArgs {
    #[clap(
        long,
        short = 'u',
        default_value = "http://127.0.0.1:50000",
        help = "rings-node endpoint url.",
        env
    )]
    endpoint_url: String,

    #[clap(long = "key", short = 'k', env)]
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
#[clap(rename_all = "kebab-case")]
enum ConnectCommand {
    #[clap()]
    Node(ConnectUrlArgs),
    #[clap()]
    Did(ConnectWithDidArgs),
    #[clap()]
    Seed(ConnectWithSeedArgs),
}

#[derive(Args, Debug)]
#[clap(about = "Connect with Node url")]
struct ConnectUrlArgs {
    #[clap(flatten)]
    client_args: ClientArgs,

    #[clap()]
    node_url: String,
}

#[derive(Args, Debug)]
#[clap(about = "Connect with Did via DHT")]
struct ConnectWithDidArgs {
    #[clap(flatten)]
    client_args: ClientArgs,

    #[clap()]
    did: String,
}

#[derive(Args, Debug)]
#[clap(about = "Connect with seed from url or file")]
struct ConnectWithSeedArgs {
    #[clap(flatten)]
    client_args: ClientArgs,

    #[clap()]
    source: String,
}

#[derive(Subcommand, Debug)]
#[clap(rename_all = "kebab-case")]
enum SdpCommand {
    #[clap()]
    Offer(SdpOffer),
    #[clap()]
    Answer(SdpAnswer),
    #[clap()]
    AcceptAnswer(SdpAcceptAnswer),
}

#[derive(Args, Debug)]
#[clap()]
struct SdpOffer {
    #[clap(
        long,
        short = 's',
        default_value = "stun://stun.l.google.com:19302",
        env
    )]
    pub ice_server: String,
    #[clap(flatten)]
    client_args: ClientArgs,
}

#[derive(Args, Debug)]
struct SdpAnswer {
    #[clap(flatten)]
    client_args: ClientArgs,
    ice: String,
}

#[derive(Args, Debug)]
struct SdpAcceptAnswer {
    #[clap(flatten)]
    client_args: ClientArgs,

    #[clap(help = "transport_id of pending transport.")]
    transport_id: String,

    #[clap(help = "ice from remote.")]
    ice: String,
}

#[derive(Subcommand, Debug)]
#[clap(rename_all = "kebab-case")]
enum PeerCommand {
    List(PeerListArgs),
    Disconnect(PeerDisconnect),
}

#[derive(Args, Debug)]
struct PeerListArgs {
    #[clap(flatten)]
    client_args: ClientArgs,
}

#[derive(Args, Debug)]
struct PeerDisconnect {
    #[clap(flatten)]
    client_args: ClientArgs,
    address: String,
}

#[derive(Subcommand, Debug)]
#[clap(rename_all = "kebab-case")]
enum PendingCommand {
    List(PendingList),
    Close(PendingCloseTransport),
}

#[derive(Args, Debug)]
struct PendingList {
    #[clap(flatten)]
    client_args: ClientArgs,
}

#[derive(Args, Debug)]
struct PendingCloseTransport {
    #[clap(flatten)]
    client_args: ClientArgs,
    #[clap()]
    transport_id: String,
}

#[derive(Args, Debug)]
struct Send {
    #[clap(flatten)]
    client_args: ClientArgs,
    #[clap()]
    to_address: String,
    #[clap()]
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

    let callback: Option<CallbackFn> = {
        if let Some(backend) = backend {
            let config = BackendConfig::load(&backend).await?;
            let backend = Backend::new(config).await;
            Some(Box::new(backend))
        } else {
            None
        }
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
    Logger::init(cli.log_level.into())?;
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
        Command::Send(args) => {
            args.client_args
                .new_client()
                .await?
                .send_message(args.to_address.as_str(), args.text.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::NewSecretKey => {
            let k = SecretKey::random();
            println!("New secretKey: {}", k.to_string());
            Ok(())
        }
    }
}
