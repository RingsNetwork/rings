use std::sync::Arc;

use http::HeaderMap;

use crate::backend_client::HttpServerRequest;
use crate::backend_client::HttpServerResponse;
use crate::error::Error;
use crate::error::Result;
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

    pub async fn execute(&self, request: HttpServerRequest) -> Result<HttpServerResponse> {

        let request_url = format!(
            "{}/{}",
            self.api_gateway,
            request.path.trim_start_matches('/'),
        );


        let mut headers = HeaderMap::new();
        headers.insert("host", val);

        for (name, value) in request.headers {
            headers.insert(
                name.parse::<HeaderName>().map_err(|_| {
                    Error::HttpRequestError(format!("Invalid header name: {}", &name))
                })?,
                value.parse().map_err(|_| {
                    Error::HttpRequestError(format!("Invalid header value: {}", &value))
                })?,
            );
        }

        let req = self
            .client
            .request(method, &url)
            .headers(headers)
            .timeout(std::time::Duration::from_secs(15));
        let req = request
            .body
            .map_or(req.try_clone().unwrap(), |body| req.body(body));
        let resp = req
            .send()
            .await
            .map_err(|e| Error::HttpRequestError(e.to_string()))?;

        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_str().unwrap().to_string()))
            .collect();
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::HttpRequestError(e.to_string()))?;

        Ok(HttpServerResponse {
            status,
            headers,
            body: Some(body),
        })
    }
}
