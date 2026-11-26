use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;

/// HTTP error responses returned by native JSON-RPC and status route handlers.
#[derive(Debug)]
pub enum HttpError {
    /// Request was malformed or rejected as `400 Bad Request`.
    BadRequest,
    /// Server encountered an internal error (`500 Internal Server Error`).
    Internal,
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            HttpError::BadRequest => (StatusCode::BAD_REQUEST, "Bad Request"),
            HttpError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "Internal error"),
        };

        (code, msg).into_response()
    }
}

impl From<axum::http::Error> for HttpError {
    fn from(_: axum::http::Error) -> Self {
        HttpError::Internal
    }
}

impl From<anyhow::Error> for HttpError {
    fn from(_: anyhow::Error) -> Self {
        HttpError::Internal
    }
}
