//! Which services are watched, and the global on/off switch, persisted as
//! `notify.json` in `CUTHULU_DATA_DIR`.
//!
//! Same rules as the TODO store: keyed by service *name* (survives container
//! re-creation), loaded once, every change written to a temporary file and
//! renamed over the old one before the in-memory copy is updated, and a file
//! that cannot be parsed is never overwritten.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::todos::write_atomic;

/// Most services that can be watched at once.
pub const MAX_WATCHED: usize = 500;
/// Longest service name accepted, in bytes.
const MAX_NAME_LEN: usize = 256;

const FILE_NAME: &str = "notify.json";
const FILE_VERSION: u32 = 1;

/// On-disk layout of `notify.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub version: u32,
    /// Global switch for service-down alerts (email and browser).
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Service names to alert about.
    #[serde(default)]
    pub watched: BTreeSet<String>,
}

const fn enabled_by_default() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            enabled: true,
            watched: BTreeSet::new(),
        }
    }
}

impl Settings {
    /// Whether a down/up alert for `name` should go out.
    #[must_use]
    pub fn alerts_for(&self, name: &str) -> bool {
        self.enabled && self.watched.contains(name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Invalid(String),
    /// The file could not be read at startup or written now.
    #[error("{0}")]
    Storage(String),
}

pub struct NotifyStore {
    path: PathBuf,
    /// Serialises writes; held across the file write.
    write: tokio::sync::Mutex<()>,
    /// `Err` holds why the file could not be loaded.
    data: Mutex<Result<Settings, String>>,
}

impl NotifyStore {
    /// Loads `<dir>/notify.json`. A missing file means defaults (alerts on,
    /// nothing watched); an unreadable one makes every request fail.
    #[must_use]
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        let data = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Settings>(&bytes)
                .map_err(|e| format!("cannot parse {}: {e}", path.display())),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(Settings::default())
            }
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        };
        match &data {
            Ok(s) => info!(
                path = %path.display(),
                enabled = s.enabled,
                watched = s.watched.len(),
                "notification settings loaded"
            ),
            Err(e) => error!(error = %e, "notification settings unavailable"),
        }
        Self {
            path,
            write: tokio::sync::Mutex::new(()),
            data: Mutex::new(data),
        }
    }

    /// The current settings.
    pub fn get(&self) -> Result<Settings, StoreError> {
        lock(&self.data)
            .as_ref()
            .cloned()
            .map_err(|e| StoreError::Storage(e.clone()))
    }

    pub async fn set_enabled(&self, on: bool) -> Result<Settings, StoreError> {
        self.update(|s| {
            s.enabled = on;
            Ok(())
        })
        .await
    }

    /// Adds `name` to, or removes it from, the watched set.
    pub async fn set_watched(&self, name: &str, on: bool) -> Result<Settings, StoreError> {
        if name.is_empty() || name.len() > MAX_NAME_LEN || name.chars().any(char::is_control) {
            return Err(StoreError::Invalid(format!(
                "invalid service name `{name}`"
            )));
        }
        self.update(|s| {
            if !on {
                s.watched.remove(name);
            } else if !s.watched.contains(name) {
                if s.watched.len() >= MAX_WATCHED {
                    return Err(StoreError::Invalid(format!(
                        "at most {MAX_WATCHED} services can be watched"
                    )));
                }
                s.watched.insert(name.to_owned());
            }
            Ok(())
        })
        .await
    }

    /// Applies `change` to a copy, persists it, then commits it.
    async fn update(
        &self,
        change: impl FnOnce(&mut Settings) -> Result<(), StoreError>,
    ) -> Result<Settings, StoreError> {
        let _writing = self.write.lock().await;
        let mut next = self.get()?;
        change(&mut next)?;
        next.version = FILE_VERSION;
        if Ok(&next) == lock(&self.data).as_ref() {
            return Ok(next);
        }

        let bytes = serde_json::to_vec_pretty(&next)
            .map_err(|e| StoreError::Storage(format!("cannot encode settings: {e}")))?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || write_atomic(&path, &bytes))
            .await
            .map_err(|e| StoreError::Storage(format!("cannot save settings: {e}")))?
            .map_err(|e| {
                StoreError::Storage(format!(
                    "cannot save settings to {}: {e}",
                    self.path.display()
                ))
            })?;
        *lock(&self.data) = Ok(next.clone());
        Ok(next)
    }
}

/// Plain data, valid after any panic: poisoning is ignored.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todos::tests::TempDir;

    #[tokio::test]
    async fn defaults_toggle_and_persist() {
        let dir = TempDir::new();
        let store = NotifyStore::open(dir.path());
        let s = store.get().unwrap();
        assert!(s.enabled && s.watched.is_empty());
        assert!(!dir.path().join(FILE_NAME).exists(), "nothing written yet");

        store.set_watched("web", true).await.unwrap();
        store.set_watched("db", true).await.unwrap();
        store.set_watched("db", true).await.unwrap();
        let s = store.set_watched("web", false).await.unwrap();
        assert_eq!(s.watched.iter().collect::<Vec<_>>(), ["db"]);
        let s = store.set_enabled(false).await.unwrap();
        assert!(!s.alerts_for("db"));

        let reopened = NotifyStore::open(dir.path()).get().unwrap();
        assert_eq!(reopened, s);
        assert!(!reopened.enabled);
        assert!(!dir.path().join("notify.json.tmp").exists());
        assert!(
            fs::read_to_string(dir.path().join(FILE_NAME))
                .unwrap()
                .contains("\"watched\"")
        );
    }

    #[test]
    fn alerts_need_both_switches() {
        let mut s = Settings::default();
        assert!(!s.alerts_for("web"));
        s.watched.insert("web".into());
        assert!(s.alerts_for("web"));
        s.enabled = false;
        assert!(!s.alerts_for("web"));
    }

    #[tokio::test]
    async fn validates_names_and_caps_the_set() {
        let dir = TempDir::new();
        let store = NotifyStore::open(dir.path());
        for bad in [String::new(), "a\nb".into(), "x".repeat(MAX_NAME_LEN + 1)] {
            assert!(matches!(
                store.set_watched(&bad, true).await,
                Err(StoreError::Invalid(_))
            ));
        }
        for i in 0..MAX_WATCHED {
            store.set_watched(&format!("s{i}"), true).await.unwrap();
        }
        assert!(matches!(
            store.set_watched("one-more", true).await,
            Err(StoreError::Invalid(_))
        ));
        // Removing and re-adding existing names still works at the cap.
        store.set_watched("s0", true).await.unwrap();
        store.set_watched("s0", false).await.unwrap();
    }

    #[tokio::test]
    async fn corrupt_file_is_never_overwritten() {
        let dir = TempDir::new();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, "{ nope").unwrap();
        let store = NotifyStore::open(dir.path());
        assert!(matches!(store.get(), Err(StoreError::Storage(_))));
        assert!(store.set_enabled(false).await.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ nope");
    }

    #[tokio::test]
    async fn unwritable_dir_keeps_the_old_state() {
        let dir = TempDir::new();
        let blocker = dir.path().join("file");
        fs::write(&blocker, "").unwrap();
        let store = NotifyStore::open(&blocker.join("data"));
        let err = store.set_watched("web", true).await.unwrap_err();
        assert!(err.to_string().contains("cannot save settings"), "{err}");
        assert!(store.get().unwrap().watched.is_empty());
    }
}
