#![feature(async_closure)]
use bns_core::dht::PeerRing;
use bns_core::ecc::SecretKey;
use bns_core::message::MessageHandler;
use bns_core::session::SessionManager;
use bns_core::swarm::Swarm;
use bns_core::types::message::MessageListener;
use bns_node::cli::Client;
use bns_node::logger::LogLevel;
use bns_node::logger::Logger;
use bns_node::service::run_service;
use clap::{Args, Parser, Subcommand};
use futures::lock::Mutex;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Cli {
    #[clap(long, short = 'v', default_value_t = LogLevel::Info, arg_enum)]
    log_level: LogLevel,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
#[clap(rename_all = "kebab-case")]
enum Command {
    #[clap(about = "daemon")]
    Run(Daemon),
    Connect(ConnectArgs),
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

    #[clap(long, short = 's', default_value = "stun://stun.l.google.com:19302")]
    pub ice_servers: String,

    #[clap(
        long = "eth",
        short = 'e',
        default_value = "http://127.0.0.1:8545",
        env
    )]
    pub eth_endpoint: String,

    #[clap(long = "key", short = 'k', env)]
    pub eth_key: SecretKey,
}

#[derive(Args, Debug)]
struct ClientArgs {
    #[clap(
        long,
        short = 'u',
        default_value = "http://127.0.0.1:50000",
        help = "bns-node endpoint url."
    )]
    endpoint_url: String,
}

impl ClientArgs {
    async fn new_client(&self) -> anyhow::Result<Client> {
        Client::new(self.endpoint_url.as_str()).await
    }
}

#[derive(Args, Debug)]
#[clap(about)]
struct ConnectArgs {
    #[clap(flatten)]
    client_args: ClientArgs,

    #[clap(help = "Connect peer via peer_url")]
    peer_url: String,
}

#[derive(Subcommand, Debug)]
#[clap(rename_all = "kebab-case")]
enum SdpCommand {
    #[clap()]
    Offer(SdpOffer),
    #[clap(about)]
    Answer(SdpAnswer),
    #[clap(about)]
    AcceptAnswer(SdpAcceptAnswer),
}

#[derive(Args, Debug)]
#[clap(about)]
struct SdpOffer {
    #[clap(long = "key", short = 'k', env)]
    pub eth_key: SecretKey,
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

    #[clap(about)]
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

    #[clap(about)]
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
    #[clap(short = 't')]
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

async fn daemon_run(http_addr: String, key: &SecretKey, stuns: &str) -> anyhow::Result<()> {
    // TODO support run daemonize
    let dht = Arc::new(Mutex::new(PeerRing::new(key.address().into())));
    let (auth, key) =
        SessionManager::gen_unsign_info(key.address(), Some(bns_core::session::Ttl::Never))?;
    let sig = key.sign(&auth.to_string()?).to_vec();
    let session = SessionManager::new(&sig, &auth, &key);
    let swarm = Arc::new(Swarm::new(stuns, key.address(), session.clone()));

    let listen_event = MessageHandler::new(dht.clone(), swarm.clone());
    let swarm_clone = swarm.clone();

    let (_, _) = futures::join!(
        Arc::new(listen_event).listen(),
        run_service(http_addr.to_owned(), swarm_clone)
    );

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    let cli = Cli::parse();
    Logger::init(cli.log_level.into())?;

    if let Err(e) = match cli.command {
        Command::Run(args) => {
            daemon_run(args.http_addr, &args.eth_key, args.ice_servers.as_str()).await
        }
        Command::Connect(args) => {
            args.client_args
                .new_client()
                .await?
                .connect_peer_via_http(args.peer_url.as_str())
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
                .connect_peer_via_ice(args.ice.as_str())
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
            args.client_args.new_client().await?.list_peers().await?;
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
        Command::Send(_args) => {
            println!("send command does not support.");
            Ok(())
        }
        Command::NewSecretKey => {
            println!("New secretKey: {}", SecretKey::random().to_string());
            Ok(())
        }
    } {
        return Err(e);
    }
    Ok(())
}
