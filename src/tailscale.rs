//! Link to this machine's page in the Tailscale admin console.
//!
//! Not a [`Provider`](crate::providers::Provider): Tailscale is not a service
//! source, it only tells the topbar where this node lives in its tailnet.
//! The node's address comes from tailscaled's `LocalAPI` (`GET
//! /localapi/v0/status?peers=false` over its unix socket), read on demand and
//! cached for [`TTL`]. Only that one read-only endpoint is ever called, and
//! only the fields the button needs leave this module — no keys, no peers.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Bytes;
use hyper::{Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::debug;

use crate::config::Config;

/// Admin console page listing the machines of the signed-in tailnet.
pub const ADMIN_MACHINES: &str = "https://login.tailscale.com/admin/machines";
/// How long a lookup (success or failure) is reused.
pub const TTL: Duration = Duration::from_secs(60);
/// Upper bound for one `LocalAPI` exchange, connect included.
const TIMEOUT: Duration = Duration::from_secs(2);
/// Upper bound for the status body (without peers it is a few KiB).
const MAX_BODY: usize = 256 * 1024;
/// `LocalAPI` ignores the host, but the Tailscale clients send this one.
const LOCALAPI_HOST: &str = "local-tailscaled.sock";
const STATUS_PATH: &str = "/localapi/v0/status?peers=false";

#[derive(Debug, thiserror::Error)]
pub enum TailscaleError {
    #[error("cannot reach tailscaled: {0}")]
    Io(#[from] std::io::Error),
    #[error("tailscaled request failed: {0}")]
    Http(#[from] hyper::Error),
    #[error("tailscaled answered {0}")]
    Status(StatusCode),
    #[error("tailscaled did not answer within {}s", TIMEOUT.as_secs())]
    Timeout,
    #[error("tailscaled status is larger than {MAX_BODY} bytes")]
    TooLarge,
    #[error("cannot parse tailscaled status: {0}")]
    Parse(#[from] serde_json::Error),
}

/// What `/api/tailscale` returns: just enough for the topbar button.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Link {
    /// False when Tailscale is disabled or tailscaled cannot be reached;
    /// every other field is then `null` and the button stays hidden.
    pub available: bool,
    pub url: Option<String>,
    /// Name of the tailnet of the current profile.
    pub tailnet: Option<String>,
    /// `MagicDNS` name of this node (or its host name).
    pub host: Option<String>,
    /// This node's Tailscale IPv4 address.
    pub ip: Option<String>,
}

/// The subset of tailscaled's `ipnstate.Status` that is read. Every other
/// field (keys, users, health…) is skipped by the deserializer.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Status {
    #[serde(rename = "Self")]
    pub node: Option<Node>,
    pub current_tailnet: Option<Tailnet>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Node {
    #[serde(default)]
    pub host_name: String,
    #[serde(rename = "DNSName", default)]
    pub dns_name: String,
    #[serde(rename = "TailscaleIPs", default)]
    pub tailscale_ips: Option<Vec<IpAddr>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Tailnet {
    #[serde(default)]
    pub name: String,
}

pub fn parse_status(body: &[u8]) -> Result<Status, TailscaleError> {
    Ok(serde_json::from_slice(body)?)
}

/// Builds the link from a status; `override_url`, when set, wins over the
/// derived one. Without a known IPv4 the link falls back to the machine list.
#[must_use]
pub fn link(status: &Status, override_url: Option<&str>) -> Link {
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    let node = status.node.as_ref();
    let ip = node
        .and_then(|n| n.tailscale_ips.as_deref())
        .and_then(|ips| ips.iter().find(|ip| ip.is_ipv4()))
        .map(ToString::to_string);
    let host = node.and_then(|n| {
        non_empty(n.dns_name.trim_end_matches('.')).or_else(|| non_empty(&n.host_name))
    });
    let url = override_url.map_or_else(
        || {
            ip.as_ref().map_or_else(
                || ADMIN_MACHINES.to_owned(),
                |ip| format!("{ADMIN_MACHINES}/{ip}"),
            )
        },
        ToOwned::to_owned,
    );
    Link {
        available: true,
        url: Some(url),
        tailnet: status
            .current_tailnet
            .as_ref()
            .and_then(|t| non_empty(&t.name)),
        host,
        ip,
    }
}

/// Where the status comes from; tests inject their own.
#[async_trait]
pub trait StatusSource: Send + Sync {
    async fn status(&self) -> Result<Status, TailscaleError>;
}

/// tailscaled's `LocalAPI` on a unix socket.
#[derive(Debug)]
pub struct LocalApi {
    socket: PathBuf,
}

impl LocalApi {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    async fn fetch(&self) -> Result<Bytes, TailscaleError> {
        let stream = tokio::net::UnixStream::connect(&self.socket).await?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        let req = Request::get(STATUS_PATH)
            .header(header::HOST, LOCALAPI_HOST)
            .body(Empty::<Bytes>::new())
            .expect("static request is valid");
        let exchange = async move {
            let res = sender.send_request(req).await?;
            if res.status() != StatusCode::OK {
                return Err(TailscaleError::Status(res.status()));
            }
            Limited::new(res.into_body(), MAX_BODY)
                .collect()
                .await
                .map(http_body_util::Collected::to_bytes)
                .map_err(|e| match e.downcast::<hyper::Error>() {
                    Ok(e) => TailscaleError::Http(*e),
                    Err(_) => TailscaleError::TooLarge,
                })
        };
        // Drive the connection in this future rather than a spawned task, so
        // nothing outlives the timeout.
        tokio::pin!(conn, exchange);
        tokio::select! {
            biased;
            res = &mut exchange => res,
            res = &mut conn => {
                res?;
                exchange.await
            }
        }
    }
}

#[async_trait]
impl StatusSource for LocalApi {
    async fn status(&self) -> Result<Status, TailscaleError> {
        let body = tokio::time::timeout(TIMEOUT, self.fetch())
            .await
            .map_err(|_| TailscaleError::Timeout)??;
        parse_status(&body)
    }
}

/// On-demand, cached lookup of the admin console link.
pub struct Tailscale {
    source: Option<Arc<dyn StatusSource>>,
    override_url: Option<String>,
    ttl: Duration,
    /// Held across a lookup, so concurrent requests share one.
    cache: Mutex<Option<(Instant, Arc<Link>)>>,
}

impl Tailscale {
    #[must_use]
    pub fn new(config: &Config) -> Self {
        let source = config
            .tailscale_socket
            .as_ref()
            .map(|p| Arc::new(LocalApi::new(p)) as Arc<dyn StatusSource>);
        Self::with_source(source, config.tailscale_url.clone(), TTL)
    }

    #[must_use]
    pub fn with_source(
        source: Option<Arc<dyn StatusSource>>,
        override_url: Option<String>,
        ttl: Duration,
    ) -> Self {
        Self {
            source,
            override_url,
            ttl,
            cache: Mutex::new(None),
        }
    }

    /// The current link, from cache when it is younger than the TTL.
    pub async fn link(&self) -> Arc<Link> {
        let mut cache = self.cache.lock().await;
        if let Some((at, link)) = cache.as_ref()
            && at.elapsed() < self.ttl
        {
            return Arc::clone(link);
        }
        let link = Arc::new(self.lookup().await);
        *cache = Some((Instant::now(), Arc::clone(&link)));
        link
    }

    async fn lookup(&self) -> Link {
        let status = match &self.source {
            Some(source) => source.status().await,
            None => return self.override_only(),
        };
        match status {
            Ok(status) => link(&status, self.override_url.as_deref()),
            Err(e) => {
                debug!(error = %e, "tailscale status unavailable");
                self.override_only()
            }
        }
    }

    /// An explicit URL works without tailscaled; there is just no detail.
    fn override_only(&self) -> Link {
        self.override_url
            .as_ref()
            .map_or_else(Link::default, |url| Link {
                available: true,
                url: Some(url.clone()),
                ..Link::default()
            })
    }
}

#[cfg(test)]
pub mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Trimmed from `tailscale status --json` (1.102), peers omitted.
    pub const STATUS: &str = r#"{
      "Version": "1.102.4",
      "BackendState": "Running",
      "TailscaleIPs": ["100.115.90.103", "fd7a:115c:a1e0::43a:5a67"],
      "Self": {
        "ID": "nTestNodeID1CNTRL",
        "PublicKey": "nodekey:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "HostName": "box-Lenovo",
        "DNSName": "box-lenovo.tail9cad21.ts.net.",
        "OS": "linux",
        "TailscaleIPs": ["fd7a:115c:a1e0::43a:5a67", "100.115.90.103"],
        "Online": true
      },
      "MagicDNSSuffix": "tail9cad21.ts.net",
      "CurrentTailnet": {
        "Name": "someone@example.com",
        "MagicDNSSuffix": "tail9cad21.ts.net",
        "MagicDNSEnabled": true
      },
      "User": {"1": {"ID": 1, "LoginName": "someone@example.com"}}
    }"#;

    /// No socket, no override: the button stays hidden.
    #[must_use]
    pub fn disabled() -> Tailscale {
        Tailscale::with_source(None, None, TTL)
    }

    /// Counts calls and returns a fixed status (or an error).
    pub struct FakeSource {
        pub body: Option<&'static str>,
        pub calls: AtomicUsize,
    }

    impl FakeSource {
        #[must_use]
        pub fn new(body: Option<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                body,
                calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl StatusSource for FakeSource {
        async fn status(&self) -> Result<Status, TailscaleError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.body {
                Some(body) => parse_status(body.as_bytes()),
                None => Err(TailscaleError::Timeout),
            }
        }
    }

    #[test]
    fn links_to_this_machine_by_ipv4() {
        let l = link(&parse_status(STATUS.as_bytes()).unwrap(), None);
        assert_eq!(
            l,
            Link {
                available: true,
                url: Some(format!("{ADMIN_MACHINES}/100.115.90.103")),
                tailnet: Some("someone@example.com".into()),
                host: Some("box-lenovo.tail9cad21.ts.net".into()),
                ip: Some("100.115.90.103".into()),
            }
        );
    }

    #[test]
    fn never_exposes_keys_or_ids() {
        let l = link(&parse_status(STATUS.as_bytes()).unwrap(), None);
        let json = serde_json::to_string(&l).unwrap();
        assert!(
            !json.contains("nodekey") && !json.contains("TestNodeID"),
            "{json}"
        );
    }

    #[test]
    fn falls_back_to_the_machine_list() {
        // Logged out: no Self, or Self without addresses yet.
        for body in [
            r#"{"BackendState": "NeedsLogin", "Self": null, "CurrentTailnet": null}"#,
            r#"{"BackendState": "Starting", "Self": {"HostName": "box", "DNSName": "", "TailscaleIPs": null}}"#,
            r#"{"Self": {"HostName": "box", "TailscaleIPs": ["fd7a:115c:a1e0::1"]}}"#,
        ] {
            let l = link(&parse_status(body.as_bytes()).unwrap(), None);
            assert!(l.available);
            assert_eq!(l.url.as_deref(), Some(ADMIN_MACHINES), "{body}");
            assert_eq!(l.ip, None);
        }
        let l = link(
            &parse_status(br#"{"Self": {"HostName": "box", "DNSName": ""}}"#).unwrap(),
            None,
        );
        assert_eq!(l.host.as_deref(), Some("box"), "host name without MagicDNS");
    }

    #[test]
    fn override_url_wins() {
        let status = parse_status(STATUS.as_bytes()).unwrap();
        let l = link(&status, Some("https://example.com/ts"));
        assert_eq!(l.url.as_deref(), Some("https://example.com/ts"));
        assert_eq!(l.ip.as_deref(), Some("100.115.90.103"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            parse_status(b"<html>"),
            Err(TailscaleError::Parse(_))
        ));
    }

    #[tokio::test]
    async fn unavailable_without_source() {
        let ts = Tailscale::with_source(None, None, TTL);
        assert_eq!(*ts.link().await, Link::default());

        let ts = Tailscale::with_source(None, Some("https://x.test/".into()), TTL);
        let l = ts.link().await;
        assert!(l.available);
        assert_eq!(l.url.as_deref(), Some("https://x.test/"));
        assert_eq!(l.host, None);
    }

    #[tokio::test]
    async fn failures_hide_the_button_unless_overridden() {
        let ts = Tailscale::with_source(Some(FakeSource::new(None)), None, TTL);
        assert!(!ts.link().await.available);

        let ts = Tailscale::with_source(
            Some(FakeSource::new(None)),
            Some("https://x.test/".into()),
            TTL,
        );
        assert_eq!(ts.link().await.url.as_deref(), Some("https://x.test/"));
    }

    #[tokio::test]
    async fn caches_for_the_ttl() {
        let src = FakeSource::new(Some(STATUS));
        let ts = Tailscale::with_source(Some(src.clone()), None, TTL);
        ts.link().await;
        ts.link().await;
        assert_eq!(src.calls.load(Ordering::SeqCst), 1);

        let ts = Tailscale::with_source(Some(src.clone()), None, Duration::ZERO);
        ts.link().await;
        ts.link().await;
        assert_eq!(src.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn missing_socket_is_an_io_error() {
        let api = LocalApi::new("/nonexistent/cuthulu/tailscaled.sock");
        assert!(matches!(api.status().await, Err(TailscaleError::Io(_))));
    }

    /// Serves one canned HTTP response on a unix socket in a temp dir.
    fn serve_once(response: Vec<u8>) -> (PathBuf, tokio::task::JoinHandle<Vec<u8>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = std::env::temp_dir().join(format!(
            "cuthulu-ts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tailscaled.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let task = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut req = vec![0u8; 4096];
            let n = s.read(&mut req).await.unwrap();
            req.truncate(n);
            // The client may hang up early (body too large).
            let _ = s.write_all(&response).await;
            let _ = s.shutdown().await;
            req
        });
        (path, task)
    }

    #[tokio::test]
    async fn local_api_reads_status_over_the_socket() {
        let (path, server) = serve_once(ok(
            r#"{"Self":{"DNSName":"a.b.ts.net.","TailscaleIPs":["100.64.0.1"]},"X":[1,2,3]}"#,
        ));
        let status = LocalApi::new(&path).status().await.unwrap();
        let req = String::from_utf8(server.await.unwrap()).unwrap();
        assert!(
            req.starts_with("GET /localapi/v0/status?peers=false HTTP/1.1\r\n"),
            "{req}"
        );
        assert!(
            req.to_ascii_lowercase()
                .contains("host: local-tailscaled.sock")
        );
        assert_eq!(link(&status, None).ip.as_deref(), Some("100.64.0.1"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn local_api_rejects_errors() {
        let (path, _server) =
            serve_once(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 6\r\n\r\ndenied".to_vec());
        assert!(matches!(
            LocalApi::new(&path).status().await,
            Err(TailscaleError::Status(StatusCode::FORBIDDEN))
        ));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();

        let huge = format!(r#"{{"X":"{}"}}"#, "a".repeat(MAX_BODY));
        let (path, _server) = serve_once(ok(&huge));
        assert!(matches!(
            LocalApi::new(&path).status().await,
            Err(TailscaleError::TooLarge)
        ));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn ok(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
}
