//! Sources of services. Every backend (Docker today, systemd later) implements
//! [`Provider`]; nothing outside this module knows which one it talks to.

pub mod ansi;
pub mod docker;
pub mod level;
pub mod lines;

use async_trait::async_trait;
use futures_util::stream::BoxStream;

use crate::model::{Action, LogLine, LogOptions, ProviderKind, Service, ServiceDetail, ServiceId};

use std::time::Duration;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("service `{0}` not found")]
    NotFound(ServiceId),
    #[error("provider unavailable: {0}")]
    Unavailable(#[source] BoxError),
    #[error("provider error: {0}")]
    Backend(#[source] BoxError),
}

pub type Result<T, E = ProviderError> = std::result::Result<T, E>;

/// Something that changed on the provider side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    /// The service was created or changed; re-read it.
    Changed(ServiceId),
    /// The service no longer exists.
    Removed(ServiceId),
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// All services currently known to the backend, running or not.
    async fn list(&self) -> Result<Vec<Service>>;

    /// One service, or `None` if it no longer exists.
    async fn get(&self, id: &ServiceId) -> Result<Option<Service>>;

    async fn detail(&self, id: &ServiceId) -> Result<ServiceDetail>;

    /// Log history followed (optionally) by live lines.
    async fn logs(
        &self,
        id: &ServiceId,
        opts: LogOptions,
    ) -> Result<BoxStream<'static, Result<LogLine>>>;

    async fn act(&self, id: &ServiceId, action: Action) -> Result<()>;

    /// Change notifications. The stream ends or yields an error when the
    /// connection to the backend is lost; the caller re-subscribes.
    fn events(&self) -> BoxStream<'static, Result<ProviderEvent>>;
}

/// A `/bin/sh` script to run as root on the host, built only from fixed
/// templates (see [`crate::envedit::host`]). It is visible to anyone who can
/// inspect the helper, so it never holds a secret: those go through stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostScript {
    /// Short name of the template, for logs and test doubles.
    pub(crate) op: &'static str,
    pub(crate) text: String,
}

/// What a host script did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostOutput {
    /// Exit status of the script.
    pub status: i32,
    pub stdout: Vec<u8>,
    /// The end of stderr, bounded; may be lossy UTF-8.
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("docker is unavailable: {0}")]
    Unavailable(#[source] BoxError),
    #[error("cannot pull the helper image {image}: {reason}")]
    Image { image: String, reason: String },
    #[error("the helper container failed: {0}")]
    Helper(String),
    #[error("the host command did not finish within {0:?}")]
    Timeout(Duration),
    #[error("the host command printed more than {0} bytes")]
    TooMuchOutput(usize),
}

/// Runs commands on the host itself, outside any container. Used only for
/// the password-gated env file editor; nothing else needs host access.
#[async_trait]
pub trait HostControl: Send + Sync {
    /// Runs `script` as root on the host with `stdin` as its standard input
    /// and returns its exit status and output.
    async fn run(
        &self,
        script: &HostScript,
        stdin: &[u8],
        timeout: Duration,
    ) -> std::result::Result<HostOutput, HostError>;
}
