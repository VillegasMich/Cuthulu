//! `RustDesk` on the host: `GET /api/rustdesk` for the topbar button and its
//! panel (always 200; `available: false` disables it), and `POST
//! /api/rustdesk/password`, which sets a new random permanent password after
//! the host user's sudo password is checked and returns it once.
//!
//! The POST carries the sudo password and returns a secret, so it is
//! same-origin checked, refused in read-only mode, rate limited with the env
//! editor, and its answer is never cached.

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{ApiError, guard};
use crate::envedit::Password;
use crate::rustdesk::RustDeskError;
use crate::server::AppState;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetPasswordRequest {
    /// The host user's sudo password, not the new `RustDesk` one.
    password: Password,
}

impl From<RustDeskError> for ApiError {
    fn from(e: RustDeskError) -> Self {
        let message = e.to_string();
        match e {
            RustDeskError::Unsupported(_) | RustDeskError::NotInstalled => Self::Conflict(message),
            RustDeskError::RateLimited(wait) => Self::TooManyRequests {
                message,
                retry_after: wait.as_secs().max(1),
            },
            RustDeskError::BadPassword(_) => Self::BadRequest(message),
            RustDeskError::WrongPassword => Self::Unauthorized(message),
            RustDeskError::NotSudoer(_) => Self::Forbidden(message),
            RustDeskError::Host(_) => Self::Unavailable(message),
            RustDeskError::Random(_) | RustDeskError::Failed(_) => Self::Internal(message),
        }
    }
}

pub async fn status(State(st): State<AppState>) -> Response {
    Json(st.rustdesk.status().await).into_response()
}

pub async fn set_password(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SetPasswordRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    guard::same_origin(&headers)?;
    if st.config.read_only {
        return Err(ApiError::Forbidden(
            "cuthulu is running in read-only mode".to_owned(),
        ));
    }
    // serde's messages can quote the offending input (the password).
    let Json(req) = body.map_err(|_| ApiError::BadRequest("malformed request body".to_owned()))?;
    let set = st.rustdesk.set_password(&req.password).await?;
    let mut res = Json(set).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}
