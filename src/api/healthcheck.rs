//! `GET /api/healthcheck`: how Cuthulu's own heartbeat is doing, for the
//! topbar's healthchecks.io button. Always 200; `available: false` hides it.

use axum::Json;
use axum::extract::State;

use crate::notify::HealthcheckStatus;
use crate::server::AppState;

pub async fn status(State(st): State<AppState>) -> Json<HealthcheckStatus> {
    Json(st.notifier.healthcheck())
}
