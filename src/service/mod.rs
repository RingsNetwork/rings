#![warn(missing_docs)]
//! rings-node server
mod http_error;
#[cfg(feature = "daemon")]
mod is_turn;

use std::sync::Arc;

use axum::async_trait;
use axum::extract::Extension;
use axum::extract::FromRequest;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use http::header;
use http::header::HeaderValue;
#[cfg(feature = "daemon")]
pub use is_turn::run_udp_turn;
use jsonrpc_core::MetaIoHandler;
use tower_http::cors::CorsLayer;

use self::http_error::HttpError;
use crate::prelude::rings_core::dht::Stabilization;
use crate::prelude::rings_core::message::MessageHandler;
use crate::prelude::rings_core::swarm::Swarm;
use crate::processor::Processor;

/// Run a web server to handle jsonrpc request
pub async fn run_service(
    addr: String,
    swarm: Arc<Swarm>,
    msg_handler: Arc<MessageHandler>,
    stabilization: Arc<Stabilization>,
) -> anyhow::Result<()> {
    let binding_addr = addr.parse().unwrap();

    let swarm_layer = Extension(swarm.clone());
    let msg_handler_layer = Extension(msg_handler.clone());
    let stabilization_layer = Extension(stabilization.clone());

    let mut jsonrpc_handler: MetaIoHandler<Processor> = MetaIoHandler::default();
    crate::jsonrpc::build_handler(&mut jsonrpc_handler).await;
    let jsonrpc_handler_layer = Extension(Arc::new(jsonrpc_handler));

    let axum_make_service = Router::new()
        .route(
            "/",
            post(jsonrpc_io_handler)
                .layer(&swarm_layer)
                .layer(&msg_handler_layer)
                .layer(&stabilization_layer)
                .layer(&jsonrpc_handler_layer),
        )
        .layer(CorsLayer::permissive())
        .into_make_service();

    println!("Server listening on http://{}", addr);
    axum::Server::bind(&binding_addr)
        .serve(axum_make_service)
        .await?;
    Ok(())
}

#[async_trait]
impl<B> FromRequest<B> for crate::jsonrpc::AuthorityInfo
where B: Send
{
    type Rejection = JsonResponse;

    async fn from_request(
        req: &mut axum::extract::RequestParts<B>,
    ) -> Result<Self, Self::Rejection> {
        let Extension(swarm) = Extension::<Arc<Swarm>>::from_request(req)
            .await
            .expect("`Swarm` Extension is missing");
        let authed = if let Some(auth_value) = req.headers().get(reqwest::header::AUTHORIZATION) {
            Processor::verify_signature(auth_value.as_bytes(), &swarm.public_key()).unwrap_or(false)
        } else {
            false
        };
        Ok(authed.into())
    }
}

async fn jsonrpc_io_handler(
    body: String,
    authority_info: crate::jsonrpc::AuthorityInfo,
    Extension(swarm): Extension<Arc<Swarm>>,
    Extension(msg_handler): Extension<Arc<MessageHandler>>,
    Extension(stabilization): Extension<Arc<Stabilization>>,
    Extension(io_handler): Extension<Arc<MetaIoHandler<Processor>>>,
) -> Result<JsonResponse, HttpError> {
    log::debug!("authority_info: {:?}", authority_info);
    let r = io_handler
        .handle_request(&body, (swarm, msg_handler, stabilization).into())
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
