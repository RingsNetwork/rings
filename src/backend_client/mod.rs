use std::collections::HashMap;

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendMessage {
    HttpServer(HttpServerMessage),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpServerMessage {
    Request(HttpServerRequest),
    Response(HttpServerResponse),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpServerRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Option<Bytes>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpServerResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Option<Bytes>,
}
