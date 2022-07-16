#![warn(missing_docs)]
//! rings-node server
mod http_error;
#[cfg(feature = "daemon")]
mod is_turn;

use std::sync::Arc;

use axum::extract::Extension;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use http::header;
use http::header::HeaderValue;
use http::HeaderMap;
#[cfg(feature = "daemon")]
pub use is_turn::run_udp_turn;
use jsonrpc_core::MetaIoHandler;
use tower_http::cors::CorsLayer;

use self::http_error::HttpError;
use crate::jsonrpc::RpcMeta;
use crate::prelude::rings_core::dht::Stabilization;
use crate::prelude::rings_core::ecc::PublicKey;
use crate::prelude::rings_core::message::MessageHandler;
use crate::prelude::rings_core::swarm::Swarm;
use crate::processor::Processor;

/// Run a web server to handle jsonrpc request
pub async fn run_service(
    addr: String,
    swarm: Arc<Swarm>,
    msg_handler: Arc<MessageHandler>,
    stabilization: Arc<Stabilization>,
    pubkey: Arc<PublicKey>,
) -> anyhow::Result<()> {
    let binding_addr = addr.parse().unwrap();

    // let swarm_layer = Extension(swarm.clone());
    // let msg_handler_layer = Extension(msg_handler.clone());
    // let stabilization_layer = Extension(stabilization.clone());

    let processor = Arc::new(Processor::from((swarm, msg_handler, stabilization)));
    let processor_layer = Extension(processor);

    let mut jsonrpc_handler: MetaIoHandler<RpcMeta> = MetaIoHandler::default();
    crate::jsonrpc::build_handler(&mut jsonrpc_handler).await;
    let jsonrpc_handler_layer = Extension(Arc::new(jsonrpc_handler));

    let pubkey_layer = Extension(pubkey);

    let axum_make_service = Router::new()
        .route(
            "/",
            post(jsonrpc_io_handler)
                .layer(&processor_layer)
                .layer(&jsonrpc_handler_layer)
                .layer(&pubkey_layer),
        )
        .layer(CorsLayer::permissive())
        .into_make_service();

    println!("Server listening on http://{}", addr);
    axum::Server::bind(&binding_addr)
        .serve(axum_make_service)
        .await?;
    Ok(())
}

async fn jsonrpc_io_handler(
    body: String,
    headers: HeaderMap,
    Extension(processor): Extension<Arc<Processor>>,
    Extension(io_handler): Extension<Arc<MetaIoHandler<RpcMeta>>>,
    Extension(pubkey): Extension<Arc<PublicKey>>,
) -> Result<JsonResponse, HttpError> {
    // let is_auth
    let is_auth = if let Some(signature) = headers.get(header::AUTHORIZATION) {
        Processor::verify_signature(signature.as_bytes(), &pubkey)
            .map_err(|_| HttpError::BadRequest)?
    } else {
        false
    };
    let r = io_handler
        .handle_request(&body, (processor, is_auth).into())
        .await
        .ok_or(HttpError::BadRequest)?;
    Ok(JsonResponse(r))
}

/// JSON response struct
#[derive(Debug, Clone)]
pub struct JsonResponse(String);

impl IntoResponse for JsonResponse {
    fn into_response(self) -> axum::response::Response {
        (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            self.0,
        )
            .into_response()
    }
}
