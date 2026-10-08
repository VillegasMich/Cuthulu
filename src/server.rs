//! HTTP application: shared state, router and security headers.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::envedit::EnvEditor;
use crate::notify::Notifier;
use crate::registry::Registry;
use crate::system::SystemMonitor;
use crate::tailscale::Tailscale;
use crate::todos::TodoStore;
use crate::{api, web};

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<Registry>,
    pub config: Arc<Config>,
    /// Fires on shutdown so long-lived streams end and the server can exit.
    pub shutdown: CancellationToken,
    /// Host CPU/memory sampler for the dashboard's system panel.
    pub system: Arc<SystemMonitor>,
    /// Per-service TODO items (`CUTHULU_DATA_DIR/todos.json`).
    pub todos: Arc<TodoStore>,
    /// Watched services, alerts, heartbeat (`CUTHULU_DATA_DIR/notify.json`).
    pub notifier: Arc<Notifier>,
    /// Link to this machine in the Tailscale admin console.
    pub tailscale: Arc<Tailscale>,
    /// Password-gated env file editor for catalog services.
    pub env_edit: Arc<EnvEditor>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(web::index))
        .route("/services/{id}", get(web::service))
        .route("/static/{*path}", get(web::asset))
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", api::router())
        .fallback(web::not_found)
        .layer(middleware::from_fn_with_state(state.clone(), check_host))
        .layer(middleware::map_response(security_headers))
        .with_state(state)
}

/// Refuses requests for host names this machine does not go by, so a page
/// that rebinds its own DNS name to this machine cannot use the API.
/// Requests without a host (non-browser clients) pass.
async fn check_host(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .map(|v| v.to_str().unwrap_or_default())
        .or_else(|| {
            req.uri()
                .authority()
                .map(axum::http::uri::Authority::as_str)
        });
    match host {
        Some(host) if !state.config.allowed_hosts.allows(host) => (
            StatusCode::MISDIRECTED_REQUEST,
            "unknown host name; add it to CUTHULU_ALLOWED_HOSTS\n",
        )
            .into_response(),
        _ => next.run(req).await,
    }
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
    use crate::envedit::tests::{MockHost, editor as env_editor, test_catalog};
    use crate::model::{ProviderKind, Service, ServiceState};
    use crate::registry::tests::{MockProvider, service};
    use crate::tailscale::TTL;

    async fn app_with(services: Vec<Service>, read_only: bool) -> (Router, Arc<Registry>) {
        app_with_env(services, read_only, EnvEditor::new(None, None, false)).await
    }

    async fn app_with_env(
        services: Vec<Service>,
        read_only: bool,
        env_edit: EnvEditor,
    ) -> (Router, Arc<Registry>) {
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
        let system = SystemMonitor::new(&config, shutdown.clone());
        let app = router(AppState {
            registry: Arc::clone(&registry),
            config: Arc::new(config),
            shutdown,
            system,
            todos: Arc::new(TodoStore::open(std::path::Path::new(
                "/nonexistent/cuthulu",
            ))),
            notifier: Notifier::new(&Config {
                data_dir: "/nonexistent/cuthulu".into(),
                ..Config::default()
            }),
            tailscale: Arc::new(crate::tailscale::tests::disabled()),
            env_edit: Arc::new(env_edit),
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

    fn post_json(uri: &str, body: &serde_json::Value) -> Request<Body> {
        Request::post(uri)
            .header(api::CSRF_HEADER, "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// An app whose only catalog entry is the service `tool`, with `host`
    /// standing in for the host.
    async fn env_app(
        host: &Arc<MockHost>,
        read_only: bool,
        host_user: Option<&str>,
    ) -> (Router, String) {
        let mut editor = env_editor(host, host_user);
        if read_only {
            editor = EnvEditor::with_catalog(
                Some(Arc::clone(host) as Arc<dyn crate::providers::HostControl>),
                test_catalog(),
                None,
                true,
            );
        }
        let tool = service("tool", ServiceState::Running);
        let base = format!("/api/services/{}/env", tool.id);
        let (app, _) = app_with_env(vec![tool, web()], read_only, editor).await;
        (app, base)
    }

    #[tokio::test]
    async fn env_load_and_save() {
        let host = Arc::new(MockHost::default());
        let text = "# c\nIMAGE=me/tool:1\nGITHUB_TOKEN=ghp_x\n";
        host.file(text);
        let (app, base) = env_app(&host, false, Some("manuel")).await;

        let (status, _, body) = send(
            &app,
            post_json(
                &format!("{base}/load"),
                &serde_json::json!({ "password": "pw" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let loaded: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(loaded["file"], "/etc/tool/env");
        assert_eq!(loaded["user"], "manuel");
        assert_eq!(loaded["vars"][1]["key"], "GITHUB_TOKEN");
        assert_eq!(loaded["vars"][1]["secret"], true);
        assert!(!body.contains("\"pw\""), "the password is never echoed");

        host.file(text);
        host.answer("write", 0, "", "");
        let save = serde_json::json!({
            "password": "pw",
            "version": loaded["version"],
            "vars": [{ "key": "IMAGE", "value": "me/tool:2" }],
        });
        let (status, _, body) = send(&app, post_json(&format!("{base}/save"), &save)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, r#"{"unit":"tool","restarting_self":false}"#);
        let calls = host.calls.lock().unwrap();
        assert_eq!(calls.last().unwrap().1, b"# c\nIMAGE=me/tool:2\n");
    }

    #[tokio::test]
    async fn env_routes_refuse_read_only_cross_origin_and_get() {
        let host = Arc::new(MockHost::default());
        let (app, base) = env_app(&host, true, Some("manuel")).await;
        let pw = serde_json::json!({ "password": "pw" });
        for path in ["load", "save"] {
            let (status, _, body) = send(&app, post_json(&format!("{base}/{path}"), &pw)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
            assert!(body.contains("read-only"), "{body}");
        }

        let (app, base) = env_app(&host, false, Some("manuel")).await;
        let no_header = Request::post(format!("{base}/load"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(pw.to_string()))
            .unwrap();
        let (status, _, _) = send(&app, no_header).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let cross = Request::post(format!("{base}/load"))
            .header(api::CSRF_HEADER, "1")
            .header("host", "localhost:8686")
            .header("origin", "http://evil.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(pw.to_string()))
            .unwrap();
        let (status, _, _) = send(&app, cross).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = send(&app, get(&format!("{base}/load"))).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "never a GET");
        assert_eq!(host.ops(), Vec::<&str>::new(), "nothing reached the host");
    }

    #[tokio::test]
    async fn env_wrong_password_is_401_then_429() {
        let host = Arc::new(MockHost::default());
        let (app, base) = env_app(&host, false, Some("manuel")).await;
        let pw = serde_json::json!({ "password": "nope" });
        for _ in 0..crate::envedit::MAX_FAILURES {
            host.answer("probe", 0, "0 root 4\n", "");
            host.answer(
                "read",
                crate::envedit::host::EXIT_SUDO,
                "",
                "Sorry, try again.",
            );
            let (status, _, body) = send(&app, post_json(&format!("{base}/load"), &pw)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert!(body.contains("wrong password"), "{body}");
        }
        let (status, headers, body) = send(&app, post_json(&format!("{base}/load"), &pw)).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        let retry: u64 = headers[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=300).contains(&retry), "{retry}");
    }

    #[tokio::test]
    async fn env_refuses_services_outside_the_catalog_and_bad_bodies() {
        let host = Arc::new(MockHost::default());
        let (app, _) = env_app(&host, false, Some("manuel")).await;
        let pw = serde_json::json!({ "password": "pw" });
        let (status, _, body) = send(
            &app,
            post_json(&format!("/api/services/{}/env/load", web().id), &pw),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(body.contains("no editable env file"), "{body}");
        let (status, _, _) = send(&app, post_json("/api/services/docker:gone/env/load", &pw)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (app, base) = env_app(&host, false, Some("manuel")).await;
        let bad = serde_json::json!({ "password": 1234, "extra": "hunter2" });
        let (status, _, body) = send(&app, post_json(&format!("{base}/load"), &bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            !body.contains("1234") && !body.contains("hunter2"),
            "{body}"
        );

        let (app, base) = env_app(&host, false, None).await;
        let (status, _, body) = send(&app, post_json(&format!("{base}/load"), &pw)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("CUTHULU_HOST_USER"), "{body}");
        assert_eq!(host.ops(), Vec::<&str>::new());
    }

    #[tokio::test]
    async fn healthz() {
        let (app, _) = app_with(vec![], false).await;
        let (status, _, body) = send(&app, get("/healthz")).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "ok"));
    }

    #[tokio::test]
    async fn version_endpoint_and_footer_show_the_build() {
        let (app, _) = app_with(vec![], false).await;
        let (status, _, body) = send(&app, get("/api/version")).await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            json["git_sha"].as_str(),
            crate::build_info::git_sha(),
            "null unless CUTHULU_BUILD_SHA was set at build time"
        );

        // Every page footer links the version to its release tag.
        let (_, _, page) = send(&app, get("/nope")).await;
        let link = format!(
            r#"<a href="https://github.com/VillegasMich/cuthulu/releases/tag/v{0}" title="release notes">v{0}</a>"#,
            env!("CARGO_PKG_VERSION")
        );
        assert!(page.contains(&link), "{page}");
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
    async fn actions_work_with_a_port_less_host() {
        // compose publishes port 80, so a tailnet browser sends a port-less
        // Host and Origin; a proxy in front may add X-Forwarded-* headers.
        let (app, _) = app_with(vec![web()], false).await;
        let host = "box.tail1234.ts.net";
        let stop_from = |origin: &str| {
            Request::post(format!("/api/services/{}/stop", web().id))
                .header(header::HOST, host)
                .header(header::ORIGIN, origin)
                .header("sec-fetch-site", "same-origin")
                .header("x-forwarded-host", host)
                .header("x-forwarded-for", "100.64.0.7")
                .header(api::CSRF_HEADER, "1")
                .body(Body::empty())
                .unwrap()
        };

        let (status, _, body) = send(&app, stop_from("http://evil.example")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

        let (status, _, body) = send(&app, stop_from(&format!("http://{host}"))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn refuses_unknown_host_names() {
        let (app, registry) = app_with(vec![web()], false).await;
        let with_host = |method: &str, uri: &str, host: &str| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::HOST, host)
                .header(header::ORIGIN, format!("http://{host}"))
                .header(api::CSRF_HEADER, "1")
                .body(Body::empty())
                .unwrap()
        };
        let stop = format!("/api/services/{}/stop", web().id);

        // A rebound name passes the same-origin check, so the host check must stop it.
        for (method, uri) in [
            ("GET", "/"),
            ("GET", "/api/services"),
            ("POST", stop.as_str()),
        ] {
            let (status, headers, _) = send(&app, with_host(method, uri, "evil.example")).await;
            assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{method} {uri}");
            assert!(headers.contains_key(header::CONTENT_SECURITY_POLICY));
        }
        assert_eq!(
            registry.get(&web().id).unwrap().state,
            ServiceState::Running
        );

        for host in [
            "localhost",
            "100.115.90.103",
            "box",
            "box.tail1234.ts.net",
            "nas.lan:80",
        ] {
            let (status, _, _) = send(&app, with_host("GET", "/api/services", host)).await;
            assert_eq!(status, StatusCode::OK, "{host}");
        }
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
    async fn dashboard_has_host_panel_splitter() {
        let (app, _) = app_with(vec![web()], false).await;
        let (_, _, index) = send(&app, get("/")).await;
        assert!(index.contains(r#"id="split-sys""#));
        assert!(index.contains(r#"role="separator" aria-orientation="horizontal""#));
        // Column splitters are added by app.js to these headers.
        for col in ["c-state", "c-name", "c-group", "c-image", "c-ports", "c-up"] {
            assert!(index.contains(&format!(r#"<th class="{col}"#)), "{col}");
        }
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
    async fn web_app_manifest_and_its_icons_are_served() {
        let (app, _) = app_with(vec![], false).await;
        let (_, _, index) = send(&app, get("/")).await;
        for link in [
            r#"<link rel="manifest" href="/static/manifest.webmanifest">"#,
            r#"<link rel="apple-touch-icon" href="/static/icons/apple-touch-icon.png">"#,
            r#"<link rel="icon" href="/static/icons/favicon-32.png" type="image/png" sizes="32x32">"#,
        ] {
            assert!(index.contains(link), "{link}");
        }

        let (status, headers, body) = send(&app, get("/static/manifest.webmanifest")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/manifest+json");
        let manifest: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(manifest["display"], "standalone");
        assert_eq!(manifest["start_url"], "/");

        let mut icons: Vec<(String, String)> = manifest["icons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["src"].as_str().unwrap().to_owned(),
                    i["sizes"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert!(
            manifest["icons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["purpose"] == "maskable")
        );
        icons.extend([
            (
                "/static/icons/apple-touch-icon.png".to_owned(),
                "180x180".to_owned(),
            ),
            (
                "/static/icons/favicon-32.png".to_owned(),
                "32x32".to_owned(),
            ),
        ]);
        for (src, sizes) in icons {
            let res = app.clone().oneshot(get(&src)).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{src}");
            let mime = res.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .to_owned();
            let png = res.into_body().collect().await.unwrap().to_bytes();
            if sizes == "any" {
                assert_eq!(mime, "image/svg+xml", "{src}");
                continue;
            }
            assert_eq!(mime, "image/png", "{src}");
            // The IHDR chunk follows the 8-byte signature: width, height as u32 BE.
            let dim = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().unwrap());
            assert_eq!(format!("{}x{}", dim(16), dim(20)), sizes, "{src}");
        }
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

    fn app_with_proc(dir: &std::path::Path) -> (Router, Arc<SystemMonitor>) {
        let shutdown = CancellationToken::new();
        let system = crate::system::tests::monitor(dir, 50, true, shutdown.clone());
        let app = router(AppState {
            registry: Registry::new(vec![]),
            config: Arc::new(Config::default()),
            shutdown,
            system: Arc::clone(&system),
            todos: Arc::new(TodoStore::open(std::path::Path::new(
                "/nonexistent/cuthulu",
            ))),
            notifier: Notifier::new(&Config {
                data_dir: "/nonexistent/cuthulu".into(),
                ..Config::default()
            }),
            tailscale: Arc::new(crate::tailscale::tests::disabled()),
            env_edit: Arc::new(EnvEditor::new(None, None, false)),
        });
        (app, system)
    }

    fn app_with_tailscale(tailscale: Tailscale) -> Router {
        let shutdown = CancellationToken::new();
        router(AppState {
            registry: Registry::new(vec![]),
            system: SystemMonitor::new(&Config::default(), shutdown.clone()),
            config: Arc::new(Config::default()),
            shutdown,
            todos: Arc::new(TodoStore::open(std::path::Path::new(
                "/nonexistent/cuthulu",
            ))),
            notifier: Notifier::new(&Config {
                data_dir: "/nonexistent/cuthulu".into(),
                ..Config::default()
            }),
            tailscale: Arc::new(tailscale),
            env_edit: Arc::new(EnvEditor::new(None, None, false)),
        })
    }

    #[tokio::test]
    async fn tailscale_link_hidden_when_unavailable() {
        use crate::tailscale::tests::FakeSource;
        for ts in [
            crate::tailscale::tests::disabled(),
            Tailscale::with_source(Some(FakeSource::new(None)), None, TTL),
        ] {
            let (status, headers, body) =
                send(&app_with_tailscale(ts), get("/api/tailscale")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "application/json");
            let link: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(link["available"], false, "{body}");
            assert!(link["url"].is_null());
        }
    }

    #[tokio::test]
    async fn tailscale_link_for_this_machine() {
        use crate::tailscale::tests::{FakeSource, STATUS};
        let ts = Tailscale::with_source(Some(FakeSource::new(Some(STATUS))), None, TTL);
        let (status, _, body) = send(&app_with_tailscale(ts), get("/api/tailscale")).await;
        assert_eq!(status, StatusCode::OK);
        let link: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            link,
            serde_json::json!({
                "available": true,
                "url": "https://login.tailscale.com/admin/machines/100.115.90.103",
                "tailnet": "someone@example.com",
                "host": "box-lenovo.tail9cad21.ts.net",
                "ip": "100.115.90.103",
            })
        );
    }

    #[tokio::test]
    async fn pages_have_a_hidden_tailscale_button() {
        let (app, _) = app_with(vec![], false).await;
        let (_, _, index) = send(&app, get("/")).await;
        assert!(index.contains(r#"id="tailscale""#), "{index}");
        assert!(index.contains(r#"rel="noopener noreferrer""#));
    }

    const PING_UUID: &str = "5bf0f1d6-0e1c-4a3c-9a3c-0c6a4c0b0b7e";

    /// An app whose notifier is built from `CUTHULU_*` variables.
    fn app_with_notify(env: &[(&str, &str)]) -> (Router, Arc<Notifier>) {
        let mut env: std::collections::HashMap<_, _> = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        env.insert("CUTHULU_DATA_DIR".into(), "/nonexistent/cuthulu".into());
        let config = Config::from_lookup(|k| env.get(k).cloned()).unwrap();
        let notifier = Notifier::new(&config);
        let shutdown = CancellationToken::new();
        let app = router(AppState {
            registry: Registry::new(vec![]),
            system: SystemMonitor::new(&config, shutdown.clone()),
            config: Arc::new(config),
            shutdown,
            todos: Arc::new(TodoStore::open(std::path::Path::new(
                "/nonexistent/cuthulu",
            ))),
            notifier: Arc::clone(&notifier),
            tailscale: Arc::new(crate::tailscale::tests::disabled()),
            env_edit: Arc::new(EnvEditor::new(None, None, false)),
        });
        (app, notifier)
    }

    async fn healthcheck(app: &Router) -> (serde_json::Value, String) {
        let (status, headers, body) = send(app, get("/api/healthcheck")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert!(
            !body.contains(PING_UUID) && !body.contains("hc-ping"),
            "{body}"
        );
        (serde_json::from_str(&body).unwrap(), body)
    }

    #[tokio::test]
    async fn healthcheck_hidden_without_ping_url() {
        for env in [
            &[][..],
            &[("CUTHULU_HEALTHCHECK_LINK", "https://healthchecks.io/")][..],
        ] {
            let (app, _) = app_with_notify(env);
            let (st, _) = healthcheck(&app).await;
            assert_eq!(
                st,
                serde_json::json!({
                    "available": false,
                    "url": null,
                    "state": "off",
                    "last_ping_at": null,
                    "error": null,
                })
            );
        }
    }

    #[tokio::test]
    async fn healthcheck_links_to_the_dashboard_or_the_override() {
        let url = format!("https://hc-ping.com/{PING_UUID}");
        let (app, _) = app_with_notify(&[("CUTHULU_HEALTHCHECK_URL", &url)]);
        let (st, _) = healthcheck(&app).await;
        assert_eq!(st["available"], true);
        assert_eq!(st["url"], "https://healthchecks.io/");
        assert_eq!(st["state"], "pending");

        let (app, _) = app_with_notify(&[
            ("CUTHULU_HEALTHCHECK_URL", &url),
            (
                "CUTHULU_HEALTHCHECK_LINK",
                "https://hc.example.com/projects/1/checks/",
            ),
        ]);
        let (st, _) = healthcheck(&app).await;
        assert_eq!(st["url"], "https://hc.example.com/projects/1/checks/");

        let (app, _) = app_with_notify(&[
            ("CUTHULU_HEALTHCHECK_URL", &url),
            ("CUTHULU_NOTIFY_ENABLED", "false"),
        ]);
        let (st, _) = healthcheck(&app).await;
        assert_eq!(
            (&st["available"], &st["state"]),
            (&true.into(), &"off".into())
        );
    }

    #[tokio::test]
    async fn healthcheck_reports_the_latest_ping() {
        use crate::notify::Outcome;
        let url = format!("https://hc-ping.com/{PING_UUID}");
        let (app, notifier) = app_with_notify(&[("CUTHULU_HEALTHCHECK_URL", &url)]);

        notifier.record_ping(Outcome::Sent);
        let (st, _) = healthcheck(&app).await;
        assert_eq!(
            (&st["state"], &st["error"]),
            (&"ok".into(), &serde_json::Value::Null)
        );
        assert!(st["last_ping_at"].as_str().unwrap().ends_with('Z'));

        notifier.record_ping(Outcome::Failed("io: Connection refused".into()));
        let (st, _) = healthcheck(&app).await;
        assert_eq!(st["state"], "failed");
        assert_eq!(st["error"], "io: Connection refused");

        notifier.record_ping(Outcome::Skipped);
        let (st, _) = healthcheck(&app).await;
        assert_eq!(st["state"], "skipped");

        notifier.record_ping(Outcome::Sent);
        let (st, _) = healthcheck(&app).await;
        assert_eq!(st["state"], "ok");
    }

    #[tokio::test]
    async fn failed_test_ping_shows_without_the_ping_url() {
        // Nothing listens on the discard port.
        let url = format!("http://127.0.0.1:9/{PING_UUID}");
        let (app, _) = app_with_notify(&[("CUTHULU_HEALTHCHECK_URL", &url)]);
        let (status, _, body) = send(&app, post("/api/notify/test")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!body.contains(PING_UUID), "{body}");

        let (st, body) = healthcheck(&app).await;
        assert_eq!(st["state"], "failed", "{body}");
        assert!(!body.contains("127.0.0.1:9"), "{body}");
    }

    #[tokio::test]
    async fn pages_have_a_hidden_healthcheck_button() {
        let url = format!("https://hc-ping.com/{PING_UUID}");
        let (app, _) = app_with_notify(&[("CUTHULU_HEALTHCHECK_URL", &url)]);
        for path in ["/", &format!("/services/{}", web().id)] {
            let (_, _, page) = send(&app, get(path)).await;
            assert!(page.contains(r#"id="healthcheck""#), "{page}");
            assert!(!page.contains(PING_UUID) && !page.contains("hc-ping"));
        }
        let (_, _, index) = send(&app, get("/")).await;
        let button = &index[index.find(r#"id="healthcheck""#).unwrap()..];
        let tag = &button[..button.find('>').unwrap()];
        assert!(
            tag.contains(" hidden") && tag.contains(r#"rel="noopener noreferrer""#),
            "{tag}"
        );
    }

    #[tokio::test]
    async fn pages_link_the_github_repo_between_tailscale_and_theme() {
        let (app, _) = app_with(vec![web()], false).await;
        // Every page shell extends base.html; the mock has no service detail,
        // so the not-found page stands in for the service page.
        for path in ["/", "/services/docker:ghost"] {
            let (_, _, page) = send(&app, get(path)).await;
            let start = page.find(r#"<a id="github""#).expect("github link");
            let link = &page[start..start + page[start..].find('>').unwrap()];
            assert!(
                link.contains(r#"href="https://github.com/VillegasMich/cuthulu""#),
                "{link}"
            );
            assert!(link.contains(r#"target="_blank""#), "{link}");
            assert!(link.contains(r#"rel="noopener noreferrer""#), "{link}");
            assert!(!link.contains("hidden"), "{link}");
            let at = |id: &str| page.find(&format!(r#"id="{id}""#)).unwrap();
            assert!(at("tailscale") < at("github") && at("github") < at("theme"));
        }
    }

    #[tokio::test]
    async fn system_snapshot_from_proc_dir() {
        let fake = crate::system::tests::FakeProc::new();
        let (app, _) = app_with_proc(&fake.0);
        let (status, headers, body) = send(&app, get("/api/system")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        let snap: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(snap["hostname"], "box");
        assert_eq!(snap["cpus"].as_array().unwrap().len(), 2);
        assert_eq!(snap["mem"]["total"], 1_024_000);
        assert_eq!(snap["load"][0], 0.5);
        assert_eq!(snap["net"]["iface"], "eth0");
        assert_eq!(snap["net"]["addrs"][0]["ip"], "192.168.1.57");
        assert_eq!(snap["net"]["addrs"][0]["kind"], "local");
        assert!(snap["disk"]["read"].is_u64());
        assert!(
            snap["procs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["cmd"] == "/sbin/init")
        );
    }

    #[tokio::test]
    async fn system_unavailable_without_proc() {
        let (app, _) = app_with_proc(std::path::Path::new("/nonexistent/cuthulu"));
        let (status, _, body) = send(&app, get("/api/system")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("\"code\":\"unavailable\""), "{body}");

        // The stream reports the failure as an event instead of closing.
        let res = app.oneshot(get("/api/system/stream")).await.unwrap();
        let mut body = res.into_body();
        let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
        let text = String::from_utf8_lossy(&frame);
        assert!(
            text.starts_with("event: failure\ndata: cannot read"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn system_stream_samples_only_while_open() {
        let fake = crate::system::tests::FakeProc::new();
        let (app, system) = app_with_proc(&fake.0);
        assert!(!system.is_sampling());

        let res = app.oneshot(get("/api/system/stream")).await.unwrap();
        assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
        assert!(system.is_sampling());
        let mut body = res.into_body();
        let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
        let text = String::from_utf8_lossy(&frame);
        assert!(text.starts_with("event: system\ndata: {"), "{text}");
        assert!(text.contains("\"hostname\":\"box\""));

        drop(body);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while system.is_sampling() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("closing the last stream stops the sampler");
    }
}
