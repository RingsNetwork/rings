#![feature(async_closure)]
use anyhow::Result;
use bns_core::dht::Chord;
use bns_core::ecc::SecretKey;
use bns_core::message::handler::MessageHandler;
use bns_core::swarm::Swarm;
use bns_node::grpc::{
    grpc_client::GrpcClient,
    request::{
        ConnectWithHandshakeInfo, ConnectWithUrl, Disconnect, GenerateHandshakeInfo, ListPeers,
        SendTo,
    },
    response::Peer,
};
use bns_node::logger::Logger;
use bns_node::service::run_service;
use clap::{ArgEnum, Args, Parser, Subcommand};
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

#[derive(ArgEnum, Debug, Clone)]
#[clap(rename_all = "kebab-case")]
enum LogLevel {
    Off,
    Debug,
    Info,
    Warn,
    Error,
    Trace,
}

impl From<LogLevel> for log::LevelFilter {
    fn from(val: LogLevel) -> Self {
        match val {
            LogLevel::Off => log::LevelFilter::Off,
            LogLevel::Debug => log::LevelFilter::Debug,
            LogLevel::Info => log::LevelFilter::Info,
            LogLevel::Warn => log::LevelFilter::Warn,
            LogLevel::Error => log::LevelFilter::Error,
            LogLevel::Trace => log::LevelFilter::Trace,
        }
    }
}

#[derive(Subcommand, Debug)]
#[clap(rename_all = "kebab-case")]
enum Command {
    #[clap(about = "daemon")]
    Daemon(Daemon),
    Connect(ConnectArgs),
    #[clap(subcommand)]
    Sdp(SdpCommand),
    #[clap(subcommand)]
    Peer(PeerCommand),
    NewSecretKey,
    Send(Send),
}

#[derive(Args, Debug)]
#[clap(about)]
struct Daemon {
    #[clap(long, short = 'b', default_value = "127.0.0.1:50000", env)]
    pub http_addr: String,

    #[clap(long, short = 's', default_value = "stun://stun.l.google.com:19302")]
    pub ice_server: String,

    #[clap(
        long = "eth",
        short = 'e',
        default_value = "http://127.0.0.1:8545",
        env
    )]
    pub eth_endpoint: String,

    #[clap(long = "key", short = 'k', env)]
    pub eth_key: SecretKey,

    #[clap(long, short = 'd', help = "Run in daemon mode.")]
    pub daemon: bool,
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
    fn new_client(&self) -> anyhow::Result<Client> {
        Client::new(self.endpoint_url.as_str())
    }
}

#[derive(Args, Debug)]
#[clap(about)]
struct ConnectArgs {
    #[clap(flatten)]
    client_args: ClientArgs,

    #[clap(help = "Connect peer with peer_url")]
    peer_url: String,
}

#[derive(Subcommand, Debug)]
#[clap(rename_all = "kebab-case")]
enum SdpCommand {
    #[clap(about = "Generate transport and wait for connection.")]
    Gen(SdpGen),
    #[clap(about = "Connect to a peer.")]
    Connect(SdpConnect),
}

#[derive(Args, Debug)]
#[clap(about)]
struct SdpGen {
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
struct SdpConnect {
    #[clap(flatten)]
    client_args: ClientArgs,

    #[clap(about)]
    handshake_info: String,

    #[clap(
        short = 'f',
        long = "force",
        parse(from_flag),
        help = "Force start a new transport to connect peer."
    )]
    force_new_transport: bool,
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
    #[clap(parse(from_flag), short = 'a')]
    all: bool,
}

#[derive(Args, Debug)]
struct PeerDisconnect {
    #[clap(flatten)]
    client_args: ClientArgs,

    #[clap(about)]
    address: String,
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

async fn daemon_run(http_addr: String, key: &SecretKey, stun: &str) -> anyhow::Result<()> {
    // TODO support run daemonize
    let dht = Arc::new(Mutex::new(Chord::new(key.address().into())));
    let swarm = Arc::new(Swarm::new(stun, key.to_owned()));

    let listen_event = MessageHandler::new(dht.clone(), swarm.clone());
    let swarm_clone = swarm.clone();
    let key = key.to_owned();

    let (_, _) = futures::join!(async { listen_event.listen().await }, async {
        run_service(http_addr.to_owned(), swarm_clone, key).await
    },);
    Ok(())
}

#[derive(Clone, Debug)]
struct Client {
    client: GrpcClient<tonic::transport::Channel>,
}

impl Client {
    fn new(endpoint: &str) -> anyhow::Result<Self> {
        log::debug!("endpoint_url: {}", endpoint);
        let channel =
            tonic::transport::Channel::builder(endpoint.parse::<tonic::transport::Uri>()?);
        let client = GrpcClient::new(channel.connect_lazy())
            .send_gzip()
            .accept_gzip();
        Ok(Self { client })
    }

    async fn connect_with_url(&mut self, url: &str) -> anyhow::Result<()> {
        let resp = self.client.grpc(ConnectWithUrl::new(url)).await?;
        log::debug!("resp: {:?}", resp.get_ref());
        let sdp = resp.get_ref().as_text_result()?;
        println!("Succeed, Your sdp: {}", sdp);
        Ok(())
    }

    async fn connect_with_handshake_info(
        &mut self,
        handshake_info: &str,
        new_transport: bool,
    ) -> anyhow::Result<()> {
        let resp = self
            .client
            .grpc(ConnectWithHandshakeInfo::new(handshake_info, new_transport))
            .await?;
        let result = resp.get_ref().as_text_result()?;
        println!("Succussful, {}", result);
        Ok(())
    }

    async fn generate_handshake_info(&mut self) -> anyhow::Result<()> {
        let resp = self.client.grpc(GenerateHandshakeInfo::default()).await?;
        let hs_info = resp.get_ref().as_text_result()?;
        println!("Succussful, Your new handshake info: {}", hs_info);
        Ok(())
    }

    async fn list_peers(&mut self, all: bool) -> anyhow::Result<()> {
        let resp = self.client.grpc(ListPeers::new(all)).await?;
        let resp: Vec<Peer> = resp.get_ref().as_json_result()?;
        println!("Succussful");
        println!("Address, TransportId");
        resp.iter().for_each(|item| {
            println!("{}, {}", item.address, item.transport_id);
        });
        Ok(())
    }

    async fn send_to(&mut self, to_address: &str, text: &str) -> anyhow::Result<()> {
        let resp = self.client.grpc(SendTo::new(to_address, text)).await?;
        let resp = resp.get_ref().as_text_result()?;
        println!("Succussful, {}", resp);
        Ok(())
    }

    async fn disconnect(&mut self, address: &str) -> anyhow::Result<()> {
        self.client
            .grpc(Disconnect {
                address: address.to_owned(),
            })
            .await?;
        println!("Done.");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let cli = Cli::parse();
    Logger::init(cli.log_level.into())?;

    if let Err(e) = match cli.command {
        Command::Daemon(args) => {
            daemon_run(args.http_addr, &args.eth_key, args.ice_server.as_str()).await
        }
        Command::Connect(args) => {
            let mut client = args.client_args.new_client()?;
            client.connect_with_url(args.peer_url.as_str()).await?;
            Ok(())
        }
        Command::Sdp(SdpCommand::Gen(args)) => {
            let mut client = args.client_args.new_client()?;
            client.generate_handshake_info().await
        }
        Command::Sdp(SdpCommand::Connect(args)) => {
            let mut client = args.client_args.new_client()?;
            client
                .connect_with_handshake_info(args.handshake_info.as_str(), args.force_new_transport)
                .await?;
            Ok(())
        }
        Command::Peer(PeerCommand::List(args)) => {
            let mut client = args.client_args.new_client()?;
            client.list_peers(args.all).await
        }
        Command::Peer(PeerCommand::Disconnect(args)) => {
            let mut client = args.client_args.new_client()?;
            client.disconnect(args.address.as_str()).await
        }
        Command::Send(args) => {
            let mut client = args.client_args.new_client()?;
            client
                .send_to(args.to_address.as_str(), args.text.as_str())
                .await
        }
        Command::NewSecretKey => {
            println!("New secretKey: {}", SecretKey::random().to_string());
            Ok(())
        }
    } {
        //log::error!("{}", e);
        return Err(e);
    }
    Ok(())
}
