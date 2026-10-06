//! Per-service TODO items, persisted as one JSON file in `CUTHULU_DATA_DIR`.
//!
//! Items are keyed by service *name*, which survives container re-creation
//! (the id does not). Items of services that no longer exist are kept.
//!
//! The whole file is cached in memory and loaded once at startup. Every
//! change is serialised behind a mutex, written to a temporary file and
//! renamed over the old one, and only then applied to the cache, so the
//! cache never holds anything the disk does not.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{error, info};

/// Most items one service can hold.
pub const MAX_TODOS_PER_SERVICE: usize = 200;
/// Longest item text, in characters.
pub const MAX_TODO_CHARS: usize = 500;

const FILE_NAME: &str = "todos.json";
const FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub id: u64,
    pub text: String,
    pub done: bool,
    /// RFC 3339, UTC.
    pub created_at: String,
    /// RFC 3339, UTC; set while `done`.
    #[serde(default)]
    pub done_at: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TodoError {
    #[error("{0}")]
    Invalid(String),
    #[error("todo {0} not found")]
    NotFound(u64),
    /// The file could not be read at startup or written now.
    #[error("{0}")]
    Storage(String),
}

/// On-disk layout of `todos.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Data {
    version: u32,
    /// Next id to hand out; ids are never reused.
    next_id: u64,
    services: BTreeMap<String, Vec<Todo>>,
}

pub struct TodoStore {
    path: PathBuf,
    /// `Err` holds why the file could not be loaded; the store then refuses
    /// every request instead of overwriting data it could not read.
    data: Mutex<Result<Data, String>>,
}

impl TodoStore {
    /// Loads `<dir>/todos.json`. A missing file is an empty store; an
    /// unreadable one is logged and makes every later request fail, so the
    /// app still starts.
    #[must_use]
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        let data = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Data>(&bytes)
                .map_err(|e| format!("cannot parse {}: {e}", path.display())),
            // Nothing there yet; whether the dir is writable shows on the first write.
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
            Ok(d) => info!(
                path = %path.display(),
                services = d.services.len(),
                "todos loaded"
            ),
            Err(e) => error!(error = %e, "todos unavailable"),
        }
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The items of `service`, oldest first.
    pub async fn list(&self, service: &str) -> Result<Vec<Todo>, TodoError> {
        let guard = self.data.lock().await;
        let data = guard.as_ref().map_err(|e| TodoError::Storage(e.clone()))?;
        Ok(data.services.get(service).cloned().unwrap_or_default())
    }

    /// Adds an item and returns the service's updated list.
    pub async fn add(&self, service: &str, text: &str) -> Result<Vec<Todo>, TodoError> {
        let text = validate(text)?;
        self.update(service, |data| {
            let id = data.next_id.max(1);
            let list = data.services.entry(service.to_owned()).or_default();
            if list.len() >= MAX_TODOS_PER_SERVICE {
                return Err(TodoError::Invalid(format!(
                    "a service can have at most {MAX_TODOS_PER_SERVICE} todos"
                )));
            }
            list.push(Todo {
                id,
                text,
                done: false,
                created_at: now(),
                done_at: None,
            });
            data.next_id = id + 1;
            Ok(())
        })
        .await
    }

    /// Flips `done` and returns the service's updated list.
    pub async fn toggle(&self, service: &str, id: u64) -> Result<Vec<Todo>, TodoError> {
        self.update(service, |data| {
            let todo = data
                .services
                .get_mut(service)
                .and_then(|list| list.iter_mut().find(|t| t.id == id))
                .ok_or(TodoError::NotFound(id))?;
            todo.done = !todo.done;
            todo.done_at = todo.done.then(now);
            Ok(())
        })
        .await
    }

    /// Removes an item and returns the service's updated list.
    pub async fn delete(&self, service: &str, id: u64) -> Result<Vec<Todo>, TodoError> {
        self.update(service, |data| {
            let list = data
                .services
                .get_mut(service)
                .ok_or(TodoError::NotFound(id))?;
            let i = list
                .iter()
                .position(|t| t.id == id)
                .ok_or(TodoError::NotFound(id))?;
            list.remove(i);
            if list.is_empty() {
                data.services.remove(service);
            }
            Ok(())
        })
        .await
    }

    /// Applies `change` to a copy of the data, persists it, then commits it.
    async fn update(
        &self,
        service: &str,
        change: impl FnOnce(&mut Data) -> Result<(), TodoError>,
    ) -> Result<Vec<Todo>, TodoError> {
        let mut guard = self.data.lock().await;
        let mut data = guard
            .as_ref()
            .map_err(|e| TodoError::Storage(e.clone()))?
            .clone();
        change(&mut data)?;
        data.version = FILE_VERSION;

        let bytes = serde_json::to_vec_pretty(&data)
            .map_err(|e| TodoError::Storage(format!("cannot encode todos: {e}")))?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || write_atomic(&path, &bytes))
            .await
            .map_err(|e| TodoError::Storage(format!("cannot save todos: {e}")))?
            .map_err(|e| {
                TodoError::Storage(format!("cannot save todos to {}: {e}", self.path.display()))
            })?;

        let list = data.services.get(service).cloned().unwrap_or_default();
        *guard = Ok(data);
        Ok(list)
    }
}

/// Trims `text` and checks it is non-empty, short enough and single-line.
fn validate(text: &str) -> Result<String, TodoError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(TodoError::Invalid("todo text is empty".to_owned()));
    }
    if text.chars().count() > MAX_TODO_CHARS {
        return Err(TodoError::Invalid(format!(
            "todo text is longer than {MAX_TODO_CHARS} characters"
        )));
    }
    if text.chars().any(char::is_control) {
        return Err(TodoError::Invalid(
            "todo text must not contain control characters".to_owned(),
        ));
    }
    Ok(text.to_owned())
}

/// Writes `bytes` to a temporary file next to `path` and renames it over
/// `path`, so readers see either the old or the new file, never half of one.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    // Persist the rename itself; not every platform can sync a directory.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    rfc3339(secs)
}

/// Formats Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil-from-days, H. Hinnant: http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A fresh directory under the system temp dir, removed on drop.
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "cuthulu-todos-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn formats_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_791_234_567), "2026-10-05T21:09:27Z");
        assert_eq!(rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn validates_text() {
        assert_eq!(validate("  fix it \n").unwrap(), "fix it");
        assert!(matches!(validate(" \t "), Err(TodoError::Invalid(_))));
        assert!(matches!(validate("a\nb"), Err(TodoError::Invalid(_))));
        assert!(validate(&"é".repeat(MAX_TODO_CHARS)).is_ok());
        assert!(validate(&"x".repeat(MAX_TODO_CHARS + 1)).is_err());
    }

    #[tokio::test]
    async fn add_toggle_delete_and_persist() {
        let dir = TempDir::new();
        let store = TodoStore::open(dir.path());
        assert!(store.list("web").await.unwrap().is_empty());

        store.add("web", "first").await.unwrap();
        let list = store.add("web", "second").await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].id, list[1].id), (1, 2));
        assert!(!list[0].done && list[0].done_at.is_none());
        store.add("db", "other service").await.unwrap();

        let list = store.toggle("web", 1).await.unwrap();
        assert!(list[0].done && list[0].done_at.is_some());
        let list = store.toggle("web", 1).await.unwrap();
        assert!(!list[0].done && list[0].done_at.is_none());
        store.toggle("web", 2).await.unwrap();

        let list = store.delete("web", 1).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].text, "second");

        // A new store reads the same state back, and ids are not reused.
        let reopened = TodoStore::open(dir.path());
        let list = reopened.list("web").await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].done);
        assert_eq!(reopened.list("db").await.unwrap().len(), 1);
        let list = reopened.add("web", "third").await.unwrap();
        assert_eq!(list[1].id, 4);
        assert!(!dir.path().join("todos.json.tmp").exists());
    }

    #[tokio::test]
    async fn last_delete_drops_the_service_key() {
        let dir = TempDir::new();
        let store = TodoStore::open(dir.path());
        store.add("web", "x").await.unwrap();
        store.delete("web", 1).await.unwrap();
        let file = fs::read_to_string(store.path()).unwrap();
        assert!(!file.contains("\"web\""), "{file}");
    }

    #[tokio::test]
    async fn unknown_ids_are_not_found() {
        let dir = TempDir::new();
        let store = TodoStore::open(dir.path());
        store.add("web", "x").await.unwrap();
        assert!(matches!(
            store.toggle("web", 9).await,
            Err(TodoError::NotFound(9))
        ));
        assert!(matches!(
            store.delete("db", 1).await,
            Err(TodoError::NotFound(1))
        ));
    }

    #[tokio::test]
    async fn caps_items_per_service() {
        let dir = TempDir::new();
        let store = TodoStore::open(dir.path());
        for i in 0..MAX_TODOS_PER_SERVICE {
            store.add("web", &format!("item {i}")).await.unwrap();
        }
        assert!(matches!(
            store.add("web", "one too many").await,
            Err(TodoError::Invalid(_))
        ));
        assert_eq!(
            store.list("web").await.unwrap().len(),
            MAX_TODOS_PER_SERVICE
        );
        assert!(store.add("db", "other services are fine").await.is_ok());
    }

    #[tokio::test]
    async fn unwritable_dir_fails_writes_but_keeps_reads() {
        let dir = TempDir::new();
        // A path below a regular file can never be created, even as root.
        let blocker = dir.path().join("file");
        fs::write(&blocker, "").unwrap();
        let store = TodoStore::open(&blocker.join("data"));

        assert!(store.list("web").await.unwrap().is_empty());
        let err = store.add("web", "x").await.unwrap_err();
        assert!(matches!(err, TodoError::Storage(_)));
        assert!(err.to_string().contains("cannot save todos"), "{err}");
        // The failed write did not leak into the cache.
        assert!(store.list("web").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn corrupt_file_is_never_overwritten() {
        let dir = TempDir::new();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, "{ not json").unwrap();
        let store = TodoStore::open(dir.path());

        assert!(matches!(
            store.list("web").await,
            Err(TodoError::Storage(_))
        ));
        assert!(matches!(
            store.add("web", "x").await,
            Err(TodoError::Storage(_))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
    }
}
