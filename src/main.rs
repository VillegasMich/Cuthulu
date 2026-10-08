use std::io::{IsTerminal, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use cuthulu::build_info;
use cuthulu::config::Config;
use cuthulu::envedit::EnvEditor;
use cuthulu::envfile::{self, EnvFile};
use cuthulu::notify::Notifier;
use cuthulu::providers::docker::DockerProvider;
use cuthulu::providers::{HostControl, Provider};
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

Configuration is read from CUTHULU_* environment variables, and from ./.env
when it exists (the real environment wins), see docs/ARCHITECTURE.md.";

fn main() -> anyhow::Result<ExitCode> {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("healthcheck") => {
            return Ok(healthcheck(&EnvFile::load(envfile::FILE_NAME.as_ref())?));
        }
        Some("-V" | "--version") => {
            match build_info::short_sha() {
                Some(sha) => println!("cuthulu {} ({sha})", build_info::VERSION),
                None => println!("cuthulu {}", build_info::VERSION),
            }
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

    // Before tracing, so RUST_LOG may come from the file too.
    let env = EnvFile::load(envfile::FILE_NAME.as_ref())?;
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            env.lookup("RUST_LOG")
                .and_then(|f| EnvFilter::try_new(f).ok())
                .unwrap_or_else(|| EnvFilter::new("info")),
        )
        .init();
    if let Some(path) = env.path() {
        // Names only: values may be secrets.
        info!(path = %path.display(), keys = ?env.keys(), "settings read from env file");
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(&env))?;
    Ok(ExitCode::SUCCESS)
}

async fn run(env: &EnvFile) -> anyhow::Result<()> {
    let config = Config::from_lookup(|k| env.lookup(k))?;
    let docker = Arc::new(
        DockerProvider::connect(&config.docker_host)
            .with_context(|| format!("cannot use docker at {}", config.docker_host))?
            .with_helper_image(config.helper_image.clone()),
    );

    let registry = Registry::new(vec![Arc::clone(&docker) as Arc<dyn Provider>]);
    let env_edit = Arc::new(EnvEditor::new(
        Some(docker as Arc<dyn HostControl>),
        config.host_user.clone(),
        config.read_only,
    ));
    let todos = Arc::new(TodoStore::open(&config.data_dir));
    let shutdown = CancellationToken::new();
    let notifier = Notifier::new(&config);
    // Before the registry starts, so the first listing seeds the alert state.
    let mut tasks = notifier.spawn(&registry, &shutdown);
    tasks.extend(registry.spawn(config.reconcile_interval, &shutdown));

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("cannot listen on {}", config.bind))?;
    info!(
        version = build_info::VERSION,
        git_sha = build_info::git_sha().unwrap_or("unknown"),
        read_only = config.read_only,
        host_user = config.host_user.as_ref().map(ToString::to_string),
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
        notifier: Arc::clone(&notifier),
        tailscale,
        env_edit,
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown.clone()))
        .await?;

    shutdown.cancel();
    for t in tasks {
        t.await?;
    }
    notifier.stopped().await;
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
fn healthcheck(env: &EnvFile) -> ExitCode {
    let bind =
        Config::from_lookup(|k| env.lookup(k)).map_or_else(|_| Config::default().bind, |c| c.bind);
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
