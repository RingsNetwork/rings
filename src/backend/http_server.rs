#![warn(missing_docs)]
//! http server handler
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::chunk::ChunkList;
use serde::Deserialize;
use serde::Serialize;

use super::types::BackendMessage;
use super::types::HttpResponse;
use super::types::MessageEndpoint;
use crate::backend::types::HttpRequest;
use crate::backend::types::MessageType;
use crate::consts::BACKEND_MTU;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::*;

/// HTTP Server Config, specific determine port.
#[derive(Deserialize, Serialize, Debug)]
pub struct HttpServerConfig {
    /// port of service
    pub port: u16,
}

/// HttpServer struct
/// * ipfs_endpoint
pub struct HttpServer {
    /// http client
    pub client: Arc<reqwest::Client>,
    /// ipfs_endpoint to serve ipfs request
    pub port: u16,
}

impl From<HttpServerConfig> for HttpServer {
    fn from(config: HttpServerConfig) -> Self {
        Self::new(config.port)
    }
}

impl HttpServer {
    /// new HttpServer from port
    pub fn new(port: u16) -> Self {
        Self {
            client: Arc::new(reqwest::Client::new()),
            port,
        }
    }

    /// not allowd response
    pub fn not_allowed_resp() -> HttpResponse {
        HttpResponse {
            status: http::StatusCode::METHOD_NOT_ALLOWED.as_u16(),
            headers: HashMap::new(),
            body: None,
        }
    }

    /// execute http request
    pub async fn execute(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let url = format!(
            "http://localhost:{}/{}",
            self.port,
            request.path.trim_start_matches('/')
        );

        let request_url = url.parse::<http::Uri>().unwrap();

        let resp = self
            .client
            .request(
                http::Method::from_str(request.method.as_str()).unwrap(),
                request_url.to_string(),
            )
            .headers((&request.headers).try_into().unwrap())
            .timeout(request.timeout.clone().into())
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
        handler: &MessageHandler,
        ctx: &MessagePayload<Message>,
        relay: &MessageRelay,
        msg: &BackendMessage,
    ) -> Result<()> {
        let req: HttpRequest =
            bincode::deserialize(msg.data.as_slice()).map_err(|_| Error::DeserializeError)?;

        let resp = self.execute(&req).await?;
        tracing::debug!("Sending HTTP response: {:?}", resp);
        tracing::debug!("resp_bytes start gzip");
        let json_bytes = bincode::serialize(&resp)
            .map_err(|_| Error::JsonSerializeError)?
            .into();
        let resp_bytes =
            message::encode_data_gzip(&json_bytes, 9).map_err(|_| Error::EncodedError)?;

        let resp_bytes: Bytes =
            BackendMessage::new(MessageType::HttpResponse, resp_bytes.to_vec().as_slice()).into();
        tracing::debug!("resp_bytes gzip_data len: {}", resp_bytes.len());

        let chunks = ChunkList::<BACKEND_MTU>::from(&resp_bytes);
        for c in chunks {
            tracing::debug!("Chunk data len: {}", c.data.len());
            let bytes = c.to_bincode().map_err(|_| Error::SerializeError)?;
            tracing::debug!("Chunk len: {}", bytes.len());
            super::types::send_chunk_report_message(handler, ctx, relay, bytes.to_vec().as_slice())
                .await?;
        }
        Ok(())
    }
}
