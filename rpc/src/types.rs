use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

/// Timeout in milliseconds.
#[derive(Deserialize, Debug, Serialize, Clone)]
pub struct Timeout(u64);

impl Default for Timeout {
    fn default() -> Self {
        Timeout(30 * 1000)
    }
}

impl From<Timeout> for Duration {
    fn from(val: Timeout) -> Self {
        Duration::from_millis(val.0)
    }
}

impl From<u64> for Timeout {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

fn default_http_request_body() -> Option<Vec<u8>> {
    None
}

/// HttpRequest
/// - `method`: request methods
///    * GET
///    * POST
///    * PUT
///    * DELETE
///    * OPTION
///    * HEAD
///    * TRACE
///    * CONNECT
/// - `path`: hidden service path
/// - `timeout`: timeout in milliseconds
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpRequest {
    /// service name
    pub name: String,
    /// method
    pub method: String,
    /// url
    pub path: String,
    /// timeout
    #[serde(default)]
    pub timeout: Timeout,
    /// headers
    pub headers: HashMap<String, String>,
    /// body
    #[serde(default = "default_http_request_body")]
    pub body: Option<Vec<u8>>,
}

impl HttpRequest {
    /// new HttpRequest
    /// - `name`
    /// - `method`
    /// - `url`
    /// - `timeout`
    /// - `headers`
    /// - `body`: optional
    pub fn new<U>(
        name: U,
        method: http::Method,
        path: U,
        timeout: Timeout,
        headers: &[(U, U)],
        body: Option<Vec<u8>>,
    ) -> Self
    where
        U: ToString,
    {
        Self {
            name: name.to_string(),
            method: method.to_string(),
            path: path.to_string(),
            timeout,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body,
        }
    }

    /// new `GET` HttpRequest
    /// - `name`
    /// - `method`
    /// - `url`
    /// - `timeout`
    /// - `headers`
    /// - `body`: optional
    pub fn get<U>(
        name: U,
        url: U,
        timeout: Timeout,
        headers: &[(U, U)],
        body: Option<Vec<u8>>,
    ) -> Self
    where
        U: ToString,
    {
        Self::new(name, http::Method::GET, url, timeout, headers, body)
    }
}
