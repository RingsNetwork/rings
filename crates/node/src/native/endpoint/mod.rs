//! Native JSON-RPC listeners: routing, per-listener authorization, and the JSON-RPC dispatch.
//!
//! Both listeners are built by `secure_router`, whose security layer decodes a JSON-RPC body
//! exactly once, decides the request's authorization requirement through
//! [`ApiListener::required_authorization`], and hands the decoded request to the route handler as
//! a `DecodedJsonRpc` extension so that no later stage buffers or parses the body again.
mod http_error;
mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ConnectInfo;
use axum::extract::FromRequest;
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
use axum::Extension;
use axum::Router;
use jsonrpc_core::ErrorCode;
use jsonrpc_core::MetaIoHandler;
use jsonrpc_core::Version;
use rings_gateway::GatewayStatus;
use rings_gateway::GatewayStatusHandle;
use rings_rpc::method::AuthorizationClass;
use rings_rpc::protos::rings_node::NodeInfoResponse;
use tokio::net::TcpListener;

use self::http_error::HttpError;
use crate::native::api_auth::ApiListener;
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

/// Security state of one listener: the node-wide credential policy and the listener's floor.
#[derive(Clone)]
struct ListenerSecurity {
    policy: Arc<ApiSecurity>,
    listener: ApiListener,
}

/// A JSON-RPC body decoded exactly once by the security layer.
///
/// The layer attaches it as a request extension and forwards an empty body, so the route
/// handler dispatches the decoded request instead of parsing again. A body that failed to
/// decode carries the JSON-RPC parse error the handler must answer with. Invariant: every
/// request reaching the JSON-RPC route carries this extension, because [`secure_router`] is the
/// only constructor of a served router.
#[derive(Clone)]
struct DecodedJsonRpc(Result<jsonrpc_core::Request, jsonrpc_core::Error>);

impl DecodedJsonRpc {
    /// Decode a body the way `MetaIoHandler::handle_request` does, keeping its parse error.
    fn decode(bytes: &[u8]) -> Self {
        Self(
            serde_json::from_slice(bytes)
                .map_err(|_| jsonrpc_core::Error::new(ErrorCode::ParseError)),
        )
    }

    /// Return the decoded request, or `None` for a body that did not decode.
    fn request(&self) -> Option<&jsonrpc_core::Request> {
        self.0.as_ref().ok()
    }
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
    let axum_make_service = internal_router(processor, gateway, security)
        .into_make_service_with_connect_info::<SocketAddr>();

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
    let axum_make_service =
        external_router(processor, security).into_make_service_with_connect_info::<SocketAddr>();

    println!("JSON-RPC endpoint: http://{addr}");
    let listener = TcpListener::bind(binding_addr).await?;
    axum::serve(listener, axum_make_service).await?;
    Ok(())
}

/// Build the operator's control router: JSON-RPC, WebSocket, status, and optional gateway status.
fn internal_router(
    processor: Arc<Processor>,
    gateway: Option<GatewayStatusHandle>,
    security: Arc<ApiSecurity>,
) -> Router {
    let jsonrpc_state = Arc::new(JsonRpcState {
        processor: processor.clone(),
        io_handler: MetaIoHandler::with_middleware(InternalRpcMiddleware),
    });
    let ws_state = Arc::new(WsState {
        processor: processor.clone(),
    });
    let status_state = Arc::new(StatusState { processor });

    let mut router = Router::new()
        .route("/", post(jsonrpc_io_handler).with_state(jsonrpc_state))
        .route("/ws", get(ws_handler).with_state(ws_state))
        .route("/status", get(status_handler).with_state(status_state));
    if let Some(status) = gateway {
        router = router.route(
            "/gateway/status",
            get(gateway_status_handler).with_state(Arc::new(GatewayStatusState { status })),
        );
    }
    secure_router(router, security, ApiListener::Internal)
}

/// Build the peer-facing router: the external JSON-RPC allowlist and the status read.
fn external_router(processor: Arc<Processor>, security: Arc<ApiSecurity>) -> Router {
    let jsonrpc_state = Arc::new(JsonRpcState {
        processor: processor.clone(),
        io_handler: MetaIoHandler::with_middleware(ExternalRpcMiddleware),
    });
    let status_state = Arc::new(StatusState { processor });

    let router = Router::new()
        .route("/", post(jsonrpc_io_handler).with_state(jsonrpc_state))
        .route("/status", get(status_handler).with_state(status_state));
    secure_router(router, security, ApiListener::External)
}

fn secure_router(router: Router, policy: Arc<ApiSecurity>, listener: ApiListener) -> Router {
    let cors = policy.cors_layer();
    router
        .layer(axum::middleware::from_fn(node_info_header))
        .layer(axum::middleware::from_fn_with_state(
            ListenerSecurity { policy, listener },
            enforce_api_security,
        ))
        .layer(cors)
}

/// Apply the listener's authorization policy to one request.
///
/// Routes other than the JSON-RPC root are status and control reads, so their floor is `⊤` on
/// both listeners; the JSON-RPC root takes the listener's floor. The requirement of a JSON-RPC
/// request is `floor ⊔ class(body)`, and since `⊤` absorbs under `⊔` a floor of `⊤` settles an
/// unauthenticated request before its body is read; only a public floor needs the body decoded
/// to reach a verdict. The body is read through the `Bytes` extractor so axum's default size
/// limit and its rejections apply as they did when the route handler extracted the body, and the
/// decoded request is forwarded as [`DecodedJsonRpc`] so the handler never parses it again.
async fn enforce_api_security(
    State(security): State<ListenerSecurity>,
    mut req: Request,
    next: axum::middleware::Next,
) -> Response {
    let authenticated = security.policy.authorizes(req.headers());
    let jsonrpc = is_jsonrpc_post(&req);
    let floor = if jsonrpc {
        security.listener.authorization_floor()
    } else {
        AuthorizationClass::Gated
    };
    if !floor.satisfied_by(authenticated) {
        return unauthorized();
    }
    if !jsonrpc {
        return next.run(req).await;
    }
    if !has_json_content_type(&req) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "application/json required",
        )
            .into_response();
    }
    let body = std::mem::take(req.body_mut());
    let decoded = match Bytes::from_request(Request::new(body), &()).await {
        Ok(bytes) => DecodedJsonRpc::decode(bytes.as_ref()),
        Err(rejection) => return rejection.into_response(),
    };
    let required = security.listener.required_authorization(decoded.request());
    if !required.satisfied_by(authenticated) {
        return unauthorized();
    }
    req.extensions_mut().insert(decoded);
    next.run(req).await
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
        "authentication required",
    )
        .into_response()
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
    Extension(DecodedJsonRpc(request)): Extension<DecodedJsonRpc>,
) -> Result<JsonResponse, HttpError>
where
    M: jsonrpc_core::Middleware<Arc<Processor>>,
{
    let response = match request {
        Ok(request) => {
            state
                .io_handler
                .handle_rpc_request(request, state.processor.clone())
                .await
        }
        // The handlers are built with the default compatibility, whose version is V2.
        Err(error) => Some(jsonrpc_core::Response::from(error, Some(Version::V2))),
    };
    let response = response.ok_or(HttpError::BadRequest)?;
    let body = serde_json::to_string(&response).map_err(|_| HttpError::Internal)?;
    Ok(JsonResponse(body))
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
    use axum::http::header::AUTHORIZATION;
    use axum::http::header::ORIGIN;
    use tower::ServiceExt;

    use super::*;
    use crate::tests::native::prepare_processor;

    macro_rules! token {
        () => {
            "0123456789abcdef0123456789abcdef"
        };
    }

    const TOKEN: &str = token!();
    const ORIGIN_ALLOWED: &str = "https://app.example.com";

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

    fn security() -> Option<Arc<ApiSecurity>> {
        let origins = [ORIGIN_ALLOWED.to_string()];
        ApiSecurity::new(TOKEN.to_string(), &origins, false)
            .ok()
            .map(Arc::new)
    }

    /// A router whose JSON-RPC route accepts whatever the security layer lets through.
    fn stub_router(listener: ApiListener) -> Router {
        let Some(security) = security() else {
            return Router::new();
        };
        let router = Router::new()
            .route("/", post(|| async { "accepted" }))
            .route("/status", get(|| async { "status" }))
            .route("/ws", get(|| async { "websocket" }))
            .route("/gateway/status", get(|| async { "gateway status" }));
        secure_router(router, security, listener)
    }

    fn call(method: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{{}}}}"#)
    }

    fn batch(methods: &[&str]) -> String {
        let calls = methods.iter().copied().map(call).collect::<Vec<_>>();
        format!("[{}]", calls.join(","))
    }

    fn jsonrpc_request(body: String) -> std::result::Result<Request, axum::http::Error> {
        Request::builder()
            .method(Method::POST)
            .uri("/")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
    }

    fn bearer(
        request: std::result::Result<Request, axum::http::Error>,
    ) -> std::result::Result<Request, axum::http::Error> {
        request.map(|mut request| {
            request.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_static(concat!("Bearer ", token!())),
            );
            request
        })
    }

    fn authorized_request(content_type: &str) -> std::result::Result<Request, axum::http::Error> {
        bearer(
            Request::builder()
                .method(Method::POST)
                .uri("/")
                .header(CONTENT_TYPE, content_type)
                .body(Body::from(call("nodeInfo"))),
        )
    }

    async fn status_of(
        router: Router,
        request: std::result::Result<Request, axum::http::Error>,
    ) -> Option<StatusCode> {
        let request = request.ok()?;
        router
            .oneshot(request)
            .await
            .ok()
            .map(|response| response.status())
    }

    #[tokio::test]
    async fn every_listener_requires_auth_before_every_control_route() {
        for listener in [ApiListener::Internal, ApiListener::External] {
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
                    .body(Body::from(call("nodeInfo")));
                assert_eq!(
                    status_of(stub_router(listener), request).await,
                    Some(StatusCode::UNAUTHORIZED),
                    "{listener:?} route {path} was not protected"
                );
            }
        }
    }

    #[tokio::test]
    async fn external_listener_serves_the_handshake_without_a_token() {
        for method in ["nodeDid", "answerOffer"] {
            assert_eq!(
                status_of(
                    stub_router(ApiListener::External),
                    jsonrpc_request(call(method))
                )
                .await,
                Some(StatusCode::OK),
                "{method} must be public on the external listener"
            );
        }
        let handshake = batch(&["nodeDid", "answerOffer"]);
        assert_eq!(
            status_of(
                stub_router(ApiListener::External),
                jsonrpc_request(handshake)
            )
            .await,
            Some(StatusCode::OK)
        );
    }

    #[tokio::test]
    async fn external_listener_gates_status_and_registry_reads() {
        for method in [
            "nodeInfo",
            "lookupOnlineNodes",
            "lookupOnionExits",
            "listPeers",
        ] {
            assert_eq!(
                status_of(
                    stub_router(ApiListener::External),
                    jsonrpc_request(call(method))
                )
                .await,
                Some(StatusCode::UNAUTHORIZED),
                "{method} must be gated on the external listener"
            );
        }
    }

    #[tokio::test]
    async fn internal_listener_gates_every_method_including_the_handshake() {
        for method in ["nodeDid", "answerOffer", "nodeInfo"] {
            assert_eq!(
                status_of(
                    stub_router(ApiListener::Internal),
                    jsonrpc_request(call(method))
                )
                .await,
                Some(StatusCode::UNAUTHORIZED),
                "{method} must be gated on the internal listener"
            );
        }
    }

    #[tokio::test]
    async fn a_batch_is_gated_by_its_strictest_member() {
        let mixed = batch(&["nodeDid", "nodeInfo"]);
        assert_eq!(
            status_of(
                stub_router(ApiListener::External),
                jsonrpc_request(mixed.clone())
            )
            .await,
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            status_of(
                stub_router(ApiListener::External),
                bearer(jsonrpc_request(mixed))
            )
            .await,
            Some(StatusCode::OK)
        );
    }

    #[tokio::test]
    async fn undecodable_and_unknown_bodies_are_gated() {
        for body in ["{}", "not json", "[]{", &call("notAMethod")] {
            assert_eq!(
                status_of(
                    stub_router(ApiListener::External),
                    jsonrpc_request(body.to_string())
                )
                .await,
                Some(StatusCode::UNAUTHORIZED),
                "body {body:?} must not be served without a token"
            );
        }
    }

    #[tokio::test]
    async fn a_public_call_still_requires_json_content_type() {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/")
            .header(CONTENT_TYPE, "text/plain")
            .body(Body::from(call("nodeDid")));
        assert_eq!(
            status_of(stub_router(ApiListener::External), request).await,
            Some(StatusCode::UNSUPPORTED_MEDIA_TYPE)
        );
    }

    #[tokio::test]
    async fn router_rejects_simple_content_type_and_accepts_authenticated_json() {
        for listener in [ApiListener::Internal, ApiListener::External] {
            assert_eq!(
                status_of(stub_router(listener), authorized_request("text/plain")).await,
                Some(StatusCode::UNSUPPORTED_MEDIA_TYPE)
            );
            assert_eq!(
                status_of(
                    stub_router(listener),
                    authorized_request("application/json")
                )
                .await,
                Some(StatusCode::OK)
            );
        }
    }

    #[tokio::test]
    async fn router_emits_cors_only_for_the_configured_exact_origin() {
        let with_origin = |origin: &'static str| {
            authorized_request("application/json").map(|mut request| {
                request
                    .headers_mut()
                    .insert(ORIGIN, HeaderValue::from_static(origin));
                request
            })
        };
        let allowed = stub_router(ApiListener::Internal)
            .oneshot(with_origin(ORIGIN_ALLOWED).expect("test request must build"))
            .await;
        let denied = stub_router(ApiListener::Internal)
            .oneshot(with_origin("https://attacker.example").expect("test request must build"))
            .await;
        assert!(matches!(
            allowed,
            Ok(response)
                if response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN)
                    == Some(&HeaderValue::from_static(ORIGIN_ALLOWED))
        ));
        assert!(matches!(
            denied,
            Ok(response) if response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none()
        ));
    }

    async fn json_reply(
        router: Router,
        request: std::result::Result<Request, axum::http::Error>,
    ) -> Option<(StatusCode, serde_json::Value)> {
        let response = router.oneshot(request.ok()?).await.ok()?;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .ok()?;
        let reply = serde_json::from_slice(bytes.as_ref()).ok()?;
        Some((status, reply))
    }

    /// The decoded body reaches the real dispatcher: an unauthenticated `nodeDid` on the
    /// external router answers with the node's DID, and a parse failure answers with the
    /// JSON-RPC parse error rather than an HTTP error.
    #[tokio::test]
    async fn external_router_dispatches_the_decoded_public_call() {
        let Some(security) = security() else {
            return;
        };
        let processor = Arc::new(prepare_processor().await);
        let did = processor.did().to_string();
        let router = external_router(processor, security);

        let served = json_reply(router.clone(), jsonrpc_request(call("nodeDid"))).await;
        assert_eq!(
            served
                .as_ref()
                .map(|(status, reply)| (*status, reply.pointer("/result/did"))),
            Some((StatusCode::OK, Some(&serde_json::Value::String(did))))
        );

        let malformed = json_reply(router, bearer(jsonrpc_request("not json".to_string()))).await;
        assert_eq!(
            malformed
                .as_ref()
                .map(|(status, reply)| (*status, reply.pointer("/error/code"))),
            Some((StatusCode::OK, Some(&serde_json::Value::from(-32700))))
        );
    }
}
