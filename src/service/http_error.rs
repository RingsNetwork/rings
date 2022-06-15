use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;

#[derive(Debug)]
pub enum HttpError {
    BadRequest,
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

impl From<hyper::Error> for HttpError {
    fn from(_: hyper::Error) -> Self {
        HttpError::Internal
    }
}

impl From<hyper::http::Error> for HttpError {
    fn from(_: hyper::http::Error) -> Self {
        HttpError::Internal
    }
}

impl From<anyhow::Error> for HttpError {
    fn from(_: anyhow::Error) -> Self {
        HttpError::Internal
    }
}
