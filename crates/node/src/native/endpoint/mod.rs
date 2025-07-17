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
use jsonrpc_core::MetaIoHandler;
use rings_rpc::protos::rings_node::NodeInfoResponse;
use tower_http::cors::CorsLayer;

use self::http_error::HttpError;
use crate::processor::Processor;

/// JSON-RPC state
#[derive(Clone)]
pub struct JsonRpcState<M>
where M: jsonrpc_core::Middleware<Arc<Processor>>
{
    processor: Arc<Processor>,
    io_handler: MetaIoHandler<Arc<Processor>, M>,
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

struct ExternalRpcMiddleware;
struct InternalRpcMiddleware;

/// Run a web server to handle jsonrpc request locally
pub async fn run_internal_api(port: u16, processor: Arc<Processor>) -> anyhow::Result<()> {
    let binding_addr = SocketAddr::from(([127, 0, 0, 1], port));

    let jsonrpc_handler = MetaIoHandler::with_middleware(InternalRpcMiddleware);
    let jsonrpc_state = Arc::new(JsonRpcState {
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

    println!("JSON-RPC endpoint: http://{binding_addr}");
    println!("WebSocket endpoint: http://{binding_addr}/ws");
    axum::Server::bind(&binding_addr)
        .serve(axum_make_service)
        .await?;
    Ok(())
}

/// Run a web server to handle jsonrpc request from external
pub async fn run_external_api(addr: String, processor: Arc<Processor>) -> anyhow::Result<()> {
    let binding_addr = addr.parse().unwrap();

    let jsonrpc_handler = MetaIoHandler::with_middleware(ExternalRpcMiddleware);
    let jsonrpc_state = Arc::new(JsonRpcState {
        processor: processor.clone(),
        io_handler: jsonrpc_handler,
    });

    let status_state = Arc::new(StatusState { processor });

    let axum_make_service = Router::new()
        .route(
            "/",
            post(jsonrpc_io_handler).with_state(jsonrpc_state.clone()),
        )
        .route("/status", get(status_handler).with_state(status_state))
        .layer(CorsLayer::permissive())
        .layer(axum::middleware::from_fn(node_info_header))
        .into_make_service_with_connect_info::<SocketAddr>();

    println!("JSON-RPC endpoint: http://{addr}");
    axum::Server::bind(&binding_addr)
        .serve(axum_make_service)
        .await?;
    Ok(())
}

async fn jsonrpc_io_handler<M>(
    State(state): State<Arc<JsonRpcState<M>>>,
    body: String,
) -> Result<JsonResponse, HttpError>
where
    M: jsonrpc_core::Middleware<Arc<Processor>>,
{
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
) -> Result<axum::Json<NodeInfoResponse>, HttpError> {
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

mod jsonrpc_middleware_impl {
    use std::future::Future;

    use jsonrpc_core::futures_util::future;
    use jsonrpc_core::futures_util::future::Either;
    use jsonrpc_core::futures_util::FutureExt;
    use jsonrpc_core::middleware::NoopCallFuture;
    use jsonrpc_core::middleware::NoopFuture;
    use jsonrpc_core::*;
    use rings_rpc::protos::rings_node_handler::ExternalRpcHandler;
    use rings_rpc::protos::rings_node_handler::InternalRpcHandler;

    use super::*;

    impl Middleware<Arc<Processor>> for InternalRpcMiddleware {
        type Future = NoopFuture;
        type CallFuture = NoopCallFuture;

        fn on_call<F, X>(
            &self,
            call: Call,
            meta: Arc<Processor>,
            next: F,
        ) -> Either<Self::CallFuture, X>
        where
            F: Fn(Call, Arc<Processor>) -> X + Send + Sync,
            X: Future<Output = Option<Output>> + Send + 'static,
        {
            match call {
                Call::MethodCall(req) => {
                    let fut = InternalRpcHandler
                        .handle_request(meta, req.method, req.params.into())
                        .then(move |res| {
                            future::ready(Some(Output::from(res, req.id, req.jsonrpc)))
                        });
                    Either::Left(Box::pin(fut))
                }
                _ => Either::Left(Box::pin(next(call, meta))),
            }
        }
    }

    impl Middleware<Arc<Processor>> for ExternalRpcMiddleware {
        type Future = NoopFuture;
        type CallFuture = NoopCallFuture;

        fn on_call<F, X>(
            &self,
            call: Call,
            meta: Arc<Processor>,
            next: F,
        ) -> Either<Self::CallFuture, X>
        where
            F: Fn(Call, Arc<Processor>) -> X + Send + Sync,
            X: Future<Output = Option<Output>> + Send + 'static,
        {
            match call {
                Call::MethodCall(req) => {
                    let fut = ExternalRpcHandler
                        .handle_request(meta, req.method, req.params.into())
                        .then(move |res| {
                            future::ready(Some(Output::from(res, req.id, req.jsonrpc)))
                        });
                    Either::Left(Box::pin(fut))
                }
                _ => Either::Left(Box::pin(next(call, meta))),
            }
        }
    }
}
