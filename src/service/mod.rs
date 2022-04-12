mod backgrounds;
mod discovery;
mod http_error;
mod is_turn;
pub mod jsonrpc;
pub mod processor;
pub mod request;
pub mod response;
mod result;
use self::{http_error::HttpError, processor::Processor, result::HttpResult};
pub use backgrounds::run_stabilize;
pub use is_turn::run_udp_turn;

use axum::{
    extract::Extension,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use http::header::{self, HeaderName, HeaderValue};
use jsonrpc_core::MetaIoHandler;
use std::sync::Arc;
use tower_http::set_header::SetResponseHeaderLayer;
use bns_core::swarm::Swarm;
use bns_core::session::SessionManager;


pub async fn run_service(addr: String, swarm: Arc<Swarm>, session: SessionManager) -> anyhow::Result<()> {
    let binding_addr = addr.parse().unwrap();

    let swarm_layer = Extension(swarm.clone());
    let key_layer = Extension(session);

    let mut jsonrpc_handler: MetaIoHandler<Processor> = MetaIoHandler::default();
    jsonrpc::build_handler(&mut jsonrpc_handler).await;
    let jsonrpc_handler_layer = Extension(Arc::new(jsonrpc_handler));

    let axum_make_service = Router::new()
        .route(
            "/sdp",
            post(discovery::handshake_handler)
                .layer(&swarm_layer)
                .layer(&key_layer)
                .layer(SetResponseHeaderLayer::overriding(
                    HeaderName::from_static("access-control-allow-origin"),
                    HeaderValue::from_static("*"),
                )),
        )
        .route(
            "/connect",
            get(discovery::connect_handler)
                .layer(&swarm_layer)
                .layer(&key_layer),
        )
        .route(
            "/",
            post(jsonrpc_io_handler)
                .layer(&swarm_layer)
                .layer(&key_layer)
                .layer(&jsonrpc_handler_layer),
        )
        .into_make_service();

    println!("Service listening on http://{}", addr);
    hyper::Server::bind(&binding_addr)
        .serve(axum_make_service)
        .await?;
    Ok(())
}

pub async fn jsonrpc_io_handler(
    body: String,
    Extension(swarm): Extension<Arc<Swarm>>,
    Extension(session): Extension<SessionManager>,
    Extension(io_handler): Extension<Arc<MetaIoHandler<Processor>>>,
) -> HttpResult<JsonResponse> {
    let r = io_handler
        .handle_request(&body, (swarm, session.clone()).into())
        .await
        .ok_or(HttpError::BadRequest)?;
    Ok(JsonResponse(r))
}

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
