#![warn(missing_docs)]
//! http server handler
use std::str::FromStr;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::chunk::ChunkList;
use serde::Deserialize;
use serde::Serialize;

use super::backend::types::BackendMessage;
use super::backend::types::HttpResponse;
use super::backend::MessageEndpoint;
use super::backend::MessageType;
use crate::consts::BACKEND_MTU;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_rpc::types::HttpRequest;
use crate::prelude::*;

/// HTTP Server Config, specific determine port.
#[derive(Deserialize, Clone, Serialize, Debug, PartialEq, Eq)]
pub struct HiddenServerConfig {
    /// name of hidden service
    pub name: String,

    /// will register to storage if provided
    pub register_service: Option<String>,

    /// prefix of hidden service
    pub prefix: String,
}

/// HttpServer struct
/// * ipfs_endpoint
pub struct HttpServer {
    /// http client
    pub client: Arc<reqwest::Client>,

    /// hidden services
    pub services: Vec<HiddenServerConfig>,
}

impl Default for HttpServer {
    fn default() -> Self {
        Self {
            client: Arc::new(reqwest::Client::new()),
            services: Default::default(),
        }
    }
}

impl From<Vec<HiddenServerConfig>> for HttpServer {
    fn from(configs: Vec<HiddenServerConfig>) -> Self {
        Self {
            client: Arc::new(reqwest::Client::new()),
            services: configs,
        }
    }
}

impl HttpServer {
    /// execute http request
    pub async fn execute(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let service = self
            .services
            .iter()
            .find(|x| x.name.eq_ignore_ascii_case(request.name.as_str()))
            .ok_or(Error::InvalidService)?;

        let url = format!(
            "{}/{}",
            service.prefix,
            request.path.trim_start_matches('/')
        );

        let request_url = url.parse::<http::Uri>().unwrap();

        let request_method =
            http::Method::from_str(request.method.as_str()).map_err(|_| Error::InvalidMethod)?;

        let headers = (&request.headers).try_into().map_err(|e| {
            tracing::info!("invalid_headers: {}", e);
            Error::InvalidHeaders
        })?;

        let request_builder = self
            .client
            .request(request_method, request_url.to_string())
            .headers(headers)
            .timeout(request.timeout.clone().into());

        let request_builder = if let Some(body) = request.body.as_ref() {
            let body = body.to_vec();
            request_builder.body(body)
        } else {
            request_builder
        };

        let resp = request_builder
            .send()
            .await
            .map_err(|e| Error::HttpRequestError(e.to_string()))?;

        let status = resp.status().as_u16();

        let headers = resp
            .headers()
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_str().unwrap_or("").to_owned()))
            .collect();

        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::HttpRequestError(e.to_string()))?;

        Ok(HttpResponse {
            status,
            headers,
            body: Some(body),
        })
    }
}

#[async_trait::async_trait]
impl MessageEndpoint for HttpServer {
    async fn handle_message(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &BackendMessage,
    ) -> Result<Vec<MessageHandlerEvent>> {
        let req: HttpRequest = bincode::deserialize(&msg.data).map_err(|_| Error::DecodeError)?;

        let resp = self.execute(&req).await?;
        tracing::debug!("Sending HTTP response: {:?}", resp);
        tracing::debug!("resp_bytes start gzip");
        let json_bytes = bincode::serialize(&resp)
            .map_err(|_| Error::EncodeError)?
            .into();
        let resp_bytes =
            message::encode_data_gzip(&json_bytes, 9).map_err(|_| Error::EncodeError)?;

        let resp_bytes: Bytes = BackendMessage::from((
            MessageType::HttpResponse.into(),
            resp_bytes.to_vec().as_slice(),
        ))
        .into();
        tracing::debug!("resp_bytes gzip_data len: {}", resp_bytes.len());

        let chunks = ChunkList::<BACKEND_MTU>::from(&resp_bytes);
        let mut events = vec![];

        for c in chunks {
            tracing::debug!("Chunk data len: {}", c.data.len());
            let bytes = c.to_bincode().map_err(|_| Error::EncodeError)?;
            tracing::debug!("Chunk len: {}", bytes.len());
            let ev = super::utils::send_chunk_report_message(&ctx, bytes.to_vec().as_slice()).await?;
            events.push(ev);
        }

        Ok(events)
    }
}
