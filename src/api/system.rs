//! Host system panel: `GET /api/system` (one snapshot) and
//! `GET /api/system/stream` (SSE, a `system` event per sample).
//!
//! A dedicated stream rather than an event on `/api/events`: the sampler runs
//! only while someone holds this stream open, so pages without the panel (or
//! with it collapsed) cost nothing. Events: `system` (snapshot JSON) and
//! `failure` (message; the stream stays open and recovers). Both always carry
//! non-empty data.

use std::convert::Infallible;

use axum::Json;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::future;
use futures_util::stream::{self, Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;

use super::ApiError;
use crate::server::AppState;
use crate::system::Update;

pub async fn snapshot(State(st): State<AppState>) -> Result<Response, ApiError> {
    let snap = st
        .system
        .snapshot()
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    Ok(Json(&*snap).into_response())
}

pub async fn stream(
    State(st): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe first (starts the sampler), then replay the latest sample so
    // a new client does not wait a full interval for its first frame.
    let updates = BroadcastStream::new(st.system.subscribe());
    let first = stream::iter(st.system.latest().map(|u| event(&u)));
    // Every update is a full snapshot: a lagging client just skips some.
    let rest = updates.filter_map(|res| future::ready(res.ok().map(|u| event(&u))));

    let events = first
        .chain(rest)
        .map(Ok)
        .take_until(st.shutdown.clone().cancelled_owned());
    Sse::new(events).keep_alive(KeepAlive::default())
}

fn event(update: &Update) -> Event {
    match update {
        Ok(snap) => Event::default()
            .event("system")
            .json_data(&**snap)
            .unwrap_or_else(|e| Event::default().event("failure").data(e.to_string())),
        Err(e) => Event::default().event("failure").data(e.to_string()),
    }
}
