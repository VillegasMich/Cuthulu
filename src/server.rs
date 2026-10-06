//! HTTP application: shared state, router and security headers.

use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderName, HeaderValue, header};
use axum::middleware;
use axum::response::Response;
use axum::routing::get;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::registry::Registry;
use crate::{api, web};

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<Registry>,
    pub config: Arc<Config>,
    /// Fires on shutdown so long-lived streams end and the server can exit.
    pub shutdown: CancellationToken,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(web::index))
        .route("/services/{id}", get(web::service))
        .route("/static/{*path}", get(web::asset))
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", api::router())
        .fallback(web::not_found)
        .layer(middleware::map_response(security_headers))
        .with_state(state)
}

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; \
                   connect-src 'self'; font-src 'self'; frame-ancestors 'none'; base-uri 'none'; \
                   form-action 'none'";

async fn security_headers(mut res: Response) -> Response {
    let headers = res.headers_mut();
    let set = [
        (header::CONTENT_SECURITY_POLICY, CSP),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (
            HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
    ];
    for (name, value) in set {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    res
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::model::{ProviderKind, Service, ServiceState};
    use crate::registry::tests::{MockProvider, service};

    async fn app_with(services: Vec<Service>, read_only: bool) -> (Router, Arc<Registry>) {
        let provider = Arc::new(MockProvider::with(services));
        let registry = Registry::new(vec![provider]);
        // Run the watch loop once so the registry holds the mock's services.
        let shutdown = CancellationToken::new();
        let mut sub = registry.subscribe();
        registry.spawn(std::time::Duration::from_secs(3600), &shutdown);
        while !matches!(
            sub.recv().await,
            Ok(crate::registry::RegistryEvent::Status(_))
        ) {}

        let config = Config {
            read_only,
            ..Config::default()
        };
        let app = router(AppState {
            registry: Arc::clone(&registry),
            config: Arc::new(config),
            shutdown,
        });
        (app, registry)
    }

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
        let res = app.clone().oneshot(req).await.unwrap();
        let (parts, body) = res.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    fn get(uri: &str) -> Request<Body> {
        Request::get(uri).body(Body::empty()).unwrap()
    }

    fn post(uri: &str) -> Request<Body> {
        Request::post(uri)
            .header(api::CSRF_HEADER, "1")
            .body(Body::empty())
            .unwrap()
    }

    fn web() -> Service {
        service("web", ServiceState::Running)
    }

    #[tokio::test]
    async fn healthz() {
        let (app, _) = app_with(vec![], false).await;
        let (status, _, body) = send(&app, get("/healthz")).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "ok"));
    }

    #[tokio::test]
    async fn lists_and_filters_services() {
        let (app, _) = app_with(vec![web(), service("db", ServiceState::Stopped)], false).await;

        let (status, _, body) = send(&app, get("/api/services")).await;
        assert_eq!(status, StatusCode::OK);
        let list: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["name"], "web", "running first");

        let (_, _, body) = send(&app, get("/api/services?state=stopped")).await;
        let list: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "db");

        let (_, _, body) = send(&app, get("/api/services?q=WE")).await;
        let list: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn actions_need_the_csrf_header() {
        let (app, registry) = app_with(vec![web()], false).await;
        let id = web().id;

        let bare = Request::post(format!("/api/services/{id}/stop"))
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = send(&app, bare).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(registry.get(&id).unwrap().state, ServiceState::Running);

        let (status, _, body) = send(&app, post(&format!("/api/services/{id}/stop"))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let after: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(after["state"], "stopped");
    }

    #[tokio::test]
    async fn read_only_blocks_actions() {
        let (app, _) = app_with(vec![web()], true).await;
        let (status, _, _) = send(&app, post(&format!("/api/services/{}/start", web().id))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn cannot_stop_itself() {
        let mut me = service("me", ServiceState::Running);
        me.is_self = true;
        let (app, _) = app_with(vec![me.clone()], false).await;
        let (status, _, body) = send(&app, post(&format!("/api/services/{}/stop", me.id))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("forbidden"));
    }

    #[tokio::test]
    async fn bad_input_is_rejected() {
        let (app, _) = app_with(vec![web()], false).await;
        let (status, _, body) = send(&app, get("/api/services/not-an-id")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("\"code\":\"bad_request\""));

        let (status, _, _) = send(&app, post(&format!("/api/services/{}/explode", web().id))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let ghost = crate::model::ServiceId::new(ProviderKind::Docker, "ghost");
        let (status, _, _) = send(&app, post(&format!("/api/services/{ghost}/start"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pages_render_with_security_headers() {
        let (app, _) = app_with(vec![web()], false).await;
        let (status, headers, body) = send(&app, get("/")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>cuthulu</title>"));
        assert!(
            headers[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("default-src 'self'")
        );
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");

        let (status, _, body) = send(&app, get("/services/docker:ghost")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("does not exist"));

        let (status, _, _) = send(&app, get("/nope")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn topbar_has_icon_controls_and_dashboard_hides_stopped_toggle() {
        let (app, _) = app_with(vec![web()], false).await;
        let (_, _, index) = send(&app, get("/")).await;
        assert!(index.contains(r#"id="back""#) && index.contains(r#"aria-label="back""#));
        assert!(index.contains(r#"aria-label="toggle theme (t)""#));
        assert!(index.contains(r#"class="ico sun""#) && index.contains(r#"class="ico moon""#));
        assert!(index.contains(r#"id="show-stopped""#));
        assert!(!index.contains("state-filter"));

        // Every page shell gets the back arrow; only the dashboard has the toggle.
        let (_, _, other) = send(&app, get("/nope")).await;
        assert!(other.contains(r#"id="back""#));
        assert!(!other.contains(r#"id="show-stopped""#));
    }

    #[tokio::test]
    async fn serves_assets_with_etag() {
        let (app, _) = app_with(vec![], false).await;
        let (status, headers, _) = send(&app, get("/static/app.css")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/css");
        let etag = headers[header::ETAG].clone();

        let cached = Request::get("/static/app.css")
            .header(header::IF_NONE_MATCH, etag)
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, cached).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);

        let (status, _, _) = send(&app, get("/static/../Cargo.toml")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn event_stream_starts_with_a_snapshot() {
        let (app, _) = app_with(vec![web()], false).await;
        let res = app.oneshot(get("/api/events")).await.unwrap();
        assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
        let mut body = res.into_body();
        let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
        let text = String::from_utf8_lossy(&frame);
        assert!(text.starts_with("event: snapshot\n"), "{text}");
        assert!(text.contains("\"name\":\"web\""));
    }
}
