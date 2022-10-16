use std::collections::HashMap;

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;

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

/// Response Chunk for support multipart response
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Chunk<const MTU: usize> {
    pub chunk: [usize; 2],
    /// from serde_json::to_vec
    pub data: Vec<u8>,
}


impl<const MTU: usize> Chunk<MTU> {
    pub fn from_bytes(bytes: Vec<u8>) -> Vec<Self> {
        let chunks: Vec<&[u8; MTU]> = bytes.array_chunks::<MTU>().collect();
        let chunks_len: usize = chunks.len();
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, d)| Chunk {
                chunk: [i, chunks_len],
                data: d.to_vec(),
            })
            .collect::<Vec<Chunk<MTU>>>()
    }
}

#[cfg(test)]
mod test {
    use core::iter::repeat;
    use std::collections::HashMap;

    use bytes::Bytes;

    use super::*;

    #[test]
    fn test_data_chunks() {
        let data: String = repeat("helloworld").take(1024).collect();
        let resp = HttpServerResponse {
            status: 200,
            headers: HashMap::new(),
            body: Some(Bytes::from(data)),
        };
        let resp_bytes = serde_json::to_vec(&resp).unwrap();
        let ret = Chunk::<32>::from_bytes(resp_bytes);
        assert_eq!(ret.len(), 10 * 1024 * 4 / 32 + 1);
        assert_eq!(ret[ret.len() - 1].chunk, [1280, 1281]);
    }
}
