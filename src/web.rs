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

    /// The image reference split for coloring; see [`split_image`].
    fn image_parts(&self) -> Option<(&str, &str)> {
        self.d.service.image.as_deref().map(split_image)
    }
}

/// Splits an image reference into repository and tag/digest, e.g.
/// `ghcr.io:443/a/b:1.2@sha256:…` → (`ghcr.io:443/a/b`, `:1.2@sha256:…`).
/// The tag part is empty when there is none. Mirrors `imageParts` in `app.js`.
fn split_image(image: &str) -> (&str, &str) {
    let name_start = image.rfind('/').map_or(0, |i| i + 1);
    let cut = [
        image.find('@'),
        image[name_start..].find(':').map(|i| name_start + i),
    ]
    .into_iter()
    .flatten()
    .min()
    .filter(|&i| i > 0)
    .unwrap_or(image.len());
    image.split_at(cut)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PortMapping, ServiceState};
    use crate::registry::tests::service;

    #[test]
    fn splits_image_into_repo_and_tag() {
        assert_eq!(split_image("alpine"), ("alpine", ""));
        assert_eq!(split_image("alpine:3.20"), ("alpine", ":3.20"));
        assert_eq!(
            split_image("villegasmich/tool:0.2.0"),
            ("villegasmich/tool", ":0.2.0")
        );
        assert_eq!(
            split_image("reg.local:5000/a/b"),
            ("reg.local:5000/a/b", "")
        );
        assert_eq!(
            split_image("reg.local:5000/a/b:1"),
            ("reg.local:5000/a/b", ":1")
        );
        assert_eq!(split_image("a/b@sha256:abc"), ("a/b", "@sha256:abc"));
        assert_eq!(split_image("a/b:1@sha256:abc"), ("a/b", ":1@sha256:abc"));
        assert_eq!(split_image(":odd"), (":odd", ""));
        assert_eq!(split_image(""), ("", ""));
    }

    #[test]
    fn service_page_colors_tag_project_and_dims_port_host() {
        let mut svc = service("web", ServiceState::Running);
        svc.image = Some("nginx:1.27".into());
        svc.group = Some("shop".into());
        svc.ports = vec![
            PortMapping {
                container_port: 80,
                protocol: "tcp".into(),
                host_ip: Some("127.0.0.1".into()),
                host_port: Some(8080),
            },
            PortMapping {
                container_port: 53,
                protocol: "udp".into(),
                host_ip: None,
                host_port: None,
            },
        ];
        let page = ServicePage {
            version: VERSION,
            read_only: false,
            d: ServiceDetail {
                service: svc,
                command: None,
                created_at: None,
                restart_policy: None,
                restart_count: 0,
                error: None,
                mounts: vec![],
                networks: vec![],
                env_keys: vec![],
                labels: std::collections::BTreeMap::new(),
            },
        };
        let html = page.render().unwrap();
        assert!(
            html.contains(r#"nginx<span class="img-tag">:1.27</span>"#),
            "{html}"
        );
        assert!(html.contains(r#"<dd class="project">shop</dd>"#));
        assert!(html.contains(
            r#"<span class="muted">127.0.0.1:</span>8080->80<span class="muted">/tcp</span>"#
        ));
        assert!(html.contains(r#"<div>53<span class="muted">/udp</span></div>"#));
    }
}
