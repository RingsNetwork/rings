#![feature(async_closure)]
use anyhow::Result;
use bns_core::channels::default::TkChannel;
use bns_core::swarm::Swarm;
use bns_core::transports::default::DefaultTransport;
use bns_core::types::channel::Channel;
use bns_core::types::ice_transport::IceTransport;
use bns_node::config::read_config;
use bns_node::discoveries::http::{sdp_handler, Payload};
use bns_node::logger::Logger;
use clap::Parser;
use futures::future::join_all;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Client, Method, Request, Server};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
pub struct Args {
    #[clap(long, short = 'd', default_value = "127.0.0.1:50000")]
    pub http_addr: String,
    #[clap(long, short = 't', default_value = "swarm")]
    pub types: String,
}

async fn start_swarm(host: &str) {
    let swarm = Swarm::new(TkChannel::new(1));
    let signaler = swarm.signaler();
    let host = host.to_owned();

    tokio::spawn(async move {
        let swarm = swarm.clone();
        let http_addr = host.clone();
        let service = make_service_fn(move |_| {
            let swarm = swarm.to_owned();
            let host = host.clone();
            async move {
                Ok::<_, hyper::Error>(service_fn(move |req| {
                    sdp_handler(req, host.clone(), swarm.to_owned())
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

async fn start_node(host: &str) {
    let mut transport = DefaultTransport::new(Arc::new(Mutex::new(TkChannel::new(1))));
    let offer = transport.start_node().await.unwrap();
    let config = read_config("bns-node.toml");
    let swarm_host = format!("{}:{}", config.swarm.addr, config.swarm.port);

    println!("1 {:?}", transport.get_pending_candidates().await);

    let node = format!("http://{}/offer", swarm_host);
    println!("2 {:?}", transport.get_pending_candidates().await);
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(node.to_owned())
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::empty())
        //.body(Body::from(serde_json::to_string(&req).unwrap()))
        .unwrap();
    println!("3 {:?}", transport.get_pending_candidates().await);

    let local_candidates_json = join_all(
        transport
            .get_pending_candidates()
            .await
            .iter()
            .map(async move |c| c.clone().to_json().await.unwrap()),
    )
    .await;
    let payload = Payload {
        sdp: serde_json::to_string(&offer).unwrap(),
        host: swarm_host.clone(),
        candidates: local_candidates_json,
    };

    println!("4 {:?}", transport.get_pending_candidates().await);
    match Client::new().request(req).await {
        Ok(resp) => {
            println!("Successful Send Offer");
            println!("Response: {:?}", resp);
            let data: Payload =
                serde_json::from_slice(&hyper::body::to_bytes(resp).await.unwrap()).unwrap();
            let answer = serde_json::from_str::<RTCSessionDescription>(&data.sdp).unwrap();
            transport.set_remote_description(answer).await.unwrap();
            for c in data.candidates {
                transport
                    .add_ice_candidate(c.candidate.clone())
                    .await
                    .unwrap();
            }
        }
        Err(e) => panic!("Opps, Sending Offer Failed with {:?}", e),
    };

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
        start_swarm(&args.http_addr).await;
    } else {
        start_node(&args.http_addr).await;
    }
    Ok(())
}
