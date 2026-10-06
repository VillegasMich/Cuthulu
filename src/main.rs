use std::io::{IsTerminal, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use cuthulu::config::Config;
use cuthulu::providers::docker::DockerProvider;
use cuthulu::registry::Registry;
use cuthulu::server::{self, AppState};
use cuthulu::system::SystemMonitor;
use cuthulu::tailscale::Tailscale;
use cuthulu::todos::TodoStore;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
cuthulu - the eye that never sleeps

USAGE:
    cuthulu               run the dashboard
    cuthulu healthcheck   exit 0 if the local server answers /healthz
    cuthulu --version

Configuration is read from CUTHULU_* environment variables, see docs/ARCHITECTURE.md.";

fn main() -> anyhow::Result<ExitCode> {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("healthcheck") => return Ok(healthcheck()),
        Some("-V" | "--version") => {
            println!("cuthulu {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        }
        Some(other) => {
            eprintln!("unknown argument `{other}`\n\n{USAGE}");
            return Ok(ExitCode::from(2));
        }
    }

    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())?;
    Ok(ExitCode::SUCCESS)
}

async fn run() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let docker = DockerProvider::connect(&config.docker_host)
        .with_context(|| format!("cannot use docker at {}", config.docker_host))?;

    let registry = Registry::new(vec![Arc::new(docker)]);
    let todos = Arc::new(TodoStore::open(&config.data_dir));
    let shutdown = CancellationToken::new();
    let watchers = registry.spawn(config.reconcile_interval, &shutdown);

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("cannot listen on {}", config.bind))?;
    info!(
        version = env!("CARGO_PKG_VERSION"),
        read_only = config.read_only,
        "listening on http://{}",
        listener.local_addr()?
    );

    let system = SystemMonitor::new(&config, shutdown.clone());
    let tailscale = Arc::new(Tailscale::new(&config));
    let app = server::router(AppState {
        registry,
        config: Arc::new(config),
        shutdown: shutdown.clone(),
        system,
        todos,
        tailscale,
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown.clone()))
        .await?;

    shutdown.cancel();
    for w in watchers {
        w.await?;
    }
    info!("bye");
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM and cancels `token` so open streams close.
async fn shutdown_signal(token: CancellationToken) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
        () = token.cancelled() => {}
    }
    info!("shutting down");
    token.cancel();
}

/// Used as the container HEALTHCHECK, since the image has no curl.
fn healthcheck() -> ExitCode {
    let bind = Config::from_env().map_or_else(|_| Config::default().bind, |c| c.bind);
    let ip = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let addr = SocketAddr::new(ip, bind.port());
    let timeout = Duration::from_secs(3);

    let check = || -> std::io::Result<bool> {
        let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        let mut head = [0u8; 12];
        stream.read_exact(&mut head)?;
        Ok(head.ends_with(b" 200"))
    };

    match check() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("healthcheck {addr}: {e}");
            ExitCode::FAILURE
        }
    }
}
