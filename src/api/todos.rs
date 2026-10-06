//! Per-service TODO items: `GET|POST /api/services/{id}/todos` and
//! `POST /api/services/{id}/todos/{todo_id}/toggle|delete`.
//!
//! Items are stored by service name, so the `{id}` must belong to a service
//! the registry currently knows. Every route returns the service's full list.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::Deserialize;

use super::{ApiError, guard, parse_id};
use crate::server::AppState;
use crate::todos::{Todo, TodoError};

#[derive(Debug, Deserialize)]
pub struct NewTodo {
    text: String,
}

impl From<TodoError> for ApiError {
    fn from(e: TodoError) -> Self {
        match e {
            TodoError::Invalid(msg) => Self::BadRequest(msg),
            TodoError::NotFound(_) => Self::NotFound(e.to_string()),
            TodoError::Storage(msg) => Self::Unavailable(msg),
        }
    }
}

pub async fn list(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Todo>>, ApiError> {
    let name = service_name(&st, &id)?;
    Ok(Json(st.todos.list(&name).await?))
}

pub async fn create(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<NewTodo>, JsonRejection>,
) -> Result<Json<Vec<Todo>>, ApiError> {
    let name = writable(&st, &headers, &id)?;
    let Json(new) = body.map_err(|e| ApiError::BadRequest(e.body_text()))?;
    Ok(Json(st.todos.add(&name, &new.text).await?))
}

pub async fn toggle(
    State(st): State<AppState>,
    Path((id, todo_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<Todo>>, ApiError> {
    let name = writable(&st, &headers, &id)?;
    Ok(Json(
        st.todos.toggle(&name, parse_todo_id(&todo_id)?).await?,
    ))
}

pub async fn delete(
    State(st): State<AppState>,
    Path((id, todo_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<Todo>>, ApiError> {
    let name = writable(&st, &headers, &id)?;
    Ok(Json(
        st.todos.delete(&name, parse_todo_id(&todo_id)?).await?,
    ))
}

/// Same-origin and read-only checks for a write, then the service name.
fn writable(st: &AppState, headers: &HeaderMap, id: &str) -> Result<String, ApiError> {
    guard::same_origin(headers)?;
    if st.config.read_only {
        return Err(ApiError::Forbidden(
            "cuthulu is running in read-only mode".to_owned(),
        ));
    }
    service_name(st, id)
}

fn service_name(st: &AppState, raw: &str) -> Result<String, ApiError> {
    let id = parse_id(raw)?;
    st.registry
        .get(&id)
        .map(|s| s.name)
        .ok_or_else(|| ApiError::NotFound(format!("service `{id}` not found")))
}

fn parse_todo_id(raw: &str) -> Result<u64, ApiError> {
    raw.parse()
        .map_err(|_| ApiError::BadRequest(format!("invalid todo id `{raw}`")))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use crate::api::CSRF_HEADER;
    use crate::config::Config;
    use crate::model::{ProviderKind, ServiceId, ServiceState};
    use crate::registry::tests::{MockProvider, service};
    use crate::registry::{Registry, RegistryEvent};
    use crate::server::{AppState, router};
    use crate::system::SystemMonitor;
    use crate::todos::tests::TempDir;
    use crate::todos::{MAX_TODO_CHARS, TodoStore};

    struct App {
        router: Router,
        _dir: TempDir,
    }

    async fn router_with(read_only: bool, todo_dir: &Path) -> Router {
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
            ..Config::default()
        };
        router(AppState {
            registry,
            system: SystemMonitor::new(&config, shutdown.clone()),
            config: Arc::new(config),
            todos: Arc::new(TodoStore::open(todo_dir)),
            shutdown,
            tailscale: Arc::new(crate::tailscale::tests::disabled()),
        })
    }

    async fn app(read_only: bool) -> App {
        let dir = TempDir::new();
        App {
            router: router_with(read_only, dir.path()).await,
            _dir: dir,
        }
    }

    fn base() -> String {
        format!(
            "/api/services/{}/todos",
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

    fn post(uri: &str) -> Request<Body> {
        Request::post(uri)
            .header(CSRF_HEADER, "1")
            .body(Body::empty())
            .unwrap()
    }

    fn create(text: &str) -> Request<Body> {
        Request::post(base())
            .header(CSRF_HEADER, "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::json!({ "text": text }).to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn create_toggle_delete_round_trip() {
        let app = app(false).await;
        let (status, body) = send(&app, get(&base())).await;
        assert_eq!((status, body), (StatusCode::OK, Value::Array(vec![])));

        let (status, body) = send(&app, create("  check backups ")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body[0]["text"], "check backups");
        assert_eq!(body[0]["done"], false);
        assert!(body[0]["created_at"].is_string());
        assert!(body[0]["done_at"].is_null());
        let id = body[0]["id"].as_u64().unwrap();

        let (status, body) = send(&app, post(&format!("{}/{id}/toggle", base()))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body[0]["done"], true);
        assert!(body[0]["done_at"].is_string());

        let (_, body) = send(&app, get(&base())).await;
        assert_eq!(body[0]["done"], true);

        let (status, body) = send(&app, post(&format!("{}/{id}/delete", base()))).await;
        assert_eq!((status, body), (StatusCode::OK, Value::Array(vec![])));
    }

    #[tokio::test]
    async fn writes_need_the_csrf_header() {
        let app = app(false).await;
        let bare = Request::post(base())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"text":"x"}"#))
            .unwrap();
        let (status, body) = send(&app, bare).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

        let cross = Request::post(base())
            .header(CSRF_HEADER, "1")
            .header(header::HOST, "localhost:8686")
            .header(header::ORIGIN, "http://evil.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"text":"x"}"#))
            .unwrap();
        let (status, _) = send(&app, cross).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (_, body) = send(&app, post(&base())).await;
        assert_eq!(body["code"], "bad_request", "missing JSON body");
        let (status, _) = send(&app, create("x")).await;
        assert_eq!(status, StatusCode::OK);

        for op in ["toggle", "delete"] {
            let bare = Request::post(format!("{}/1/{op}", base()))
                .body(Body::empty())
                .unwrap();
            let (status, _) = send(&app, bare).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{op}");
        }
        let (_, body) = send(&app, get(&base())).await;
        assert_eq!(body[0]["done"], false);
    }

    #[tokio::test]
    async fn read_only_lists_but_refuses_writes() {
        let app = app(true).await;
        let (status, _) = send(&app, get(&base())).await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send(&app, create("x")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body["error"].as_str().unwrap().contains("read-only"));
        for op in ["toggle", "delete"] {
            let (status, _) = send(&app, post(&format!("{}/1/{op}", base()))).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{op}");
        }
    }

    #[tokio::test]
    async fn rejects_bad_text() {
        let app = app(false).await;
        for text in [
            String::new(),
            "   ".to_owned(),
            "x".repeat(MAX_TODO_CHARS + 1),
        ] {
            let (status, body) = send(&app, create(&text)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{text:?}");
            assert_eq!(body["code"], "bad_request");
        }
        let wrong_shape = Request::post(base())
            .header(CSRF_HEADER, "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"title":"x"}"#))
            .unwrap();
        let (status, body) = send(&app, wrong_shape).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bad_request");
    }

    #[tokio::test]
    async fn caps_items_per_service() {
        let app = app(false).await;
        for i in 0..crate::todos::MAX_TODOS_PER_SERVICE {
            let (status, _) = send(&app, create(&format!("item {i}"))).await;
            assert_eq!(status, StatusCode::OK);
        }
        let (status, body) = send(&app, create("one more")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("at most"));
    }

    #[tokio::test]
    async fn unknown_service_or_todo() {
        let app = app(false).await;
        let ghost = format!(
            "/api/services/{}/todos",
            ServiceId::new(ProviderKind::Docker, "ghost")
        );
        let (status, body) = send(&app, get(&ghost)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");
        let (status, _) = send(&app, post(&format!("{ghost}/1/toggle"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = send(&app, get("/api/services/not-an-id/todos")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) = send(&app, post(&format!("{}/42/toggle", base()))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");
        let (status, body) = send(&app, post(&format!("{}/abc/delete", base()))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bad_request");
    }

    #[tokio::test]
    async fn storage_failure_is_reported() {
        let dir = TempDir::new();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        // A directory below a regular file can never be created.
        let app = App {
            router: router_with(false, &blocker.join("data")).await,
            _dir: dir,
        };

        let (status, body) = send(&app, create("x")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "unavailable");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("cannot save todos")
        );
    }
}
