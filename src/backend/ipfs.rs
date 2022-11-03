use std::sync::Arc;

use bytes::Bytes;
use http::method;

use super::types::BackendMessage;
use super::types::HttpRequest;
use super::types::HttpResponse;
use super::types::MessageEndpoint;
use crate::backend::types::send_chunk_report_message;
use crate::backend::types::MessageType;
use crate::consts::BACKEND_MTU;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::chunk::ChunkList;
use crate::prelude::*;

#[derive(Debug, Clone)]
pub struct IpfsEndpoint {
    client: Arc<reqwest::Client>,
    api_gateway: String,
}

impl IpfsEndpoint {
    pub fn new(api_gateway: &str) -> Self {
        Self {
            client: Arc::new(reqwest::Client::new()),
            api_gateway: api_gateway.to_owned(),
        }
    }

    /// execute ipfs reuqest
    /// support ipfs:// and ipns:// schema
    pub async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        // let request_uri = request.url.parse::<Uri>().map_err(|_| Error::InvalidUrl)?;
        if !request.url.starts_with("ipfs://") || !request.url.starts_with("ipns://") {
            return Err(Error::InvalidUrl);
        }
        // TODO fix this request_url
        let request_url = format!(
            "{}/{}",
            self.api_gateway,
            request.url.trim_start_matches("ipfs://"),
        );

        let resp = self
            .client
            .request(method::Method::GET, request_url.as_str())
            .timeout(request.timeout.into())
            .send()
            .await
            .map_err(|e| Error::HttpRequestError(e.to_string()))?;

        let status = resp.status().as_u16();

        let headers = resp
            .headers()
            .iter()
            .filter(|(key, _)| {
                [
                    http::header::CONTENT_TYPE,
                    http::header::DATE,
                    http::header::ETAG,
                ]
                .contains(key)
            })
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
impl MessageEndpoint for IpfsEndpoint {
    async fn handle_message(
        &self,
        handler: &MessageHandler,
        ctx: &MessagePayload<Message>,
        relay: &MessageRelay,
        msg: &BackendMessage,
    ) -> Result<()> {
        let req: HttpRequest =
            bincode::deserialize(msg.data.as_slice()).map_err(|_| Error::DeserializeError)?;

        tracing::debug!("Sending IPFS request: {:?}", req);
        let resp = self.execute(req).await?;
        tracing::debug!("Sending IPFS response: {:?}", resp);
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
            send_chunk_report_message(handler, ctx, relay, bytes.to_vec().as_slice()).await?;
        }
        Ok(())
    }
}
