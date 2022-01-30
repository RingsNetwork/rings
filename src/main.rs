use anyhow::Result;
use bns_core::channels::default::TkChannel;
use bns_core::transports::default::DefaultTransport;
use bns_core::types::channel::Channel;
use bns_core::types::ice_transport::IceTransport;
use bns_core::types::ice_transport::IceTransportCallback;
use bns_node::discoveries::http::remote_handler;
use clap::Parser;
use hyper::service::{make_service_fn, service_fn};
use hyper::Server;
use std::net::SocketAddr;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Args {
    #[clap(long, short = 'h', default_value = "0.0.0.0:60000")]
    http_addr: String,

    #[clap(long, short = 'r', default_value = "0.0.0.0:50000")]
    remote_addr: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut ice_transport = DefaultTransport::new(TkChannel::new(1));
    let signaler = ice_transport.signaler();
    ice_transport.start().await?;
    ice_transport.setup_callback().await?;

    tokio::spawn(async move {
        let ice_transport = ice_transport.clone();
        let remote_addr = args.remote_addr.clone();

        let service = make_service_fn(move |_| {
            let ice_transport = ice_transport.to_owned();
            let remote_addr = remote_addr.to_owned();

            async move {
                Ok::<_, hyper::Error>(service_fn(move |req| {
                    remote_handler(req, remote_addr.to_owned(), ice_transport.to_owned())
                }))
            }
        });

        let http_addr: SocketAddr = args.http_addr.parse().unwrap();
        let server = Server::bind(&http_addr).serve(service);

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
    Ok(())
}
