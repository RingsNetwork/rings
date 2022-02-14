#![feature(async_closure)]
use anyhow::Result;
use bns_core::channels::default::TkChannel;
use bns_core::swarm::Swarm;
use bns_core::transports::default::DefaultTransport;
use bns_core::types::channel::Channel;
use bns_core::types::ice_transport::IceTransport;
use bns_node::config::read_config;
use bns_node::discoveries::http::sdp_handler;
use bns_node::discoveries::http::send_to_swarm;
use bns_node::logger::Logger;
use clap::Parser;
use hyper::service::{make_service_fn, service_fn};
use hyper::Server;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
pub struct Args {
    #[clap(long, short = 'd', default_value = "127.0.0.1:50000")]
    pub http_addr: String,

    #[clap(long, short = 't', default_value = "swarm")]
    pub types: String,

    #[clap(long, short = 'f', default_value = "bns-node.toml")]
    pub config_filename: String,

    #[clap(long, short = 'l', default_value = "Info")]
    pub log_level: String,

    #[clap(
        long = "eth",
        short = 'e',
        default_value = "http://127.0.0.1:8545",
        env
    )]
    pub eth_endpoint: String,

    #[clap(long = "key", short = 'w', env)]
    pub eth_key: String,
}

async fn run_as_swarm(localhost: &str) {
    let swarm = Swarm::new(TkChannel::new(1));
    let signaler = swarm.signaler();
    let localhost = localhost.to_owned();

    tokio::spawn(async move {
        let swarm = swarm.clone();
        let http_addr = localhost.clone();
        let service = make_service_fn(move |_| {
            let swarm = swarm.to_owned();
            let localhost = localhost.clone();
            async move {
                Ok::<_, hyper::Error>(service_fn(move |req| {
                    sdp_handler(req, swarm.to_owned())
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
    run_as_swarm(&args.http_addr).await;
    Ok(())
}
