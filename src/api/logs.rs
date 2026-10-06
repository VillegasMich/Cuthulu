//! `GET /api/services/{id}/logs`: history then live lines, as SSE.
//!
//! Events: `lines` (JSON array of log lines, batched), `failure` (message,
//! stream ends) and `end` (the service stopped writing, e.g. it exited).
//! Every event carries non-empty data: browsers drop events whose data is empty.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::{self, Stream, StreamExt};
use serde::Deserialize;

use super::{ApiError, parse_id};
use crate::config::MAX_LOG_TAIL;
use crate::model::LogOptions;
use crate::server::AppState;

/// Lines arriving within [`BATCH_WINDOW`] are sent together, up to this many per event.
const BATCH: usize = 500;
const BATCH_WINDOW: Duration = Duration::from_millis(50);

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    tail: Option<usize>,
    follow: Option<bool>,
}

pub async fn stream(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let id = parse_id(&id)?;
    let opts = LogOptions {
        tail: query.tail.unwrap_or(st.config.log_tail).min(MAX_LOG_TAIL),
        follow: query.follow.unwrap_or(true),
    };
    let lines = st.registry.logs(&id, opts).await?;

    let events = tokio_stream::StreamExt::chunks_timeout(lines, BATCH, BATCH_WINDOW)
        .flat_map(|chunk| {
            let mut ok = Vec::with_capacity(chunk.len());
            let mut err = None;
            for item in chunk {
                match item {
                    Ok(line) => ok.push(line),
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            let lines = (!ok.is_empty()).then(|| {
                Event::default()
                    .event("lines")
                    .json_data(&ok)
                    .unwrap_or_else(|e| Event::default().event("failure").data(e.to_string()))
            });
            let error = err.map(|e| Event::default().event("failure").data(e.to_string()));
            stream::iter(lines.into_iter().chain(error))
        })
        .chain(stream::once(async {
            Event::default().event("end").data("end")
        }))
        .map(Ok)
        .take_until(st.shutdown.clone().cancelled_owned());

    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}
