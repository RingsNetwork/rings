#![feature(async_closure)]
use anyhow::Result;
use bns_core::dht::Chord;
use bns_core::ecc::SecretKey;
use bns_core::message::handler::MessageHandler;
use bns_core::swarm::Swarm;
use bns_core::types::message::MessageListener;
use bns_node::logger::LogLevel;
use bns_node::logger::Logger;
use bns_node::service::response::Peer;
use bns_node::service::{request::Method, response::TransportAndIce, run_service};
use clap::{Args, Parser, Subcommand};
use futures::lock::Mutex;
use jsonrpc_core::Params;
use jsonrpc_core::Value;
use jsonrpc_core_client::RawClient;
use serde_json::json;
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
    Send(Send),
    NewSecretKey,
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
        Ok(Client {
            client: jsonrpc_core_client::transports::http::connect(self.endpoint_url.as_str())
                .await
                .map_err(|e| anyhow::anyhow!("jsonrpc client error: {}.", e))?,
        })
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

    let (_, _) = futures::join!(
        Arc::new(listen_event).listen(),
        run_service(http_addr.to_owned(), swarm_clone, key)
    );

    Ok(())
}

#[derive(Clone)]
struct Client {
    client: RawClient,
}

impl Client {
    async fn connect_peer_via_http(&mut self, http_url: &str) -> anyhow::Result<()> {
        let resp = self
            .client
            .call_method(
                Method::ConnectPeerViaHttp.as_str(),
                Params::Array(vec![Value::String(http_url.to_owned())]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        log::debug!("resp: {:?}", resp);
        let transport_id = resp
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Unexpect response"))?;
        println!("Succeed, Your transport_id: {}", transport_id);
        Ok(())
    }

    async fn connect_peer_via_ice(&mut self, ice_info: &str) -> anyhow::Result<()> {
        let resp = self
            .client
            .call_method(
                Method::ConnectPeerViaIce.as_str(),
                Params::Array(vec![Value::String(ice_info.to_owned())]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let info: TransportAndIce =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;

        println!(
            "Successful!\ntransport_id: {}\nice: {}",
            info.transport_id, info.ice,
        );
        Ok(())
    }

    async fn create_offer(&mut self) -> anyhow::Result<()> {
        let resp = self
            .client
            .call_method(Method::CreateOffer.as_str(), Params::Array(vec![]))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let info: TransportAndIce =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;
        println!(
            "Successful!\ntransport_id: {}\nice: {}",
            info.transport_id, info.ice
        );
        Ok(())
    }

    async fn accept_answer(&mut self, transport_id: &str, ice: &str) -> anyhow::Result<()> {
        let resp = self
            .client
            .call_method(
                Method::AcceptAnswer.as_str(),
                Params::Array(vec![json!(transport_id), json!(ice)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let peer: Peer = serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;
        println!("Successful, transport_id: {}", peer.transport_id);
        Ok(())
    }

    async fn list_peers(&mut self, _all: bool) -> anyhow::Result<()> {
        let resp = self
            .client
            .call_method(Method::ListPeers.as_str(), Params::Array(vec![]))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let peers: Vec<Peer> =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;
        println!("Successful");
        println!("Address, TransportId");
        peers.iter().for_each(|item| {
            println!("{}, {}", item.address, item.transport_id);
        });
        Ok(())
    }

    async fn disconnect(&mut self, address: &str) -> anyhow::Result<()> {
        self.client
            .call_method(
                Method::Disconnect.as_str(),
                Params::Array(vec![json!(address)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
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
        Command::Run(args) => {
            daemon_run(args.http_addr, &args.eth_key, args.ice_server.as_str()).await
        }
        Command::Connect(args) => {
            args.client_args
                .new_client()
                .await?
                .connect_peer_via_http(args.peer_url.as_str())
                .await?;
            Ok(())
        }
        Command::Sdp(SdpCommand::Offer(args)) => {
            args.client_args.new_client().await?.create_offer().await?;
            Ok(())
        }
        Command::Sdp(SdpCommand::Answer(args)) => {
            args.client_args
                .new_client()
                .await?
                .connect_peer_via_ice(args.ice.as_str())
                .await?;
            Ok(())
        }
        Command::Sdp(SdpCommand::AcceptAnswer(args)) => {
            args.client_args
                .new_client()
                .await?
                .accept_answer(args.transport_id.as_str(), args.ice.as_str())
                .await?;
            Ok(())
        }
        Command::Peer(PeerCommand::List(args)) => {
            args.client_args
                .new_client()
                .await?
                .list_peers(args.all)
                .await?;
            Ok(())
        }
        Command::Peer(PeerCommand::Disconnect(args)) => {
            args.client_args
                .new_client()
                .await?
                .disconnect(args.address.as_str())
                .await?;
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
