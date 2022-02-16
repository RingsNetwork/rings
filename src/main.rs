#![feature(async_closure)]
use anyhow::Result;
use bns_core::channels::default::TkChannel;
use bns_core::swarm::Swarm;
use bns_core::types::channel::Channel;
//use bns_node::config::read_config;
use bns_node::discoveries::http::discoveries_services;
use bns_node::logger::Logger;
use clap::Parser;
use hyper::service::{make_service_fn, service_fn};
use hyper::Server;
use std::net::SocketAddr;
use secp256k1::SecretKey;
use std::str::FromStr;


#[derive(Parser, Debug)]
#[clap(about, version, author)]
pub struct Args {
    #[clap(long, short = 'd', default_value = "127.0.0.1:50000")]
    pub http_addr: String,

    #[clap(long, short = 't', default_value = "swarm")]
    pub types: String,

    #[clap(long, short = 'f', default_value = "bns-node.toml")]
    pub config_filename: String,

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

async fn run(localhost: &str, key: SecretKey, stun: &str) {
    let swarm = Swarm::new(TkChannel::new(1), stun.to_string());
    let signaler = swarm.signaler();
    let localhost = localhost.to_owned();

    tokio::spawn(async move {
        let swarm = swarm.clone();
        let http_addr = localhost.clone();
        let service = make_service_fn(move |_| {
            let swarm = swarm.to_owned();
            async move {
                Ok::<_, hyper::Error>(service_fn(move |req| {
                    discoveries_services(req, swarm.to_owned(), key)
                }))
            }
        });

        let sock_addr: SocketAddr = http_addr.clone().parse().unwrap();
        let server = Server::bind(&sock_addr).serve(service);
        println!("Serving on {}", http_addr.clone());
        // Run this server for... forever!
        if let Err(e) = server.await {
            eprintln!("server error: {}", e);
        }
    });

    let mut channel = signaler.lock().unwrap();
    tokio::select! {
        _ = channel.recv() => {
            println!("received done signal!");
        }
        _ = tokio::signal::ctrl_c() => {
            println!("");
        }
    };
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    Logger::init(args.log_level)?;
    let key = SecretKey::from_str(&args.eth_key)?;
    run(&args.http_addr, key, &args.stun_server).await;
    Ok(())
}
