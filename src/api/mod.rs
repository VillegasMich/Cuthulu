//! JSON and Server-Sent Events API under `/api`.

mod error;
mod events;
mod guard;
mod logs;
mod services;
mod system;
mod todos;

use axum::Router;
use axum::routing::{get, post};

pub use error::ApiError;
pub use guard::HEADER as CSRF_HEADER;

use crate::model::ServiceId;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/services", get(services::list))
        .route("/services/{id}", get(services::detail))
        .route("/services/{id}/logs", get(logs::stream))
        .route("/services/{id}/{action}", post(services::act))
        .route("/services/{id}/todos", get(todos::list).post(todos::create))
        .route("/services/{id}/todos/{todo_id}/toggle", post(todos::toggle))
        .route("/services/{id}/todos/{todo_id}/delete", post(todos::delete))
        .route("/events", get(events::stream))
        .route("/system", get(system::snapshot))
        .route("/system/stream", get(system::stream))
}

fn parse_id(raw: &str) -> Result<ServiceId, ApiError> {
    raw.parse()
        .map_err(|e: crate::model::InvalidServiceId| ApiError::BadRequest(e.to_string()))
}
