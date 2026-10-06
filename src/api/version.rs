//! `GET /api/version`: the running build, `{"version": "...", "git_sha": "..." | null}`.

use axum::Json;

use crate::build_info::{self, BuildInfo};

pub async fn get() -> Json<BuildInfo> {
    Json(build_info::info())
}
