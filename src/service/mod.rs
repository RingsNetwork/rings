//! rings-node service run with `Swarm` and chord stabilization.
#![warn(missing_docs)]
mod http_error;

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use http::header;
use http::HeaderMap;
use http::HeaderValue;
use jsonrpc_core::MetaIoHandler;
use tokio::sync::broadcast::Receiver;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

use self::http_error::HttpError;
use crate::backend::types::BackendMessage;
use crate::jsonrpc::RpcMeta;
use crate::prelude::rings_core::ecc::PublicKey;
use crate::processor::Processor;

#[derive(Clone)]
struct JsonrpcState {
    processor: Arc<Processor>,
    io_handler: Arc<MetaIoHandler<RpcMeta>>,
    pubkey: Arc<PublicKey>,
    receiver: Arc<Mutex<Receiver<BackendMessage>>>,
}

/// Run a web server to handle jsonrpc request
pub async fn run_service(
    addr: String,
    processor: Arc<Processor>,
    pubkey: Arc<PublicKey>,
    receiver: Receiver<BackendMessage>,
) -> anyhow::Result<()> {
    let binding_addr = addr.parse().unwrap();

    let mut jsonrpc_handler: MetaIoHandler<RpcMeta> = MetaIoHandler::default();
    crate::jsonrpc::build_handler(&mut jsonrpc_handler).await;
    let jsonrpc_handler_layer = Arc::new(jsonrpc_handler);
    let receiver = Arc::new(Mutex::new(receiver));

    let jsonrpc_state = Arc::new(JsonrpcState {
        processor,
        io_handler: jsonrpc_handler_layer,
        pubkey,
        receiver,
    });

    let axum_make_service = Router::new()
        .route("/", post(jsonrpc_io_handler).with_state(jsonrpc_state))
        .route("/status", get(status_handler))
        .layer(CorsLayer::permissive())
        .layer(axum::middleware::from_fn(node_info_header))
        .into_make_service();

    println!("Server listening on http://{}", addr);
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
        Processor::verify_signature(signature.as_bytes(), &state.pubkey)
            .map_err(|_| HttpError::BadRequest)?
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

async fn status_handler() -> Result<axum::extract::Json<serde_json::Value>, HttpError> {
    Ok(axum::extract::Json(serde_json::json!({
        "node_version": crate::util::build_version()
    })))
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
