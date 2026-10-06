//! `GET /api/tailscale`: where this machine lives in the Tailscale admin
//! console, for the topbar button. Always 200; `available: false` hides it.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};

use crate::server::AppState;

pub async fn link(State(st): State<AppState>) -> Response {
    Json(&*st.tailscale.link().await).into_response()
}
