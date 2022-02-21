mod discovery;
mod http_error;
mod observer;

use std::sync::Arc;
use axum::{
    routing::{get, post},
    AddExtensionLayer, Router, Server,
};
use bns_core::signing::SecretKey;
use bns_core::swarm::Swarm;

pub async fn run_service(addr: String, swarm: Arc<Swarm>, key: SecretKey) {
    let binding_addr = addr.parse().unwrap();

    let swarm_layer = AddExtensionLayer::new(swarm);
    let key_layer = AddExtensionLayer::new(key);

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
