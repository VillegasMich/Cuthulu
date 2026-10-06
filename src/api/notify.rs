//! Notification settings: `GET|POST /api/notify`, `POST /api/notify/test`
//! and `POST /api/services/{id}/notify`.
//!
//! Watching is stored by service name, so the `{id}` must belong to a
//! service the registry currently knows. Every route returns the full
//! [`NotifyState`] (except the test, which reports per channel).

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::Deserialize;

use super::{ApiError, guard, parse_id};
use crate::notify::{NotifyState, StoreError, TestReport};
use crate::server::AppState;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetEnabled {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetWatch {
    watch: bool,
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Invalid(msg) => Self::BadRequest(msg),
            StoreError::Storage(msg) => Self::Unavailable(msg),
        }
    }
}

pub async fn state(State(st): State<AppState>) -> Result<Json<NotifyState>, ApiError> {
    Ok(Json(st.notifier.state()?))
}

pub async fn set_enabled(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SetEnabled>, JsonRejection>,
) -> Result<Json<NotifyState>, ApiError> {
    writable(&st, &headers)?;
    let Json(req) = body.map_err(|e| ApiError::BadRequest(e.body_text()))?;
    Ok(Json(st.notifier.set_enabled(req.enabled).await?))
}

pub async fn set_watch(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<SetWatch>, JsonRejection>,
) -> Result<Json<NotifyState>, ApiError> {
    writable(&st, &headers)?;
    let id = parse_id(&id)?;
    let service = st
        .registry
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("service `{id}` not found")))?;
    let Json(req) = body.map_err(|e| ApiError::BadRequest(e.body_text()))?;
    Ok(Json(
        st.notifier.set_watched(&service.name, req.watch).await?,
    ))
}

pub async fn test(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<TestReport>, ApiError> {
    writable(&st, &headers)?;
    Ok(Json(st.notifier.test().await))
}

/// Same-origin and read-only checks for a write.
fn writable(st: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    guard::same_origin(headers)?;
    if st.config.read_only {
        return Err(ApiError::Forbidden(
            "cuthulu is running in read-only mode".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use crate::api::CSRF_HEADER;
    use crate::config::Config;
    use crate::model::{ProviderKind, ServiceId, ServiceState};
    use crate::notify::Notifier;
    use crate::registry::tests::{MockProvider, service};
    use crate::registry::{Registry, RegistryEvent};
    use crate::server::{AppState, router};
    use crate::system::SystemMonitor;
    use crate::todos::TodoStore;
    use crate::todos::tests::TempDir;

    struct App {
        router: Router,
        dir: TempDir,
    }

    async fn app_in(read_only: bool, dir: TempDir, config: Config) -> App {
        let registry = Registry::new(vec![Arc::new(MockProvider::with(vec![service(
            "web",
            ServiceState::Running,
        )]))]);
        let shutdown = CancellationToken::new();
        let mut sub = registry.subscribe();
        registry.spawn(std::time::Duration::from_secs(3600), &shutdown);
        while !matches!(sub.recv().await, Ok(RegistryEvent::Status(_))) {}

        let config = Config {
            read_only,
            data_dir: dir.path().to_owned(),
            ..config
        };
        let router = router(AppState {
            registry,
            system: SystemMonitor::new(&config, shutdown.clone()),
            todos: Arc::new(TodoStore::open(dir.path())),
            notifier: Notifier::new(&config),
            config: Arc::new(config),
            shutdown,
        });
        App { router, dir }
    }

    async fn app(read_only: bool) -> App {
        app_in(read_only, TempDir::new(), Config::default()).await
    }

    fn web_url() -> String {
        format!(
            "/api/services/{}/notify",
            service("web", ServiceState::Running).id
        )
    }

    async fn send(app: &App, req: Request<Body>) -> (StatusCode, Value) {
        let res = app.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn get(uri: &str) -> Request<Body> {
        Request::get(uri).body(Body::empty()).unwrap()
    }

    fn post(uri: &str, body: &Value) -> Request<Body> {
        Request::post(uri)
            .header(CSRF_HEADER, "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn defaults_then_watch_and_switch_off() {
        let app = app(false).await;
        let (status, body) = send(&app, get("/api/notify")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({
                "enabled": true,
                "watched": [],
                "email": false,
                "healthcheck": false,
                "cooldown_minutes": 15,
                "restarted_elsewhere": [],
            })
        );

        let (status, body) = send(&app, post(&web_url(), &json!({ "watch": true }))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["watched"], json!(["web"]));

        let (status, body) = send(&app, post("/api/notify", &json!({ "enabled": false }))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["enabled"], false);
        assert_eq!(body["watched"], json!(["web"]), "watch list kept");

        let (_, body) = send(&app, post(&web_url(), &json!({ "watch": false }))).await;
        assert_eq!(body["watched"], json!([]));

        // Persisted by name in the data dir.
        let file = std::fs::read_to_string(app.dir.path().join("notify.json")).unwrap();
        assert!(file.contains("\"enabled\": false"), "{file}");
    }

    #[tokio::test]
    async fn writes_need_same_origin() {
        let app = app(false).await;
        for uri in [
            web_url(),
            "/api/notify".to_owned(),
            "/api/notify/test".to_owned(),
        ] {
            let bare = Request::post(&uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"watch":true,"enabled":true}"#))
                .unwrap();
            let (status, _) = send(&app, bare).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");

            let cross = Request::post(&uri)
                .header(CSRF_HEADER, "1")
                .header(header::HOST, "localhost:8686")
                .header(header::ORIGIN, "http://evil.example")
                .body(Body::empty())
                .unwrap();
            let (status, _) = send(&app, cross).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        }
        let (_, body) = send(&app, get("/api/notify")).await;
        assert_eq!(body["watched"], json!([]));
    }

    #[tokio::test]
    async fn read_only_shows_state_but_refuses_writes() {
        let app = app(true).await;
        let (status, _) = send(&app, get("/api/notify")).await;
        assert_eq!(status, StatusCode::OK);
        for (uri, body) in [
            (web_url(), json!({ "watch": true })),
            ("/api/notify".to_owned(), json!({ "enabled": false })),
            ("/api/notify/test".to_owned(), json!({})),
        ] {
            let (status, res) = send(&app, post(&uri, &body)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
            assert!(res["error"].as_str().unwrap().contains("read-only"));
        }
        assert!(!app.dir.path().join("notify.json").exists());
    }

    #[tokio::test]
    async fn bad_requests() {
        let app = app(false).await;
        let (status, _) = send(&app, post(&web_url(), &json!({ "watch": "yes" }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(&app, post("/api/notify", &json!({ "on": true }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let ghost = format!(
            "/api/services/{}/notify",
            ServiceId::new(ProviderKind::Docker, "ghost")
        );
        let (status, body) = send(&app, post(&ghost, &json!({ "watch": true }))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");
        let (status, _) = send(
            &app,
            post("/api/services/nope/notify", &json!({ "watch": true })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_reports_unconfigured_channels() {
        let app = app(false).await;
        let (status, body) = send(&app, post("/api/notify/test", &json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({ "email": { "status": "off" }, "healthcheck": { "status": "off" } })
        );
    }

    #[tokio::test]
    async fn state_never_carries_secrets() {
        let dir = TempDir::new();
        let config = crate::notify::tests::local_config(dir.path(), 9, 9);
        let app = app_in(false, dir, config).await;
        let (_, body) = send(&app, get("/api/notify")).await;
        assert_eq!(
            (&body["email"], &body["healthcheck"]),
            (&json!(true), &json!(true))
        );
        let text = body.to_string();
        assert!(
            !text.contains("example.com") && !text.contains("ping/abc"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn corrupt_settings_are_unavailable() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("notify.json"), "{ nope").unwrap();
        let app = app_in(false, dir, Config::default()).await;
        let (status, body) = send(&app, get("/api/notify")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "unavailable");
        let (status, _) = send(&app, post(&web_url(), &json!({ "watch": true }))).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
}
