#![warn(missing_docs)]
//! http server handler
use std::collections::HashMap;

use bytes::Bytes;

use super::ipfs::IpfsEndpoint;
use super::types::BackendMessage;
use super::types::HttpResponse;
use super::types::MessageEndpoint;
use crate::backend::types::send_chunk_report_message;
use crate::backend::types::HttpRequest;
use crate::backend::types::MessageType;
use crate::consts::BACKEND_MTU;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::chunk::ChunkList;
use crate::prelude::*;

/// HttpServer struct
/// * ipfs_endpoint
pub struct HttpServer {
    /// ipfs_endpoint to serve ipfs request
    pub ipfs_endpoint: Option<IpfsEndpoint>,
}

impl HttpServer {
    /// not allowd response
    pub fn not_allowed_resp() -> HttpResponse {
        HttpResponse {
            status: http::StatusCode::METHOD_NOT_ALLOWED.as_u16(),
            headers: HashMap::new(),
            body: None,
        }
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

        let uri = req
            .url
            .parse::<http::Uri>()
            .map_err(|_| Error::InvalidUrl)?;
        tracing::debug!("Sending HTTP request: {:?}", req);
        let schema = uri.scheme().ok_or(Error::InvalidUrl)?;
        let resp = if schema.as_str().eq_ignore_ascii_case("ipfs")
            || schema.as_str().eq_ignore_ascii_case("ipns")
        {
            if let Some(ipfs_endpoint) = self.ipfs_endpoint.as_ref() {
                ipfs_endpoint.execute(req).await?
            } else {
                HttpServer::not_allowed_resp()
            }
        } else {
            HttpServer::not_allowed_resp()
        };
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
            send_chunk_report_message(handler, ctx, relay, bytes.to_vec().as_slice()).await?;
        }
        Ok(())
    }
}
