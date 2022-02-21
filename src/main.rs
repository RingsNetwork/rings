#![feature(async_closure)]
use anyhow::Result;
use bns_core::channels::default::AcChannel;
use bns_core::signing::SecretKey;
use bns_core::swarm::Swarm;
use bns_core::types::channel::Channel;
use bns_node::logger::Logger;
use bns_node::service::run_service;
use clap::Parser;

use std::sync::Arc;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
pub struct Args {
    #[clap(long, short = 'd', default_value = "127.0.0.1:50000")]
    pub http_addr: String,

    #[clap(long, short = 'v', default_value = "Info")]
    pub log_level: String,

    #[clap(long, short = 's', default_value = "stun:stun.l.google.com:19302")]
    pub stun_server: String,

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
    let swarm = Arc::new(Swarm::new(Arc::new(AcChannel::new(1)), stun.to_string()));
    let signaler = swarm.signaler();

    tokio::spawn(async move { run_service(http_addr, Arc::clone(&swarm), key).await });

    tokio::select! {
        _ = signaler.recv() => {
            println!("received done signal!");
        }
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

    run(args.http_addr, key, &args.stun_server).await;

    Ok(())
}
