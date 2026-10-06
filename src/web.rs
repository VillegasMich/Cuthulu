//! Server-rendered page shells and embedded static assets.

use std::fmt::Write as _;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::Embed;

use crate::model::ServiceDetail;
use crate::registry::RegistryError;
use crate::server::AppState;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Embed)]
#[folder = "static/"]
struct Assets;

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    version: &'static str,
    read_only: bool,
}

#[derive(Template)]
#[template(path = "service.html")]
struct ServicePage {
    version: &'static str,
    read_only: bool,
    d: ServiceDetail,
}

impl ServicePage {
    fn short_id(&self) -> &str {
        let native = self.d.service.id.native();
        &native[..native.len().min(12)]
    }
}

#[derive(Template)]
#[template(path = "not_found.html")]
struct NotFoundPage {
    version: &'static str,
    read_only: bool,
    what: String,
}

pub async fn index(State(st): State<AppState>) -> Response {
    render(&IndexPage {
        version: VERSION,
        read_only: st.config.read_only,
    })
}

pub async fn service(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let read_only = st.config.read_only;
    let missing = |what: String| {
        let page = NotFoundPage {
            version: VERSION,
            read_only,
            what,
        };
        (StatusCode::NOT_FOUND, render(&page)).into_response()
    };

    let Ok(parsed) = id.parse() else {
        return missing(format!("`{id}` is not a service id"));
    };
    match st.registry.detail(&parsed).await {
        Ok(d) => render(&ServicePage {
            version: VERSION,
            read_only,
            d,
        }),
        Err(RegistryError::NotFound(_)) => missing(format!("service `{id}` does not exist")),
        Err(e) => {
            tracing::error!(error = %e, "detail page failed");
            (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response()
        }
    }
}

pub async fn not_found(State(st): State<AppState>) -> Response {
    let page = NotFoundPage {
        version: VERSION,
        read_only: st.config.read_only,
        what: "nothing to see here".to_owned(),
    };
    (StatusCode::NOT_FOUND, render(&page)).into_response()
}

/// Serves embedded files with an `ETag` so browsers revalidate cheaply.
pub async fn asset(Path(path): Path<String>, headers: HeaderMap) -> Response {
    let Some(file) = Assets::get(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = file.metadata.sha256_hash()[..16]
        .iter()
        .fold(String::from("\""), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
        + "\"";
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|v| v.as_bytes() == etag.as_bytes())
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    (
        [
            (header::CONTENT_TYPE, file.metadata.mimetype().to_owned()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        file.data,
    )
        .into_response()
}

fn render(t: &impl Template) -> Response {
    match t.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "template render failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
