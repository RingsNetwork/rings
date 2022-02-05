use anyhow::Result;
use bns_core::channels::default::TkChannel;
use bns_core::discoveries::http::sdp_handler;
use bns_core::swarm::Swarm;
use bns_core::types::channel::Channel;
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
    #[clap(long, short = 't', default_value = "offer")]
    pub types: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    //Logger::init(args.log_level)?;
    let swarm = Arc::new(Mutex::new(Swarm::new(TkChannel::new(1), args.stun_server)));
    let signaler = swarm.lock().await.signaler();

    tokio::spawn(async move {
        let swarm = Arc::clone(&swarm);
        let http_addr = args.http_addr.clone();
        let service = make_service_fn(move |_| {
            let swarm = Arc::clone(&swarm);
            let http_addr = http_addr.clone();
            async move {
                Ok::<_, hyper::Error>(service_fn(move |req| {
                    sdp_handler(req, http_addr.clone(), swarm.to_owned())
                }))
            }
        });

        let http_addr: SocketAddr = args.http_addr.parse().unwrap();
        let server = Server::bind(&http_addr).serve(service);
        log::info!("Serving on {}", args.http_addr);
        // Run this server for... forever!
        if let Err(e) = server.await {
            log::error!("server error: {}", e);
        }
    });

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
