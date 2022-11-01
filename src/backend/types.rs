//! Backend Message.
use std::collections::HashMap;

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

/// Enum HttpServer with HttpServerMessage.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendMessage {
    HttpServer(HttpServerMessage),
}

/// Enum HttpServerRequest with Request, and HttpServerResponse with Response.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpServerMessage {
    Request(HttpServerRequest),
    Response(HttpServerResponse),
}

/// HttpServerRequest
/// - `method`: GET, POST, DELETE, PUT, OPTION, HEAD.
/// - `path`: URI and URL
/// - `headers`: HashMap contains like `Context-Type: application/json`.
/// - `body`:  Message chunk split into bytes and send to remote client.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpServerRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Option<Bytes>,
}

/// HttpServerResponse
/// - `status`: Status machine with numbers, like 200, 300, 400, 500.
/// - `headers`: HashMap contains like `Context-Type: application/json`.
/// - `body`:  Message chunk split into bytes and send back to remote client.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpServerResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Option<Bytes>,
}
