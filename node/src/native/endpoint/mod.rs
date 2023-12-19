//! rings-node service run with `Swarm` and chord stabilization.
#![warn(missing_docs)]
mod http_error;
mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use tower_http::cors::CorsLayer;

use self::http_error::HttpError;
use crate::prelude::jsonrpc_core::MetaIoHandler;
use crate::prelude::rings_rpc::response::NodeInfo;
use crate::processor::Processor;

/// Jsonrpc state
#[derive(Clone)]
pub struct JsonrpcState {
    processor: Arc<Processor>,
    io_handler: MetaIoHandler<Arc<Processor>>,
}

/// websocket state
#[derive(Clone)]
#[allow(dead_code)]
pub struct WsState {
    processor: Arc<Processor>,
}

/// Status state
#[derive(Clone)]
pub struct StatusState {
    processor: Arc<Processor>,
}

/// Run a web server to handle jsonrpc request
pub async fn run_http_api(addr: String, processor: Arc<Processor>) -> anyhow::Result<()> {
    let binding_addr = addr.parse().unwrap();

    let mut jsonrpc_handler: MetaIoHandler<Arc<Processor>> = MetaIoHandler::default();
    crate::jsonrpc::handler::default::register_methods(&mut jsonrpc_handler).await;

    let jsonrpc_state = Arc::new(JsonrpcState {
        processor: processor.clone(),
        io_handler: jsonrpc_handler,
    });

    let ws_state = Arc::new(WsState {
        processor: processor.clone(),
    });

    let status_state = Arc::new(StatusState { processor });

    let axum_make_service = Router::new()
        .route(
            "/",
            post(jsonrpc_io_handler).with_state(jsonrpc_state.clone()),
        )
        .route("/ws", get(ws_handler).with_state(ws_state))
        .route("/status", get(status_handler).with_state(status_state))
        .layer(CorsLayer::permissive())
        .layer(axum::middleware::from_fn(node_info_header))
        .into_make_service_with_connect_info::<SocketAddr>();

    println!("JSON-RPC endpoint: http://{}", addr);
    println!("WebSocket endpoint: http://{}/ws", addr);
    axum::Server::bind(&binding_addr)
        .serve(axum_make_service)
        .await?;
    Ok(())
}

async fn jsonrpc_io_handler(
    State(state): State<Arc<JsonrpcState>>,
    body: String,
) -> Result<JsonResponse, HttpError> {
    let r = state
        .io_handler
        .handle_request(&body, state.processor.clone())
        .await
        .ok_or(HttpError::BadRequest)?;
    Ok(JsonResponse(r))
}

async fn node_info_header<B>(
    req: http::Request<B>,
    next: axum::middleware::Next<B>,
) -> axum::response::Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();

    if let Ok(version) = http::HeaderValue::from_str(crate::util::build_version().as_str()) {
        headers.insert("X-NODE-VERSION", version);
    }
    res
}

async fn status_handler(
    State(state): State<Arc<StatusState>>,
) -> Result<axum::Json<NodeInfo>, HttpError> {
    let info = state
        .processor
        .get_node_info()
        .await
        .map_err(|_| HttpError::Internal)?;
    Ok(axum::Json(info))
}

/// JSON response struct
#[derive(Debug, Clone)]
pub struct JsonResponse(String);

impl IntoResponse for JsonResponse {
    fn into_response(self) -> axum::response::Response {
        ([("content-type", "application/json")], self.0).into_response()
    }
}

async fn ws_handler(
    State(state): State<Arc<WsState>>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    tracing::info!("ws connected, remote: {}", addr);
    ws.on_upgrade(move |socket| self::ws::handle_socket(state, socket))
}
