//! Sources of services. Every backend (Docker today, systemd later) implements
//! [`Provider`]; nothing outside this module knows which one it talks to.

pub mod ansi;
pub mod docker;
pub mod level;
pub mod lines;

use async_trait::async_trait;
use futures_util::stream::BoxStream;

use crate::model::{Action, LogLine, LogOptions, ProviderKind, Service, ServiceDetail, ServiceId};

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
