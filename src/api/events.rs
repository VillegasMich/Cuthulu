//! `GET /api/events`: a snapshot on connect, then every registry change.

use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::future;
use futures_util::stream::{self, Stream, StreamExt};
use serde::Serialize;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::model::Service;
use crate::registry::{ProviderStatus, RegistryEvent};
use crate::server::AppState;

#[derive(Serialize)]
struct Snapshot {
    services: Vec<Service>,
    status: Vec<ProviderStatus>,
}

pub async fn stream(
    State(st): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe before taking the snapshot so no change falls in between.
    let updates = BroadcastStream::new(st.registry.subscribe());
    let snapshot = Snapshot {
        services: st.registry.snapshot(),
        status: st.registry.statuses(),
    };

    let first = stream::once(async move { json_event("snapshot", &snapshot) });
    // A client that falls too far behind gets one `resync` event and the
    // stream ends; it reconnects and receives a fresh snapshot.
    let rest = updates.scan(false, |lagged, res| {
        let ev = match res {
            _ if *lagged => None,
            Ok(RegistryEvent::Upsert(s)) => Some(json_event("upsert", &s)),
            Ok(RegistryEvent::Remove(id)) => Some(json_event("remove", &id)),
            Ok(RegistryEvent::Status(s)) => Some(json_event("status", &s)),
            Err(BroadcastStreamRecvError::Lagged(_)) => {
                *lagged = true;
                Some(Event::default().event("resync").data("resync"))
            }
        };
        future::ready(ev)
    });

    let events = first
        .chain(rest)
        .map(Ok)
        .take_until(st.shutdown.clone().cancelled_owned());

    Sse::new(events).keep_alive(KeepAlive::default())
}

fn json_event(name: &str, data: &impl Serialize) -> Event {
    Event::default()
        .event(name)
        .json_data(data)
        .unwrap_or_else(|e| Event::default().event("failure").data(e.to_string()))
}
