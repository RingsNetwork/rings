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
                    sdp_handler(req, localhost.clone(), swarm.to_owned())
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

async fn run_as_node(localhost: &str, config_filename: &str) {
    let mut transport = DefaultTransport::new(Arc::new(Mutex::new(TkChannel::new(1))));
    let config = read_config(config_filename);
    let swarmhost = format!("{}:{}", config.swarm.addr, config.swarm.port);
    send_to_swarm(&mut transport, localhost, &swarmhost).await;
    let mut channel = transport.signaler.lock().unwrap();
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
    Logger::init()?;
    let args = Args::parse();
    if args.types == "swarm" {
        run_as_swarm(&args.http_addr).await;
    } else {
        run_as_node(&args.http_addr, &args.config_filename).await;
    }
    Ok(())
}
