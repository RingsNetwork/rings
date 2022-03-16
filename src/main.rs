#![feature(async_closure)]
use anyhow::Result;
use bns_core::dht::Chord;
use bns_core::ecc::SecretKey;
use bns_core::message::handler::MessageHandler;
use bns_core::swarm::Swarm;
use bns_node::logger::Logger;
use bns_node::service::run_service;
use bns_node::service::Stabilization;
use clap::Parser;
use futures::lock::Mutex;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
pub struct Args {
    #[clap(long, short = 'd', default_value = "127.0.0.1:50000")]
    pub http_addr: String,

    #[clap(long, short = 'v', default_value = "Info")]
    pub log_level: String,

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
    pub eth_key: String,
}

async fn run(http_addr: String, key: SecretKey, stun: &str) {
    let dht = Arc::new(Mutex::new(Chord::new(key.address().into())));
    let swarm = Arc::new(Swarm::new(stun, key));

    let listen_event = MessageHandler::new(dht.clone(), swarm.clone());
    let stabilize_event = Stabilization::new(dht.clone(), swarm.clone());
    tokio::spawn(async move { listen_event.listen().await });
    tokio::spawn(async move { stabilize_event.stabilize().await });

    let swarm_clone = swarm.clone();
    tokio::spawn(async move { run_service(http_addr, swarm_clone, key).await });

    // TODO: A shutdown handler should be here
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("quit");
        }
    };
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    Logger::init(args.log_level)?;

    let key = SecretKey::try_from(args.eth_key.as_str())?;

    run(args.http_addr, key, args.ice_server.as_str()).await;

    Ok(())
}