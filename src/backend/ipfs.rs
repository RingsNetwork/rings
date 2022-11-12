#![warn(missing_docs)]
//! ipfs endpoint
//! handle ipfs reuqest
//! support schemas:
//! * ipfs://
//! * ipns://
use std::sync::Arc;

use http::method;

use super::types::HttpRequest;
use super::types::HttpResponse;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::*;

/// ipfs endpoint struct
/// * client: http client
/// * ipfs api gateway url
#[derive(Debug, Clone)]
pub struct IpfsEndpoint {
    client: Arc<reqwest::Client>,
    api_gateway: String,
}

impl IpfsEndpoint {
    /// new IpfsEndpoint by ipfs api_gateway
    pub fn new(api_gateway: &str) -> Self {
        Self {
            client: Arc::new(reqwest::Client::new()),
            api_gateway: api_gateway.to_owned(),
        }
    }

    /// execute ipfs reuqest
    /// support ipfs:// and ipns:// schema
    pub async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        let uri = request.url.parse::<http::Uri>().unwrap();
        let base_uri = self.api_gateway.parse::<http::Uri>().unwrap();
        let request_url = http::Uri::builder()
            .scheme(base_uri.scheme_str().unwrap())
            .authority(base_uri.host().unwrap())
            .path_and_query(uri.path_and_query().unwrap().as_str())
            .build()
            .unwrap();

        let resp = self
            .client
            .request(method::Method::GET, request_url.to_string())
            .headers((&request.headers).try_into().unwrap())
            .timeout(request.timeout.into())
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
