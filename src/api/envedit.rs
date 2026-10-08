//! Env file editor: `POST /api/services/{id}/env/load` and `…/env/save`.
//!
//! Both carry the sudo password and the load returns secrets, so both are
//! POST with the same-origin check, refused in read-only mode, and rate
//! limited on wrong passwords (see [`crate::envedit`]).

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::Deserialize;

use super::{ApiError, guard, parse_id};
use crate::envedit::file::Var;
use crate::envedit::{EnvEditError, Loaded, Password, Saved};
use crate::model::Service;
use crate::server::AppState;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadRequest {
    password: Password,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveRequest {
    password: Password,
    /// From the load, to detect an edit made elsewhere in between.
    version: String,
    /// The complete new list of variables.
    vars: Vec<Var>,
}

impl From<EnvEditError> for ApiError {
    fn from(e: EnvEditError) -> Self {
        let message = e.to_string();
        match e {
            EnvEditError::NotEditable(_) => Self::NotFound(message),
            EnvEditError::RateLimited(wait) => Self::TooManyRequests {
                message,
                retry_after: wait.as_secs().max(1),
            },
            EnvEditError::WrongPassword => Self::Unauthorized(message),
            EnvEditError::NotSudoer(_) => Self::Forbidden(message),
            EnvEditError::BadPassword(_) | EnvEditError::Invalid(_) => Self::BadRequest(message),
            EnvEditError::Target(_)
            | EnvEditError::NoFile(_)
            | EnvEditError::TooLarge(_)
            | EnvEditError::FileDuplicate { .. }
            | EnvEditError::Stale(_) => Self::Conflict(message),
            EnvEditError::Host(_) => Self::Unavailable(message),
            EnvEditError::Failed(_) | EnvEditError::RestartFailed { .. } => Self::Internal(message),
        }
    }
}

pub async fn load(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<LoadRequest>, JsonRejection>,
) -> Result<Json<Loaded>, ApiError> {
    let service = target(&st, &id, &headers)?;
    let Json(req) = body.map_err(malformed)?;
    Ok(Json(st.env_edit.load(&service, &req.password).await?))
}

pub async fn save(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<SaveRequest>, JsonRejection>,
) -> Result<Json<Saved>, ApiError> {
    let service = target(&st, &id, &headers)?;
    let Json(req) = body.map_err(malformed)?;
    Ok(Json(
        st.env_edit
            .save(&service, &req.password, &req.version, &req.vars)
            .await?,
    ))
}

/// Same-origin and read-only checks, then the service.
fn target(st: &AppState, id: &str, headers: &HeaderMap) -> Result<Service, ApiError> {
    guard::same_origin(headers)?;
    if st.config.read_only {
        return Err(ApiError::Forbidden(
            "cuthulu is running in read-only mode".to_owned(),
        ));
    }
    let id = parse_id(id)?;
    st.registry
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("service `{id}` not found")))
}

/// serde's messages can quote the offending input (a password or a value).
#[allow(clippy::needless_pass_by_value)] // signature of map_err
fn malformed(_: JsonRejection) -> ApiError {
    ApiError::BadRequest("malformed request body".to_owned())
}
