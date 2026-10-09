//! Catalog services Cuthulu stopped with `systemctl`, persisted as
//! `stopped.json` in `CUTHULU_DATA_DIR` so their placeholder rows survive a
//! restart of Cuthulu.
//!
//! Same rules as the other stores: keyed by service name, loaded once, every
//! change written to a temporary file and renamed over the old one before
//! the in-memory copy changes, and a file that cannot be parsed is never
//! overwritten. Only catalog names get in, which keeps it small.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::todos::write_atomic;

const FILE_NAME: &str = "stopped.json";
const FILE_VERSION: u32 = 1;

/// What the placeholder row shows about a stopped service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stopped {
    /// The container's image when it was stopped.
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    /// RFC 3339, UTC.
    pub stopped_at: String,
}

/// On-disk layout of `stopped.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Data {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    services: BTreeMap<String, Stopped>,
}

pub struct StoppedStore {
    path: PathBuf,
    /// Serialises writes; held across the file write.
    write: tokio::sync::Mutex<()>,
    /// `Err` holds why the file could not be loaded.
    data: Mutex<Result<Data, String>>,
}

impl StoppedStore {
    /// Loads `<dir>/stopped.json`. A missing file is an empty set; an
    /// unreadable one is logged and makes every later change fail.
    #[must_use]
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        let data = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Data>(&bytes)
                .map_err(|e| format!("cannot parse {}: {e}", path.display())),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(Data::default())
            }
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        };
        match &data {
            Ok(d) if !d.services.is_empty() => info!(
                path = %path.display(),
                services = d.services.len(),
                "services stopped through systemd loaded"
            ),
            Ok(_) => {}
            Err(e) => error!(error = %e, "stopped services unavailable"),
        }
        Self {
            path,
            write: tokio::sync::Mutex::new(()),
            data: Mutex::new(data),
        }
    }

    /// Every remembered service, by name.
    #[must_use]
    pub fn all(&self) -> BTreeMap<String, Stopped> {
        lock(&self.data)
            .as_ref()
            .map(|d| d.services.clone())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        lock(&self.data)
            .as_ref()
            .is_ok_and(|d| d.services.contains_key(name))
    }

    /// Remembers (or refreshes) `name`.
    ///
    /// # Errors
    /// When the file could not be loaded or written; nothing changes then.
    pub async fn insert(&self, name: &str, stopped: Stopped) -> Result<(), String> {
        self.update(|d| {
            d.services.insert(name.to_owned(), stopped);
            true
        })
        .await
    }

    /// Forgets `name`; writes only when it was there.
    ///
    /// # Errors
    /// When the file could not be loaded or written; nothing changes then.
    pub async fn remove(&self, name: &str) -> Result<(), String> {
        self.update(|d| d.services.remove(name).is_some()).await
    }

    /// Applies `change` to a copy and, when it reports a change, writes the
    /// copy and only then makes it current.
    async fn update(&self, change: impl FnOnce(&mut Data) -> bool) -> Result<(), String> {
        let _write = self.write.lock().await;
        let mut data = lock(&self.data).as_ref().map_err(Clone::clone)?.clone();
        if !change(&mut data) {
            return Ok(());
        }
        data.version = FILE_VERSION;
        let bytes = serde_json::to_vec_pretty(&data)
            .map_err(|e| format!("cannot encode stopped services: {e}"))?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || write_atomic(&path, &bytes))
            .await
            .map_err(|e| format!("cannot save stopped services: {e}"))?
            .map_err(|e| format!("cannot save {}: {e}", self.path.display()))?;
        *lock(&self.data) = Ok(data);
        Ok(())
    }
}

/// Plain data, valid after any panic.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todos::tests::TempDir;

    fn stopped(image: &str) -> Stopped {
        Stopped {
            image: Some(image.into()),
            group: None,
            stopped_at: "2026-10-08T12:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn survives_a_reopen() {
        let dir = TempDir::new();
        let store = StoppedStore::open(dir.path());
        assert_eq!(store.all(), BTreeMap::new());
        store.insert("tool", stopped("me/tool:1")).await.unwrap();
        store.insert("other", stopped("me/other:1")).await.unwrap();
        store.remove("other").await.unwrap();
        assert!(store.contains("tool") && !store.contains("other"));

        let again = StoppedStore::open(dir.path());
        assert_eq!(
            again.all(),
            BTreeMap::from([("tool".to_owned(), stopped("me/tool:1"))])
        );
        let text = fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
    }

    #[tokio::test]
    async fn removing_an_unknown_name_writes_nothing() {
        let dir = TempDir::new();
        let store = StoppedStore::open(dir.path());
        store.remove("tool").await.unwrap();
        assert!(!dir.path().join(FILE_NAME).exists());
    }

    #[tokio::test]
    async fn an_unreadable_file_is_never_overwritten() {
        let dir = TempDir::new();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, "not json").unwrap();
        let store = StoppedStore::open(dir.path());
        assert_eq!(store.all(), BTreeMap::new());
        let err = store.insert("tool", stopped("x")).await.unwrap_err();
        assert!(err.contains("cannot parse"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
    }
}
