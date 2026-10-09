use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::Deserialize;

use super::{ApiError, guard, parse_id};
use crate::model::{Action, Service, ServiceDetail, ServiceState};
use crate::server::AppState;
use crate::units::{Outcome, UnitError};

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
    let service = st
        .registry
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("service `{id}` not found")))?;
    if service.is_self && action == Action::Stop {
        return Err(ApiError::Forbidden("cuthulu cannot stop itself".to_owned()));
    }
    // Lets alerts say a stop came from here, and spot a service that
    // something else starts again.
    st.notifier.expect(&service.name, action == Action::Stop);

    match st.units.act(&service, action).await? {
        Some(Outcome::Done) => return Ok(Json(after_unit_action(&st, &service, action).await)),
        Some(Outcome::NotLoaded) if service.placeholder => {
            // Uninstalled since it was stopped here: nothing left to start.
            st.units.forget(&service.name).await;
            st.registry.release(&service.name);
            return Err(ApiError::Conflict(format!(
                "{} is no longer installed as a systemd unit",
                service.name
            )));
        }
        Some(Outcome::NotLoaded) | None => {}
    }
    Ok(Json(st.registry.act(&id, action).await?))
}

/// Updates the registry after systemd acted on `service` and returns the
/// service as known now. The container may be gone (`docker run --rm`) or
/// re-created under a new id.
async fn after_unit_action(st: &AppState, service: &Service, action: Action) -> Service {
    if action == Action::Stop {
        // Re-read first, so a container that is already gone shows the
        // placeholder right away.
        st.registry.reread(&service.id).await;
        if let Some(placeholder) = st.units.remember(service).await {
            st.registry.hold(placeholder);
        }
    } else {
        st.units.forget(&service.name).await;
        st.registry.release(&service.name);
        st.registry.reread(&service.id).await;
    }
    st.registry.current(service)
}

impl From<UnitError> for ApiError {
    fn from(e: UnitError) -> Self {
        let message = e.to_string();
        match e {
            UnitError::NoUser(_) => Self::Conflict(message),
            UnitError::Host(_) => Self::Unavailable(message),
            UnitError::Target(_) | UnitError::Failed(_) => Self::Internal(message),
        }
    }
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
