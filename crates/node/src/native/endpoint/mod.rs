//! rings-node service run with `Swarm` and chord stabilization.
mod http_error;
mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::extract::Request;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::WWW_AUTHENTICATE;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use jsonrpc_core::MetaIoHandler;
use rings_gateway::GatewayStatus;
use rings_gateway::GatewayStatusHandle;
use rings_rpc::protos::rings_node::NodeInfoResponse;
use tokio::net::TcpListener;

use self::http_error::HttpError;
use crate::native::api_auth::ApiSecurity;
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

/// Gateway status endpoint state.
#[derive(Clone)]
pub struct GatewayStatusState {
    status: GatewayStatusHandle,
}

struct ExternalRpcMiddleware;
struct InternalRpcMiddleware;

/// Run a web server to handle jsonrpc request locally
pub async fn run_internal_api(
    port: u16,
    processor: Arc<Processor>,
    security: Arc<ApiSecurity>,
) -> anyhow::Result<()> {
    run_internal_api_with_gateway(port, processor, None, security).await
}

/// Run the local JSON-RPC server with an optional foreground-gateway status endpoint.
pub async fn run_internal_api_with_gateway(
    port: u16,
    processor: Arc<Processor>,
    gateway: Option<GatewayStatusHandle>,
    security: Arc<ApiSecurity>,
) -> anyhow::Result<()> {
    let gateway_configured = gateway.is_some();
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

    let mut router = Router::new()
        .route(
            "/",
            post(jsonrpc_io_handler).with_state(jsonrpc_state.clone()),
        )
        .route("/ws", get(ws_handler).with_state(ws_state))
        .route("/status", get(status_handler).with_state(status_state));
    if let Some(status) = gateway {
        router = router.route(
            "/gateway/status",
            get(gateway_status_handler).with_state(Arc::new(GatewayStatusState { status })),
        );
    }
    let axum_make_service =
        secure_router(router, security).into_make_service_with_connect_info::<SocketAddr>();

    println!("JSON-RPC endpoint: http://{binding_addr}");
    println!("WebSocket endpoint: http://{binding_addr}/ws");
    if gateway_configured {
        println!("Gateway status endpoint: http://{binding_addr}/gateway/status");
    }
    let listener = TcpListener::bind(binding_addr).await?;
    axum::serve(listener, axum_make_service).await?;
    Ok(())
}

/// Run a web server to handle jsonrpc request from external
pub async fn run_external_api(
    addr: String,
    processor: Arc<Processor>,
    security: Arc<ApiSecurity>,
) -> anyhow::Result<()> {
    let binding_addr: SocketAddr = addr.parse()?;
    security.validate_external_listener(binding_addr)?;

    let jsonrpc_handler = MetaIoHandler::with_middleware(ExternalRpcMiddleware);
    let jsonrpc_state = Arc::new(JsonRpcState {
        processor: processor.clone(),
        io_handler: jsonrpc_handler,
    });

    let status_state = Arc::new(StatusState { processor });

    let router = Router::new()
        .route(
            "/",
            post(jsonrpc_io_handler).with_state(jsonrpc_state.clone()),
        )
        .route("/status", get(status_handler).with_state(status_state));
    let axum_make_service =
        secure_router(router, security).into_make_service_with_connect_info::<SocketAddr>();

    println!("JSON-RPC endpoint: http://{addr}");
    let listener = TcpListener::bind(binding_addr).await?;
    axum::serve(listener, axum_make_service).await?;
    Ok(())
}

fn secure_router(router: Router, security: Arc<ApiSecurity>) -> Router {
    let cors = security.cors_layer();
    router
        .layer(axum::middleware::from_fn(node_info_header))
        .layer(axum::middleware::from_fn_with_state(
            security,
            enforce_api_security,
        ))
        .layer(cors)
}

async fn enforce_api_security(
    State(security): State<Arc<ApiSecurity>>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    if !security.authorizes(req.headers()) {
        return (
            StatusCode::UNAUTHORIZED,
            [(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
            "authentication required",
        )
            .into_response();
    }
    if is_jsonrpc_post(&req) && !has_json_content_type(&req) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "application/json required",
        )
            .into_response();
    }
    next.run(req).await
}

fn is_jsonrpc_post(req: &Request) -> bool {
    req.method() == Method::POST && req.uri().path() == "/"
}

fn has_json_content_type(req: &Request) -> bool {
    req.headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
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

async fn node_info_header(req: Request, next: axum::middleware::Next) -> axum::response::Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();

    if let Ok(version) = HeaderValue::from_str(crate::util::build_version().as_str()) {
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

async fn gateway_status_handler(
    State(state): State<Arc<GatewayStatusState>>,
) -> axum::Json<GatewayStatus> {
    axum::Json(state.status.snapshot())
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
    tracing::debug!("ws connected, remote: {}", addr);
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

#[cfg(test)]
mod security_tests {
    use axum::body::Body;
    use axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn jsonrpc_content_type_rejects_simple_cross_origin_posts() {
        let plain = Request::builder()
            .method(Method::POST)
            .uri("/")
            .header(CONTENT_TYPE, "text/plain")
            .body(Body::empty());
        let json = Request::builder()
            .method(Method::POST)
            .uri("/")
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::empty());
        assert!(matches!(plain, Ok(request) if !has_json_content_type(&request)));
        assert!(matches!(json, Ok(request) if has_json_content_type(&request)));
    }

    fn test_router() -> Router {
        let origins = ["https://app.example.com".to_string()];
        let security = ApiSecurity::new(
            "0123456789abcdef0123456789abcdef".to_string(),
            &origins,
            false,
        );
        let security = match security {
            Ok(security) => Arc::new(security),
            Err(_) => return Router::new(),
        };
        let router = Router::new()
            .route("/", post(|| async { "accepted" }))
            .route("/status", get(|| async { "status" }))
            .route("/ws", get(|| async { "websocket" }))
            .route("/gateway/status", get(|| async { "gateway status" }));
        secure_router(router, security)
    }

    fn authorized_request(content_type: &str) -> std::result::Result<Request, axum::http::Error> {
        Request::builder()
            .method(Method::POST)
            .uri("/")
            .header(
                axum::http::header::AUTHORIZATION,
                "Bearer 0123456789abcdef0123456789abcdef",
            )
            .header(CONTENT_TYPE, content_type)
            .body(Body::from("{}"))
    }

    #[tokio::test]
    async fn router_requires_auth_before_every_control_route() {
        for (method, path) in [
            (Method::POST, "/"),
            (Method::GET, "/status"),
            (Method::GET, "/ws"),
            (Method::GET, "/gateway/status"),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{}"));
            let response = test_router()
                .oneshot(request.expect("test request must build"))
                .await;
            assert!(
                matches!(response, Ok(response) if response.status() == StatusCode::UNAUTHORIZED),
                "route {path} was not protected"
            );
        }
    }

    #[tokio::test]
    async fn router_rejects_simple_content_type_and_accepts_authenticated_json() {
        let plain = test_router()
            .oneshot(authorized_request("text/plain").expect("test request must build"))
            .await;
        let json = test_router()
            .oneshot(authorized_request("application/json").expect("test request must build"))
            .await;
        assert!(matches!(
            plain,
            Ok(response) if response.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE
        ));
        assert!(matches!(json, Ok(response) if response.status() == StatusCode::OK));
    }

    #[tokio::test]
    async fn router_emits_cors_only_for_the_configured_exact_origin() {
        let allowed = authorized_request("application/json").map(|mut request| {
            request.headers_mut().insert(
                axum::http::header::ORIGIN,
                HeaderValue::from_static("https://app.example.com"),
            );
            request
        });
        let denied = authorized_request("application/json").map(|mut request| {
            request.headers_mut().insert(
                axum::http::header::ORIGIN,
                HeaderValue::from_static("https://attacker.example"),
            );
            request
        });
        let allowed = test_router()
            .oneshot(allowed.expect("test request must build"))
            .await;
        let denied = test_router()
            .oneshot(denied.expect("test request must build"))
            .await;
        assert!(matches!(
            allowed,
            Ok(response)
                if response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN)
                    == Some(&HeaderValue::from_static("https://app.example.com"))
        ));
        assert!(matches!(
            denied,
            Ok(response) if response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none()
        ));
    }
}
