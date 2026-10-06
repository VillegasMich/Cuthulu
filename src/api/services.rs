use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::Deserialize;

use super::{ApiError, guard, parse_id};
use crate::model::{Action, Service, ServiceDetail, ServiceState};
use crate::server::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    /// Case-insensitive match on name, image or group.
    q: Option<String>,
    state: Option<String>,
    group: Option<String>,
}

pub async fn list(
    State(st): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Json<Vec<Service>> {
    let q = query
        .q
        .as_deref()
        .map(str::to_lowercase)
        .filter(|q| !q.is_empty());
    let contains =
        |field: Option<&str>, q: &str| field.is_some_and(|f| f.to_lowercase().contains(q));

    let services = st
        .registry
        .snapshot()
        .into_iter()
        .filter(|s| {
            q.as_deref().is_none_or(|q| {
                contains(Some(&s.name), q)
                    || contains(s.image.as_deref(), q)
                    || contains(s.group.as_deref(), q)
            })
        })
        .filter(|s| {
            query
                .state
                .as_deref()
                .is_none_or(|want| state_name(s.state) == want)
        })
        .filter(|s| {
            query
                .group
                .as_deref()
                .is_none_or(|g| s.group.as_deref() == Some(g))
        })
        .collect();
    Json(services)
}

pub async fn detail(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ServiceDetail>, ApiError> {
    let id = parse_id(&id)?;
    Ok(Json(st.registry.detail(&id).await?))
}

pub async fn act(
    State(st): State<AppState>,
    Path((id, action)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Service>, ApiError> {
    guard::same_origin(&headers)?;
    let id = parse_id(&id)?;
    let action: Action = action
        .parse()
        .map_err(|()| ApiError::BadRequest(format!("unknown action `{action}`")))?;
    if st.config.read_only {
        return Err(ApiError::Forbidden(
            "cuthulu is running in read-only mode".to_owned(),
        ));
    }
    Ok(Json(st.registry.act(&id, action).await?))
}

fn state_name(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Running => "running",
        ServiceState::Restarting => "restarting",
        ServiceState::Paused => "paused",
        ServiceState::Created => "created",
        ServiceState::Stopped => "stopped",
        ServiceState::Dead => "dead",
        ServiceState::Unknown => "unknown",
    }
}
