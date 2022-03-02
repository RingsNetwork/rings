mod discovery;
mod http_error;
mod observer;

use axum::{
    extract::Extension,
    routing::{get, post},
    Router, Server,
};
use bns_core::ecc::SecretKey;
use bns_core::swarm::Swarm;
use std::sync::Arc;

pub async fn run_service(addr: String, swarm: Arc<Swarm>, key: SecretKey) {
    let binding_addr = addr.parse().unwrap();

    let swarm_layer = Extension(swarm);
    let key_layer = Extension(key);

    let app = Router::new()
        .route(
            "/sdp",
            post(discovery::handshake_handler)
                .layer(&swarm_layer)
                .layer(&key_layer),
        )
        .route(
            "/connect",
            get(discovery::connect_handler)
                .layer(&swarm_layer)
                .layer(&key_layer),
        );

    println!("Service listening on http://{}", addr);

    Server::bind(&binding_addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
