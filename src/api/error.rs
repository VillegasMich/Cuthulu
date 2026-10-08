use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tracing::error;

use crate::providers::ProviderError;
use crate::registry::RegistryError;

/// Error returned by every JSON endpoint as `{ "error": "...", "code": "..." }`.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    /// Sent with `Retry-After` (seconds).
    #[error("{message}")]
    TooManyRequests { message: String, retry_after: u64 },
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}

impl ApiError {
    const fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Self::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            Self::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Self::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Self::TooManyRequests { .. } => (StatusCode::TOO_MANY_REQUESTS, "too_many_requests"),
            Self::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }
}

impl From<RegistryError> for ApiError {
    fn from(e: RegistryError) -> Self {
        match e {
            RegistryError::NotFound(_) => Self::NotFound(e.to_string()),
            RegistryError::Forbidden(msg) => Self::Forbidden(msg.to_owned()),
            RegistryError::Provider(ProviderError::NotFound(id)) => {
                Self::NotFound(format!("service `{id}` not found"))
            }
            RegistryError::Provider(e @ ProviderError::Unavailable(_)) => {
                Self::Unavailable(e.to_string())
            }
            RegistryError::Provider(e @ ProviderError::Backend(_)) => Self::Internal(e.to_string()),
        }
    }
}

#[derive(Serialize)]
struct Body<'a> {
    error: &'a str,
    code: &'static str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        if status.is_server_error() {
            error!(error = %self, "request failed");
        }
        let message = self.to_string();
        let mut res = (
            status,
            Json(Body {
                error: &message,
                code,
            }),
        )
            .into_response();
        if let Self::TooManyRequests { retry_after, .. } = self {
            res.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after));
        }
        res
    }
}
