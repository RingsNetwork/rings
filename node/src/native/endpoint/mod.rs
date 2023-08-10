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
use tokio::sync::broadcast::Receiver;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

use self::http_error::HttpError;
use crate::backend::types::BackendMessage;
use crate::jsonrpc::RpcMeta;
use crate::prelude::http::header;
use crate::prelude::http::HeaderMap;
use crate::prelude::http::HeaderValue;
use crate::prelude::jsonrpc_core::MetaIoHandler;
use crate::prelude::rings_rpc::response::NodeInfo;
use crate::processor::Processor;

impl crate::prelude::jsonrpc_core::Metadata for RpcMeta {}

/// Jsonrpc state
#[derive(Clone)]
pub struct JsonrpcState {
    processor: Arc<Processor>,
    io_handler: Arc<MetaIoHandler<RpcMeta>>,
    receiver: Arc<Mutex<Receiver<BackendMessage>>>,
}

/// websocket state
#[derive(Clone)]
#[allow(dead_code)]
pub struct WsState {
    processor: Arc<Processor>,
    receiver: Arc<Receiver<BackendMessage>>,
}

/// Status state
#[derive(Clone)]
pub struct StatusState {
    processor: Arc<Processor>,
}

/// Run a web server to handle jsonrpc request
pub async fn run_http_api(
    addr: String,
    processor: Arc<Processor>,
    receiver: Receiver<BackendMessage>,
) -> anyhow::Result<()> {
    let binding_addr = addr.parse().unwrap();

    let mut jsonrpc_handler: MetaIoHandler<RpcMeta> = MetaIoHandler::default();
    crate::jsonrpc::build_handler(&mut jsonrpc_handler).await;
    let jsonrpc_handler_layer = Arc::new(jsonrpc_handler);

    let jsonrpc_state = Arc::new(JsonrpcState {
        processor: processor.clone(),
        io_handler: jsonrpc_handler_layer,
        receiver: Arc::new(Mutex::new(receiver.resubscribe())),
    });

    let ws_state = Arc::new(WsState {
        processor: processor.clone(),
        receiver: Arc::new(receiver.resubscribe()),
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
    headermap: HeaderMap,
    body: String,
) -> Result<JsonResponse, HttpError> {
    let is_auth = if let Some(signature) = headermap.get("X-SIGNATURE") {
        let sig = base64::decode(signature).map_err(|e| {
            tracing::debug!("signature: {:?}", signature);
            tracing::error!("signature decode failed: {:?}", e);
            HttpError::BadRequest
        })?;
        state
            .processor
            .swarm
            .session_sk()
            .session()
            .verify(&body, sig)
            .map_err(|e| {
                tracing::debug!("body: {:?}", body);
                tracing::debug!("signature: {:?}", signature);
                tracing::error!("signature verify failed: {:?}", e);
                e
            })
            .is_ok()
    } else {
        false
    };
    let r = state
        .io_handler
        .handle_request(
            &body,
            (state.processor.clone(), state.receiver.clone(), is_auth).into(),
        )
        .await
        .ok_or(HttpError::BadRequest)?;
    Ok(JsonResponse(r))
}

async fn node_info_header<B>(
    req: axum::http::Request<B>,
    next: axum::middleware::Next<B>,
) -> axum::response::Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();

    if let Ok(version) = axum::http::HeaderValue::from_str(crate::util::build_version().as_str()) {
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

async fn ws_handler(
    State(state): State<Arc<WsState>>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    tracing::info!("ws connected, remote: {}", addr);
    ws.on_upgrade(move |socket| self::ws::handle_socket(state, socket))
}
