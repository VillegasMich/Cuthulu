//! Password-gated editing of a service's env file on the host.
//!
//! Only services in the embedded catalog ([`crate::catalog`]) qualify: the
//! entry names the systemd unit and env file. Every request checks the
//! user's sudo password on the host (never stored), reads the file, and a
//! save writes it back (after a `.bak` copy) and restarts the unit. Host
//! access goes through [`HostControl`]; values never reach a log line.

pub mod file;
pub mod host;

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use self::file::{EditError, EnvText, Var};
use self::host::{HostUser, Target, TargetError};
use crate::catalog::{self, Entry};
use crate::model::Service;
use crate::providers::{HostControl, HostError, HostOutput, HostScript};

/// Wrong passwords tolerated within [`FAILURE_WINDOW`] before every
/// password-gated request is refused until the window passes.
pub const MAX_FAILURES: usize = 5;
pub const FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Probing and reading (sudo may pause a few seconds after a wrong password).
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Writing and restarting: the restart waits for the unit to stop.
const WRITE_TIMEOUT: Duration = Duration::from_secs(90);
/// Longest password accepted, in bytes.
const MAX_PASSWORD_BYTES: usize = 1024;
/// Longest piece of host stderr put into an error message.
const MAX_DETAIL_CHARS: usize = 300;

/// A sudo password. Never printed, logged or stored.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Password(String);

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

impl Password {
    #[cfg(test)]
    pub(crate) fn new(p: &str) -> Self {
        Self(p.to_owned())
    }
}

/// Whether the detail page offers the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Availability {
    Ready,
    /// `CUTHULU_HOST_USER` is unset, so there is no one to ask sudo for.
    NoUser,
}

/// What the detail page shows about a service's env file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvInfo {
    pub file: String,
    pub unit: String,
    pub scope: String,
    pub availability: Availability,
}

/// One variable as sent to the editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoadedVar {
    pub key: String,
    pub value: String,
    /// Masked until revealed (see [`file::is_secret`]).
    pub secret: bool,
}

/// Body of a successful load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Loaded {
    pub file: String,
    pub unit: String,
    pub scope: String,
    /// The user whose password was checked.
    pub user: String,
    /// Fingerprint of the file, sent back on save to detect concurrent edits.
    pub version: String,
    pub vars: Vec<LoadedVar>,
}

/// Body of a successful save.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Saved {
    pub unit: String,
    /// Cuthulu restarts itself: the page loses its connection for a while.
    pub restarting_self: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum EnvEditError {
    #[error("`{0}` has no editable env file")]
    NotEditable(String),
    #[error("{0}")]
    Target(#[from] TargetError),
    #[error("too many wrong passwords; try again in {}s", .0.as_secs().max(1))]
    RateLimited(Duration),
    #[error("{0}")]
    BadPassword(&'static str),
    #[error("wrong password")]
    WrongPassword,
    #[error("{0} may not use sudo on the host")]
    NotSudoer(String),
    #[error("{0} does not exist on the host")]
    NoFile(String),
    #[error("{0} is larger than {max} bytes", max = host::MAX_FILE_BYTES)]
    TooLarge(String),
    #[error("{file} sets {key} twice; fix it by hand first")]
    FileDuplicate { file: String, key: String },
    #[error("{0} changed since it was opened; reopen the editor")]
    Stale(String),
    #[error("{0}")]
    Invalid(#[from] EditError),
    #[error("{0}")]
    Host(#[from] HostError),
    #[error("{0}")]
    Failed(String),
    #[error("saved, but restarting {unit} failed: {detail}")]
    RestartFailed { unit: String, detail: String },
}

pub type Result<T, E = EnvEditError> = std::result::Result<T, E>;

/// Recent wrong passwords, shared by all clients (one operator).
#[derive(Debug, Default)]
struct Limiter {
    failures: VecDeque<Instant>,
}

impl Limiter {
    fn prune(&mut self, now: Instant) {
        while self
            .failures
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= FAILURE_WINDOW)
        {
            self.failures.pop_front();
        }
    }

    /// `Err(wait)` while [`MAX_FAILURES`] wrong passwords are in the window.
    fn check(&mut self, now: Instant) -> Result<(), Duration> {
        self.prune(now);
        match self.failures.front() {
            Some(oldest) if self.failures.len() >= MAX_FAILURES => {
                Err(FAILURE_WINDOW.saturating_sub(now.saturating_duration_since(*oldest)))
            }
            _ => Ok(()),
        }
    }

    fn fail(&mut self, now: Instant) {
        self.prune(now);
        if self.failures.len() >= MAX_FAILURES {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
    }
}

pub struct EnvEditor {
    host: Option<Arc<dyn HostControl>>,
    catalog: Vec<Entry>,
    host_user: Option<HostUser>,
    read_only: bool,
    limiter: Mutex<Limiter>,
    /// One host operation at a time, so two saves cannot interleave.
    busy: tokio::sync::Mutex<()>,
}

impl EnvEditor {
    /// An editor for the embedded catalog; `host` is `None` when nothing can
    /// reach the host (the editor is then never offered).
    #[must_use]
    pub fn new(
        host: Option<Arc<dyn HostControl>>,
        host_user: Option<HostUser>,
        read_only: bool,
    ) -> Self {
        Self::with_catalog(host, catalog::embedded(), host_user, read_only)
    }

    #[must_use]
    pub fn with_catalog(
        host: Option<Arc<dyn HostControl>>,
        catalog: Vec<Entry>,
        host_user: Option<HostUser>,
        read_only: bool,
    ) -> Self {
        Self {
            host,
            catalog,
            host_user,
            read_only,
            limiter: Mutex::default(),
            busy: tokio::sync::Mutex::new(()),
        }
    }

    /// What to show on `service`'s detail page; `None` hides the editor.
    #[must_use]
    pub fn info(&self, service: &Service) -> Option<EnvInfo> {
        if self.read_only || self.host.is_none() {
            return None;
        }
        let entry = catalog::find(&self.catalog, &service.name)?;
        let target = Target::new(entry).ok()?;
        Some(EnvInfo {
            file: target.env_file.clone(),
            unit: target.unit().to_owned(),
            scope: target.scope().to_string(),
            availability: if self.host_user.is_some() {
                Availability::Ready
            } else {
                Availability::NoUser
            },
        })
    }

    /// Checks `password` and returns the env file's variables.
    ///
    /// # Errors
    /// See [`EnvEditError`]; a wrong password counts toward the rate limit.
    pub async fn load(&self, service: &Service, password: &Password) -> Result<Loaded> {
        let (host, target) = self.prepare(service, password)?;
        let _busy = self.busy.lock().await;
        let (user, text) = self.unlock(host, &target, password).await?;
        let env = EnvText::parse(&text).map_err(|e| file_error(&target, e))?;
        info!(service = %service.name, file = %target.env_file, "env file opened");
        Ok(Loaded {
            file: target.env_file.clone(),
            unit: target.unit().to_owned(),
            scope: target.scope().to_string(),
            user: user.to_string(),
            version: file::version(&text),
            vars: env
                .vars()
                .into_iter()
                .map(|Var { key, value }| LoadedVar {
                    secret: file::is_secret(&key),
                    key,
                    value,
                })
                .collect(),
        })
    }

    /// Checks `password`, writes `vars` into the env file (keeping comments,
    /// order and untouched lines) and restarts the unit.
    ///
    /// # Errors
    /// See [`EnvEditError`]; [`EnvEditError::Stale`] when the file no longer
    /// matches `version` from the load.
    pub async fn save(
        &self,
        service: &Service,
        password: &Password,
        version: &str,
        vars: &[Var],
    ) -> Result<Saved> {
        let (host, target) = self.prepare(service, password)?;
        // Refuse bad input before asking the host anything.
        EnvText::parse("")
            .and_then(|empty| empty.apply(vars))
            .map_err(EnvEditError::Invalid)?;
        let _busy = self.busy.lock().await;
        let (user, text) = self.unlock(host, &target, password).await?;
        if file::version(&text) != version {
            return Err(EnvEditError::Stale(target.env_file.clone()));
        }
        let env = EnvText::parse(&text).map_err(|e| file_error(&target, e))?;
        let new = env.apply(vars)?;
        if new.len() as u64 > host::MAX_FILE_BYTES {
            return Err(EnvEditError::TooLarge(target.env_file.clone()));
        }

        let script = host::write(&target, self.host_user.as_ref(), &user, service.is_self)?;
        let out = run(host, &script, new.as_bytes(), WRITE_TIMEOUT).await?;
        match out.status {
            0 => {}
            host::EXIT_RESTART => {
                return Err(EnvEditError::RestartFailed {
                    unit: target.unit().to_owned(),
                    detail: detail(&out.stderr),
                });
            }
            status => return Err(status_error(&target, status, &out)),
        }
        info!(
            service = %service.name,
            file = %target.env_file,
            unit = target.unit(),
            "env file saved, unit restarting"
        );
        Ok(Saved {
            unit: target.unit().to_owned(),
            restarting_self: service.is_self,
        })
    }

    /// The checks every request passes before touching the host.
    fn prepare(
        &self,
        service: &Service,
        password: &Password,
    ) -> Result<(&dyn HostControl, Target)> {
        let not_editable = || EnvEditError::NotEditable(service.name.clone());
        let host = self.host.as_deref().ok_or_else(not_editable)?;
        let entry = catalog::find(&self.catalog, &service.name).ok_or_else(not_editable)?;
        let target = Target::new(entry)?;
        if self.host_user.is_none() {
            return Err(TargetError::NoUser.into());
        }
        self.limiter()
            .check(Instant::now())
            .map_err(EnvEditError::RateLimited)?;
        let p = password.0.as_str();
        if p.is_empty() {
            return Err(EnvEditError::BadPassword("enter the sudo password"));
        }
        if p.len() > MAX_PASSWORD_BYTES || p.contains(['\n', '\r', '\0']) {
            return Err(EnvEditError::BadPassword(
                "the password must be one line of at most 1024 bytes",
            ));
        }
        Ok((host, target))
    }

    /// Resolves whose password to check, checks it, and returns the file.
    async fn unlock(
        &self,
        host: &dyn HostControl,
        target: &Target,
        password: &Password,
    ) -> Result<(HostUser, String)> {
        let host_user = self.host_user.as_ref();
        let out = run(host, &host::probe(target, host_user)?, b"", READ_TIMEOUT).await?;
        if out.status != 0 {
            return Err(status_error(target, out.status, &out));
        }
        let probe = host::parse_probe(&String::from_utf8_lossy(&out.stdout))
            .ok_or_else(|| EnvEditError::Failed(format!("cannot inspect {}", target.env_file)))?;
        if probe.size > host::MAX_FILE_BYTES {
            return Err(EnvEditError::TooLarge(target.env_file.clone()));
        }
        let user = host::resolve_user(&probe, host_user)?;

        let mut stdin = Vec::with_capacity(password.0.len() + 1);
        stdin.extend_from_slice(password.0.as_bytes());
        stdin.push(b'\n');
        let out = run(
            host,
            &host::read(target, host_user, &user)?,
            &stdin,
            READ_TIMEOUT,
        )
        .await?;
        match out.status {
            0 => {}
            host::EXIT_SUDO if refused_sudo(&out.stderr) => {
                return Err(EnvEditError::NotSudoer(user.to_string()));
            }
            host::EXIT_SUDO => {
                warn!(user = %user, file = %target.env_file, "wrong sudo password");
                self.limiter().fail(Instant::now());
                return Err(EnvEditError::WrongPassword);
            }
            status => return Err(status_error(target, status, &out)),
        }
        let text = String::from_utf8(out.stdout)
            .map_err(|_| EnvEditError::Failed(format!("{} is not UTF-8 text", target.env_file)))?;
        Ok((user, text))
    }

    fn limiter(&self) -> std::sync::MutexGuard<'_, Limiter> {
        // Plain data, valid after any panic.
        self.limiter.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

async fn run(
    host: &dyn HostControl,
    script: &HostScript,
    stdin: &[u8],
    timeout: Duration,
) -> Result<HostOutput> {
    Ok(host.run(script, stdin, timeout).await?)
}

/// sudo's messages (C locale) for a user without sudo rights.
fn refused_sudo(stderr: &str) -> bool {
    [
        "is not in the sudoers file",
        "is not allowed to",
        "may not run sudo",
    ]
    .iter()
    .any(|m| stderr.contains(m))
}

fn file_error(target: &Target, e: EditError) -> EnvEditError {
    match e {
        EditError::Duplicate(key) => EnvEditError::FileDuplicate {
            file: target.env_file.clone(),
            key,
        },
        e => EnvEditError::Invalid(e),
    }
}

fn status_error(target: &Target, status: i32, out: &HostOutput) -> EnvEditError {
    let file = target.env_file.clone();
    match status {
        host::EXIT_NO_FILE => EnvEditError::NoFile(file),
        host::EXIT_NO_USER => EnvEditError::Failed(
            "the host user does not exist on the host; check CUTHULU_HOST_USER".to_owned(),
        ),
        host::EXIT_TOO_LARGE => EnvEditError::TooLarge(file),
        host::EXIT_FILE_IO => {
            EnvEditError::Failed(format!("cannot update {file}: {}", detail(&out.stderr)))
        }
        // 127 / 126: nsenter or a host command is missing.
        status => EnvEditError::Failed(format!(
            "the host command failed (exit {status}): {}",
            detail(&out.stderr)
        )),
    }
}

/// The last lines of host stderr, short, for an error message. It never
/// holds a secret: the scripts never echo stdin.
fn detail(stderr: &str) -> String {
    let text = stderr.trim();
    if text.is_empty() {
        return "no details".to_owned();
    }
    let count = text.chars().count();
    if count <= MAX_DETAIL_CHARS {
        text.to_owned()
    } else {
        format!(
            "…{}",
            text.chars()
                .skip(count - MAX_DETAIL_CHARS)
                .collect::<String>()
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::catalog::UnitScope;
    use crate::model::ServiceState;
    use crate::registry::tests::service;

    /// A [`HostControl`] that answers each script by its `op` with canned
    /// output, and records what ran (stdin included, to check secrets).
    #[derive(Default)]
    pub(crate) struct MockHost {
        pub answers: Mutex<Vec<(&'static str, HostOutput)>>,
        pub calls: Mutex<Vec<(HostScript, Vec<u8>)>>,
    }

    impl MockHost {
        pub(crate) fn answer(&self, op: &'static str, status: i32, stdout: &str, stderr: &str) {
            self.answers.lock().unwrap().push((
                op,
                HostOutput {
                    status,
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: stderr.to_owned(),
                },
            ));
        }

        /// A probe and a read of `text`, owned by root.
        pub(crate) fn file(&self, text: &str) {
            self.answer("probe", 0, &format!("0 root {}\n", text.len()), "");
            self.answer("read", 0, text, "");
        }

        pub(crate) fn ops(&self) -> Vec<&'static str> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(s, _)| s.op)
                .collect()
        }
    }

    #[async_trait]
    impl HostControl for MockHost {
        async fn run(
            &self,
            script: &HostScript,
            stdin: &[u8],
            _timeout: Duration,
        ) -> std::result::Result<HostOutput, HostError> {
            self.calls
                .lock()
                .unwrap()
                .push((script.clone(), stdin.to_vec()));
            let mut answers = self.answers.lock().unwrap();
            let at = answers
                .iter()
                .position(|(op, _)| *op == script.op)
                .unwrap_or_else(|| panic!("unexpected host script `{}`", script.op));
            Ok(answers.remove(at).1)
        }
    }

    pub(crate) fn test_catalog() -> Vec<Entry> {
        vec![Entry {
            name: "tool".into(),
            container: "tool".into(),
            unit: "tool".into(),
            scope: UnitScope::System,
            env_file: "/etc/tool/env".into(),
        }]
    }

    pub(crate) fn editor(host: &Arc<MockHost>, host_user: Option<&str>) -> EnvEditor {
        EnvEditor::with_catalog(
            Some(Arc::clone(host) as Arc<dyn HostControl>),
            test_catalog(),
            host_user.map(|u| HostUser::parse(u).unwrap()),
            false,
        )
    }

    fn var(key: &str, value: &str) -> Var {
        Var {
            key: key.into(),
            value: value.into(),
        }
    }

    #[test]
    fn limiter_blocks_after_five_failures_until_the_window_passes() {
        let mut l = Limiter::default();
        let t0 = Instant::now();
        for i in 0..4 {
            l.fail(t0 + Duration::from_secs(i));
            assert!(l.check(t0 + Duration::from_secs(i)).is_ok());
        }
        l.fail(t0 + Duration::from_secs(4));
        let wait = l.check(t0 + Duration::from_secs(10)).unwrap_err();
        assert_eq!(wait + Duration::from_secs(10), FAILURE_WINDOW);
        // The oldest failure leaves the window: four remain, requests pass.
        assert!(l.check(t0 + FAILURE_WINDOW).is_ok());
        assert_eq!(l.failures.len(), 4);
        // Never holds more than it needs.
        for i in 0..20 {
            l.fail(t0 + FAILURE_WINDOW + Duration::from_millis(i));
        }
        assert_eq!(l.failures.len(), MAX_FAILURES);
    }

    #[test]
    fn info_only_for_catalog_services() {
        let host = Arc::new(MockHost::default());
        let tool = service("tool", ServiceState::Running);
        let ready = editor(&host, Some("manuel")).info(&tool).unwrap();
        assert_eq!(ready.availability, Availability::Ready);
        assert_eq!(
            (ready.file.as_str(), ready.unit.as_str()),
            ("/etc/tool/env", "tool")
        );
        assert_eq!(
            editor(&host, None).info(&tool).unwrap().availability,
            Availability::NoUser
        );
        assert_eq!(
            editor(&host, Some("manuel")).info(&service("web", ServiceState::Running)),
            None
        );
        let ro = EnvEditor::with_catalog(Some(host.clone()), test_catalog(), None, true);
        assert_eq!(ro.info(&tool), None);
        let no_host = EnvEditor::with_catalog(None, test_catalog(), None, false);
        assert_eq!(no_host.info(&tool), None);
    }

    #[tokio::test]
    async fn load_checks_the_password_as_the_host_user_and_masks_secrets() {
        let host = Arc::new(MockHost::default());
        host.file("# c\nIMAGE=me/tool:1\nGITHUB_TOKEN=ghp_x\n");
        let ed = editor(&host, Some("manuel"));
        let tool = service("tool", ServiceState::Running);
        let loaded = ed.load(&tool, &Password::new("pw")).await.unwrap();
        assert_eq!(loaded.user, "manuel");
        assert_eq!(
            loaded.vars,
            [
                LoadedVar {
                    key: "IMAGE".into(),
                    value: "me/tool:1".into(),
                    secret: false
                },
                LoadedVar {
                    key: "GITHUB_TOKEN".into(),
                    value: "ghp_x".into(),
                    secret: true
                },
            ]
        );
        let calls = host.calls.lock().unwrap();
        assert_eq!(calls[0].1, b"", "the probe gets no secret");
        assert_eq!(calls[1].1, b"pw\n", "the password goes on stdin");
        assert!(!calls[1].0.text.contains("pw"), "never in the script");
        assert!(calls[1].0.text.contains("runuser -u 'manuel'"));
    }

    #[tokio::test]
    async fn owner_of_the_file_is_asked_before_the_host_user() {
        let host = Arc::new(MockHost::default());
        host.answer("probe", 0, "1001 alice 4\n", "");
        host.answer("read", 0, "A=1\n", "");
        let ed = editor(&host, Some("manuel"));
        let loaded = ed
            .load(
                &service("tool", ServiceState::Running),
                &Password::new("pw"),
            )
            .await
            .unwrap();
        assert_eq!(loaded.user, "alice");
    }

    #[tokio::test]
    async fn wrong_passwords_count_and_then_everything_is_limited() {
        let host = Arc::new(MockHost::default());
        let ed = editor(&host, Some("manuel"));
        let tool = service("tool", ServiceState::Running);
        for _ in 0..MAX_FAILURES {
            host.answer("probe", 0, "0 root 4\n", "");
            host.answer(
                "read",
                host::EXIT_SUDO,
                "",
                "Sorry, try again.\nsudo: 1 incorrect password attempt",
            );
            let err = ed.load(&tool, &Password::new("nope")).await.unwrap_err();
            assert!(matches!(err, EnvEditError::WrongPassword), "{err}");
        }
        let err = ed.load(&tool, &Password::new("right")).await.unwrap_err();
        assert!(matches!(err, EnvEditError::RateLimited(_)), "{err}");
        let err = ed
            .save(&tool, &Password::new("right"), "v", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, EnvEditError::RateLimited(_)), "{err}");
        assert_eq!(
            host.ops().len(),
            2 * MAX_FAILURES,
            "limited requests never reach the host"
        );
    }

    #[tokio::test]
    async fn sudo_refusal_is_not_a_wrong_password() {
        let host = Arc::new(MockHost::default());
        host.answer("probe", 0, "0 root 4\n", "");
        host.answer(
            "read",
            host::EXIT_SUDO,
            "",
            "manuel is not in the sudoers file.",
        );
        let ed = editor(&host, Some("manuel"));
        let err = ed
            .load(
                &service("tool", ServiceState::Running),
                &Password::new("pw"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EnvEditError::NotSudoer(_)), "{err}");
        assert!(ed.limiter().failures.is_empty());
    }

    #[tokio::test]
    async fn refuses_without_host_user_bad_passwords_and_unknown_services() {
        let host = Arc::new(MockHost::default());
        let tool = service("tool", ServiceState::Running);
        let err = editor(&host, None)
            .load(&tool, &Password::new("pw"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("CUTHULU_HOST_USER"), "{err}");
        let ed = editor(&host, Some("manuel"));
        for bad in ["", "a\nb", "a\0"] {
            let err = ed.load(&tool, &Password::new(bad)).await.unwrap_err();
            assert!(matches!(err, EnvEditError::BadPassword(_)), "{bad:?}");
        }
        let err = ed
            .load(&service("web", ServiceState::Running), &Password::new("pw"))
            .await
            .unwrap_err();
        assert!(matches!(err, EnvEditError::NotEditable(_)), "{err}");
        assert_eq!(host.ops(), Vec::<&str>::new());
    }

    #[tokio::test]
    async fn save_writes_the_edited_file_on_stdin_and_restarts() {
        let host = Arc::new(MockHost::default());
        let text = "# c\nIMAGE=me/tool:1\nTOKEN=old\nGONE=x\n";
        host.file(text);
        host.answer("write", 0, "", "");
        let ed = editor(&host, Some("manuel"));
        let tool = service("tool", ServiceState::Running);
        let saved = ed
            .save(
                &tool,
                &Password::new("pw"),
                &file::version(text),
                &[
                    var("IMAGE", "me/tool:1"),
                    var("TOKEN", "new"),
                    var("ADDED", "1"),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            saved,
            Saved {
                unit: "tool".into(),
                restarting_self: false
            }
        );
        let calls = host.calls.lock().unwrap();
        let (script, stdin) = &calls[2];
        assert_eq!(script.op, "write");
        assert_eq!(stdin, b"# c\nIMAGE=me/tool:1\nTOKEN=new\nADDED=1\n");
        assert!(
            script.text.contains("systemctl restart 'tool'"),
            "{}",
            script.text
        );
        assert!(!script.text.contains("new"), "values never in the script");
    }

    #[tokio::test]
    async fn saving_itself_does_not_wait_for_the_restart() {
        let host = Arc::new(MockHost::default());
        host.file("A=1\n");
        host.answer("write", 0, "", "");
        let mut me = service("tool", ServiceState::Running);
        me.is_self = true;
        let saved = editor(&host, Some("manuel"))
            .save(
                &me,
                &Password::new("pw"),
                &file::version("A=1\n"),
                &[var("A", "2")],
            )
            .await
            .unwrap();
        assert!(saved.restarting_self);
        let calls = host.calls.lock().unwrap();
        assert!(
            calls[2]
                .0
                .text
                .contains("systemctl --no-block restart 'tool'")
        );
    }

    #[tokio::test]
    async fn save_refuses_stale_versions_and_bad_input() {
        let host = Arc::new(MockHost::default());
        host.file("A=1\n");
        let ed = editor(&host, Some("manuel"));
        let tool = service("tool", ServiceState::Running);
        let err = ed
            .save(&tool, &Password::new("pw"), "0000", &[var("A", "2")])
            .await
            .unwrap_err();
        assert!(matches!(err, EnvEditError::Stale(_)), "{err}");
        assert_eq!(host.ops(), ["probe", "read"], "nothing written");

        let err = ed
            .save(&tool, &Password::new("pw"), "0000", &[var("A", "x\ny")])
            .await
            .unwrap_err();
        assert!(matches!(err, EnvEditError::Invalid(_)), "{err}");
        assert_eq!(host.ops().len(), 2, "bad input never reaches the host");
    }

    #[tokio::test]
    async fn host_failures_become_clear_errors() {
        let tool = service("tool", ServiceState::Running);
        let host = Arc::new(MockHost::default());
        host.answer("probe", host::EXIT_NO_FILE, "", "");
        let err = editor(&host, Some("manuel"))
            .load(&tool, &Password::new("pw"))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "/etc/tool/env does not exist on the host");

        let host = Arc::new(MockHost::default());
        host.file("A=1\n");
        host.answer(
            "write",
            host::EXIT_RESTART,
            "",
            "Job for tool.service failed.\n",
        );
        let err = editor(&host, Some("manuel"))
            .save(
                &tool,
                &Password::new("pw"),
                &file::version("A=1\n"),
                &[var("A", "2")],
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "saved, but restarting tool failed: Job for tool.service failed."
        );

        let host = Arc::new(MockHost::default());
        host.file("A=1\nA=2\n");
        let err = editor(&host, Some("manuel"))
            .load(&tool, &Password::new("pw"))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "/etc/tool/env sets A twice; fix it by hand first"
        );
    }

    #[test]
    fn password_debug_is_redacted() {
        assert_eq!(
            format!("{:?}", Password::new("hunter2")),
            "Password(<redacted>)"
        );
    }
}
