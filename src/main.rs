use bns_core::channels::default::TkChannel;
use bns_core::discoveries::http::{sdp_handler, sync_offer_to_swarm};
use bns_core::swarm::Swarm;
use bns_core::types::channel::Channel;
use bns_node::config::read_config;
use bns_node::logger::Logger;
use clap::Parser;
use hyper::service::{make_service_fn, service_fn};
use hyper::Server;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
pub struct Args {
    #[clap(long, short = 'd', default_value = "127.0.0.1:50000")]
    pub http_addr: String,
    #[clap(long, short = 's', default_value = "stun:stun.l.google.com:19302")]
    pub stun_server: String,
    #[clap(long, short = 'v', default_value = "Info")]
    pub log_level: String,
    #[clap(long, short = 't', default_value = "node")]
    pub types: String,
    #[clap(long, short = 'f', default_value = "bns-node.toml")]
    pub filename: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    //Logger::init(args.log_level)?;
    let swarm = Arc::new(Mutex::new(Swarm::new(
        TkChannel::new(1),
        args.stun_server.clone(),
    )));
    let swarm2 = Arc::clone(&swarm);
    let signaler = swarm.lock().await.signaler();
    let config = read_config(&args.filename);
    let http_addr;
    if args.types == "node" {
        http_addr = format!("{}:{}", config.node.addr, config.node.port);
    } else {
        assert_eq!(args.types, "swarm");
        http_addr = format!("{}:{}", config.swarm.addr, config.swarm.port);
    }

    tokio::spawn(async move {
        let swarm = Arc::clone(&swarm);
        let sock_addr = http_addr.clone();
        let service = make_service_fn(move |_| {
            let swarm = Arc::clone(&swarm);
            let http_addr = http_addr.clone();
            async move {
                Ok::<_, hyper::Error>(service_fn(move |req| {
                    sdp_handler(req, http_addr.clone(), swarm.to_owned())
                }))
            }
        });
        let sock_addr: SocketAddr = sock_addr.parse().unwrap();
        let server = Server::bind(&sock_addr).serve(service);
        println!("Serving on {:?}", sock_addr);
        log::info!("Serving on {:?}", sock_addr);
        // Run this server for... forever!
        if let Err(e) = server.await {
            log::error!("server error: {}", e);
        }
    });
    if args.types == "node" {
        let http_addr = format!("{}:{}", config.node.addr, config.node.port);
        let swarm_addr = format!("{}:{}", config.swarm.addr, config.swarm.port);
        sync_offer_to_swarm(Arc::clone(&swarm2), swarm_addr, http_addr).await;
    }
    let mut channel = signaler.lock().unwrap();
    tokio::select! {
        _ = channel.recv() => {
            log::info!("received done signal!");
        }
        _ = tokio::signal::ctrl_c() => {
            log::info!("Stopped");
        }
    };
    Ok(())
}
