//! `RustDesk` on the host: its ID for the topbar, and a password-gated reset of
//! its permanent password.
//!
//! Not a [`Provider`](crate::providers::Provider): `RustDesk` is not a service
//! source. Whether it runs is read from the host's procfs (a process whose
//! `comm` is `rustdesk`), no privilege needed. The ID and whether a permanent
//! password is set come from one fixed script run as root on the host
//! through [`HostControl`] (`rustdesk --get-id`, and a yes/no test of the
//! `password` key in root's `RustDesk.toml` — the value never leaves the
//! host). Both are read on demand and cached for [`TTL`].
//!
//! `RustDesk` stores the permanent password hashed, so it cannot be shown.
//! Instead a new random one can be set (`rustdesk --password`) after the
//! host user's sudo password is checked, as in the env editor, and is
//! returned once. Neither password is stored or logged.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::envedit::host::{EXIT_SUDO, HostUser, TargetError, quote, sudo_check};
use crate::envedit::{Password, PasswordLimiter, detail, refused_sudo};
use crate::providers::{HostControl, HostError, HostScript};
use crate::system::proc::parse_pid_stat;

/// How long a lookup (success or failure) is reused.
pub const TTL: Duration = Duration::from_secs(60);
/// The `comm` of `RustDesk`'s processes (service, server, tray).
const COMM: &str = "rustdesk";
/// `rustdesk --get-id` answers from the running service within a second;
/// the rest is the helper container's start.
const STATUS_TIMEOUT: Duration = Duration::from_secs(30);
/// sudo pauses a few seconds after a wrong password.
const SET_TIMEOUT: Duration = Duration::from_secs(30);
/// Length of a generated password: 16 of 56 symbols, about 93 bits.
pub const PASSWORD_LEN: usize = 16;
/// Symbols of generated passwords: letters and digits that cannot be
/// confused with each other (no `0 O o 1 l I`).
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz";

/// `rustdesk` is not on the host's `PATH`.
pub const EXIT_NOT_INSTALLED: i32 = 30;
/// `rustdesk --password` did not print `Done!`.
pub const EXIT_NOT_SET: i32 = 31;

/// What `/api/rustdesk` returns: enough for the topbar button and its panel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    /// A `rustdesk` process runs on the host. False disables the button.
    pub available: bool,
    /// This machine's `RustDesk` ID; `null` when unknown.
    pub id: Option<String>,
    /// Whether a permanent password is set; `null` when unknown.
    pub password_set: Option<bool>,
    /// Whether `POST /api/rustdesk/password` may be tried.
    pub can_set_password: bool,
    /// Why not, when `can_set_password` is false.
    pub reason: Option<String>,
}

/// What the host told us, cached for [`TTL`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Probe {
    available: bool,
    id: Option<String>,
    password_set: Option<bool>,
}

/// A freshly generated `RustDesk` password, sent to the browser once.
#[derive(Clone, Serialize)]
#[serde(transparent)]
pub struct NewPassword(String);

impl fmt::Debug for NewPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NewPassword(<redacted>)")
    }
}

/// Body of a successful `POST /api/rustdesk/password`.
#[derive(Debug, Clone, Serialize)]
pub struct PasswordSet {
    /// The new permanent password, shown once.
    pub new_password: NewPassword,
}

#[derive(Debug, thiserror::Error)]
pub enum RustDeskError {
    /// Setting a password is not possible here (the reason is user-facing).
    #[error("{0}")]
    Unsupported(String),
    #[error("too many wrong passwords; try again in {}s", .0.as_secs().max(1))]
    RateLimited(Duration),
    #[error("{0}")]
    BadPassword(&'static str),
    #[error("wrong password")]
    WrongPassword,
    #[error("{0} may not use sudo on the host")]
    NotSudoer(String),
    #[error("rustdesk is not installed on the host")]
    NotInstalled,
    #[error("cannot create a password: {0}")]
    Random(#[from] std::io::Error),
    #[error("{0}")]
    Host(#[from] HostError),
    #[error("rustdesk did not set the password: {0}")]
    Failed(String),
}

pub type Result<T, E = RustDeskError> = std::result::Result<T, E>;

/// On-demand, cached view of `RustDesk` on the host, and its password reset.
pub struct RustDesk {
    proc_dir: PathBuf,
    host: Option<Arc<dyn HostControl>>,
    host_user: Option<HostUser>,
    read_only: bool,
    limiter: Arc<PasswordLimiter>,
    ttl: Duration,
    /// Held across a lookup, so concurrent requests share one.
    cache: Mutex<Option<(Instant, Probe)>>,
    /// One password change at a time.
    busy: Mutex<()>,
}

impl RustDesk {
    /// `host` is `None` when nothing can reach the host (no ID, no reset);
    /// `limiter` is shared with the env editor.
    #[must_use]
    pub fn new(
        config: &Config,
        host: Option<Arc<dyn HostControl>>,
        limiter: Arc<PasswordLimiter>,
    ) -> Self {
        Self::with_parts(
            config.proc_dir.clone(),
            host,
            config.host_user.clone(),
            config.read_only,
            limiter,
            TTL,
        )
    }

    #[must_use]
    pub fn with_parts(
        proc_dir: PathBuf,
        host: Option<Arc<dyn HostControl>>,
        host_user: Option<HostUser>,
        read_only: bool,
        limiter: Arc<PasswordLimiter>,
        ttl: Duration,
    ) -> Self {
        Self {
            proc_dir,
            host,
            host_user,
            read_only,
            limiter,
            ttl,
            cache: Mutex::new(None),
            busy: Mutex::new(()),
        }
    }

    /// The current status, from cache when it is younger than the TTL.
    pub async fn status(&self) -> Status {
        let probe = {
            let mut cache = self.cache.lock().await;
            match cache.as_ref() {
                Some((at, probe)) if at.elapsed() < self.ttl => probe.clone(),
                _ => {
                    let probe = self.lookup().await;
                    *cache = Some((Instant::now(), probe.clone()));
                    probe
                }
            }
        };
        let reason = self.unsupported(probe.available);
        Status {
            available: probe.available,
            id: probe.id,
            password_set: probe.password_set,
            can_set_password: reason.is_none(),
            reason,
        }
    }

    /// Checks `sudo_password` as `CUTHULU_HOST_USER`, sets a new random
    /// permanent password and returns it.
    ///
    /// # Errors
    /// See [`RustDeskError`]; a wrong password counts toward the rate limit
    /// shared with the env editor.
    pub async fn set_password(&self, sudo_password: &Password) -> Result<PasswordSet> {
        // Checks that need no host first, so refused requests never reach it.
        if let Some(reason) = self.unsupported(true) {
            return Err(RustDeskError::Unsupported(reason));
        }
        let (Some(host), Some(user)) = (self.host.as_deref(), self.host_user.as_ref()) else {
            return Err(RustDeskError::Unsupported(
                "cuthulu cannot run commands on this host".to_owned(),
            ));
        };
        self.limiter.check().map_err(RustDeskError::RateLimited)?;
        let sudo = sudo_password.check().map_err(RustDeskError::BadPassword)?;
        if let Some(reason) = self.unsupported(self.status().await.available) {
            return Err(RustDeskError::Unsupported(reason));
        }
        let new = generate_password()?;

        let _busy = self.busy.lock().await;
        let mut stdin = Vec::with_capacity(sudo.len() + PASSWORD_LEN + 2);
        for line in [sudo, new.as_str()] {
            stdin.extend_from_slice(line.as_bytes());
            stdin.push(b'\n');
        }
        let out = host.run(&set_script(user), &stdin, SET_TIMEOUT).await;
        // Whatever happened, the next status reads `password_set` afresh.
        *self.cache.lock().await = None;
        let out = out?;
        match out.status {
            0 => {}
            EXIT_SUDO if refused_sudo(&out.stderr) => {
                return Err(RustDeskError::NotSudoer(user.to_string()));
            }
            EXIT_SUDO => {
                warn!(user = %user, "wrong sudo password for the rustdesk password");
                self.limiter.fail();
                return Err(RustDeskError::WrongPassword);
            }
            EXIT_NOT_INSTALLED => return Err(RustDeskError::NotInstalled),
            status => {
                // rustdesk's messages never echo the password; scrub anyway.
                let text = detail(&out.stderr).replace(new.as_str(), "[redacted]");
                return Err(RustDeskError::Failed(if status == EXIT_NOT_SET {
                    text
                } else {
                    format!("the host command failed (exit {status}): {text}")
                }));
            }
        }
        info!(user = %user, "rustdesk permanent password changed");
        Ok(PasswordSet {
            new_password: NewPassword(new),
        })
    }

    /// Why a password cannot be set, or `None` when it can be tried.
    fn unsupported(&self, available: bool) -> Option<String> {
        if self.read_only {
            Some("cuthulu is running in read-only mode".to_owned())
        } else if !available {
            Some("rustdesk not found on this host".to_owned())
        } else if self.host.is_none() {
            Some("cuthulu cannot run commands on this host".to_owned())
        } else if self.host_user.is_none() {
            Some(TargetError::NoUser.to_string())
        } else {
            None
        }
    }

    async fn lookup(&self) -> Probe {
        let dir = self.proc_dir.clone();
        let available = tokio::task::spawn_blocking(move || is_running(&dir))
            .await
            .unwrap_or(false);
        if !available {
            return Probe::default();
        }
        let Some(host) = &self.host else {
            return Probe {
                available,
                ..Probe::default()
            };
        };
        let (id, password_set) = match host.run(&status_script(), b"", STATUS_TIMEOUT).await {
            Ok(out) if out.status == 0 => parse_status(&String::from_utf8_lossy(&out.stdout)),
            Ok(out) => {
                debug!(status = out.status, stderr = %detail(&out.stderr), "rustdesk status failed");
                (None, None)
            }
            Err(e) => {
                debug!(error = %e, "rustdesk status unavailable");
                (None, None)
            }
        };
        Probe {
            available,
            id,
            password_set,
        }
    }
}

/// Whether a process named `rustdesk` runs, from `<proc_dir>/<pid>/stat`.
/// Pids that vanish mid-scan or cannot be read are skipped.
#[must_use]
pub fn is_running(proc_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(proc_dir) else {
        return false;
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(is_pid))
        .any(|e| {
            std::fs::read_to_string(e.path().join("stat"))
                .ok()
                .and_then(|s| parse_pid_stat(&s).ok())
                .is_some_and(|stat| stat.comm == COMM)
        })
}

fn is_pid(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())
}

/// Prints `password yes|no` (the `password` key of root's `RustDesk.toml`
/// is non-empty; only the answer leaves the host), then `rustdesk --get-id`.
fn status_script() -> HostScript {
    HostScript {
        op: "rustdesk-status",
        text: format!(
            "set -u\nexport LC_ALL=C\n\
             command -v rustdesk >/dev/null 2>&1 || exit {EXIT_NOT_INSTALLED}\n\
             f=/root/.config/rustdesk/RustDesk.toml\n\
             if [ -r \"$f\" ] && awk {awk} \"$f\"; then echo 'password yes'; else echo 'password no'; fi\n\
             rustdesk --get-id 2>/dev/null\n",
            awk = quote(PASSWORD_SET_AWK),
        ),
    }
}

/// Exits 0 when a top-level `password` key has a non-empty value. Quotes
/// and blanks around the value do not count; a `[section]` ends the search.
const PASSWORD_SET_AWK: &str = r#"BEGIN { s = 1 }
/^\[/ { exit }
/^password[ \t]*=/ { v = $0; sub(/^[^=]*=/, "", v); gsub(/["' \t\r]/, "", v); s = (v == "") }
END { exit s }"#;

/// Parses the status script's output: whether a password is set, and the ID
/// (the last non-empty line) when it is a plausible `RustDesk` ID.
#[must_use]
pub fn parse_status(stdout: &str) -> (Option<String>, Option<bool>) {
    let mut lines = stdout.lines();
    let password_set = match lines.next().map(str::trim) {
        Some("password yes") => Some(true),
        Some("password no") => Some(false),
        _ => return (None, None),
    };
    let id = lines
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .and_then(valid_id)
        .map(ToOwned::to_owned);
    (id, password_set)
}

/// `id` if it looks like a `RustDesk` ID: digits, or a custom ID / host name
/// (`[A-Za-z0-9_-]`, at most 64 characters).
#[must_use]
pub fn valid_id(id: &str) -> Option<&str> {
    let ok = (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    ok.then_some(id)
}

/// Checks the sudo password (first stdin line) as `user`, then sets the
/// second stdin line as `RustDesk`'s permanent password. Secrets are read
/// into shell variables and fed through `printf` (a builtin), never placed in
/// the script; `rustdesk --password` only takes the new one as an argument.
fn set_script(user: &HostUser) -> HostScript {
    HostScript {
        op: "rustdesk-password",
        text: format!(
            "set -u\nexport LC_ALL=C\n\
             IFS= read -r pw || exit {EXIT_SUDO}\n\
             IFS= read -r new || exit {EXIT_NOT_SET}\n\
             command -v rustdesk >/dev/null 2>&1 || exit {EXIT_NOT_INSTALLED}\n\
             {sudo}unset pw\n\
             out=$(rustdesk --password \"$new\" 2>&1)\n\
             unset new\n\
             case \"$out\" in *'Done!'*) exit 0 ;; esac\n\
             printf '%s\\n' \"$out\" >&2\n\
             exit {EXIT_NOT_SET}\n",
            sudo = sudo_check(user, Some("printf '%s\\n' \"$pw\"")),
        ),
    }
}

/// A new random password from the OS's generator.
fn generate_password() -> std::io::Result<String> {
    use std::io::Read;
    let mut urandom = std::fs::File::open("/dev/urandom")?;
    // Rejection sampling discards about 12 % of bytes; 4× is plenty, and a
    // short draw (vanishingly unlikely) just reads again.
    let mut out = String::with_capacity(PASSWORD_LEN);
    let mut buf = [0u8; PASSWORD_LEN * 4];
    while out.len() < PASSWORD_LEN {
        urandom.read_exact(&mut buf)?;
        out.extend(password_chars(&buf).take(PASSWORD_LEN - out.len()));
    }
    Ok(out)
}

/// Maps random bytes to [`ALPHABET`] uniformly: bytes past the largest
/// multiple of its length are skipped, so no symbol is more likely.
fn password_chars(bytes: &[u8]) -> impl Iterator<Item = char> + '_ {
    let n = ALPHABET.len();
    let limit = 256 - 256 % n;
    bytes
        .iter()
        .map(|&b| usize::from(b))
        .filter(move |&b| b < limit)
        .map(move |b| char::from(ALPHABET[b % n]))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::envedit::tests::MockHost;
    use crate::todos::tests::TempDir;

    /// A procfs with one process per `comm`.
    pub(crate) fn fake_proc(comms: &[&str]) -> TempDir {
        let dir = TempDir::new();
        for (i, comm) in comms.iter().enumerate() {
            let pid = dir.path().join((100 + i).to_string());
            std::fs::create_dir_all(&pid).unwrap();
            std::fs::write(
                pid.join("stat"),
                format!(
                    "{} ({comm}) S 1 1 1 0 -1 4194560 0 0 0 0 5 3 0 0 20 0 1 0 77 0 0",
                    100 + i
                ),
            )
            .unwrap();
        }
        std::fs::create_dir_all(dir.path().join("self")).unwrap();
        dir
    }

    pub(crate) fn rustdesk(
        proc_dir: &Path,
        host: Option<&Arc<MockHost>>,
        host_user: Option<&str>,
        read_only: bool,
    ) -> RustDesk {
        RustDesk::with_parts(
            proc_dir.to_owned(),
            host.map(|h| Arc::clone(h) as Arc<dyn HostControl>),
            host_user.map(|u| HostUser::parse(u).unwrap()),
            read_only,
            Arc::default(),
            TTL,
        )
    }

    /// Nothing on the host: the button is disabled.
    #[must_use]
    pub(crate) fn disabled() -> RustDesk {
        rustdesk(Path::new("/nonexistent/cuthulu"), None, None, false)
    }

    #[test]
    fn finds_rustdesk_by_comm_only() {
        let dir = fake_proc(&["systemd", "rustdesk"]);
        assert!(is_running(dir.path()));
        for comms in [&["systemd", "rustdesk-x", "my rustdesk"][..], &[]] {
            let dir = fake_proc(comms);
            assert!(!is_running(dir.path()), "{comms:?}");
        }
        assert!(!is_running(Path::new("/nonexistent/cuthulu")));
    }

    #[test]
    fn parses_the_status_script() {
        assert_eq!(
            parse_status("password yes\n71223443\n"),
            (Some("71223443".into()), Some(true))
        );
        assert_eq!(
            parse_status("password no\nsome warning\nmy-box_2\n"),
            (Some("my-box_2".into()), Some(false))
        );
        assert_eq!(parse_status("password no\n"), (None, Some(false)));
        assert_eq!(parse_status(""), (None, None));
        assert_eq!(parse_status("71223443\n"), (None, None));
        assert_eq!(parse_status("password yes\n<html>\n"), (None, Some(true)));
    }

    #[test]
    fn ids_have_a_strict_charset() {
        for ok in ["71223443", "my-box", "a_b-C9"] {
            assert_eq!(valid_id(ok), Some(ok));
        }
        let long = "1".repeat(65);
        for bad in ["", "a b", "id;rm", "<b>", "ü", "a.b", long.as_str()] {
            assert_eq!(valid_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn generated_passwords_use_the_alphabet() {
        let p = generate_password().unwrap();
        assert_eq!(p.len(), PASSWORD_LEN);
        assert!(p.bytes().all(|b| ALPHABET.contains(&b)), "{p}");
        assert_ne!(p, generate_password().unwrap());
        for confusable in "0Oo1lI".chars() {
            assert!(!ALPHABET.contains(&(confusable as u8)));
        }
    }

    #[test]
    fn password_chars_are_uniform() {
        let all: Vec<u8> = (0..=255).collect();
        let chars: String = password_chars(&all).collect();
        // Each symbol exactly four times; bytes 224..=255 are skipped.
        assert_eq!(chars.len(), 4 * ALPHABET.len());
        for &a in ALPHABET {
            assert_eq!(chars.bytes().filter(|&c| c == a).count(), 4);
        }
    }

    #[test]
    fn scripts_hold_no_secret_and_quote_the_user() {
        let s = set_script(&HostUser::parse("manuel").unwrap());
        assert!(
            s.text
                .contains("runuser -u 'manuel' -- sudo -S -k -v -p ''"),
            "{}",
            s.text
        );
        assert!(
            s.text.contains("printf '%s\\n' \"$pw\" | runuser"),
            "{}",
            s.text
        );
        assert!(s.text.contains("rustdesk --password \"$new\""));
        assert!(status_script().text.contains("rustdesk --get-id"));
    }

    #[tokio::test]
    async fn unavailable_without_a_process_and_never_asks_the_host() {
        let dir = fake_proc(&["systemd"]);
        let host = Arc::new(MockHost::default());
        let rd = rustdesk(dir.path(), Some(&host), Some("manuel"), false);
        let st = rd.status().await;
        assert_eq!(
            st,
            Status {
                available: false,
                id: None,
                password_set: None,
                can_set_password: false,
                reason: Some("rustdesk not found on this host".into()),
            }
        );
        assert_eq!(host.ops(), Vec::<&str>::new());
    }

    #[tokio::test]
    async fn reads_id_and_password_state_once_per_ttl() {
        let dir = fake_proc(&["rustdesk"]);
        let host = Arc::new(MockHost::default());
        host.answer("rustdesk-status", 0, "password yes\n71223443\n", "");
        let rd = rustdesk(dir.path(), Some(&host), Some("manuel"), false);
        let st = rd.status().await;
        assert_eq!(
            st,
            Status {
                available: true,
                id: Some("71223443".into()),
                password_set: Some(true),
                can_set_password: true,
                reason: None,
            }
        );
        assert_eq!(rd.status().await, st, "cached");
        assert_eq!(host.ops(), ["rustdesk-status"]);
        assert_eq!(host.calls.lock().unwrap()[0].1, b"", "no secret for a read");
    }

    #[tokio::test]
    async fn host_failures_leave_the_id_unknown() {
        let dir = fake_proc(&["rustdesk"]);
        let host = Arc::new(MockHost::default());
        host.answer("rustdesk-status", EXIT_NOT_INSTALLED, "", "");
        let st = rustdesk(dir.path(), Some(&host), Some("manuel"), false)
            .status()
            .await;
        assert!(st.available);
        assert_eq!((st.id, st.password_set), (None, None));
    }

    #[tokio::test]
    async fn reasons_not_to_set_a_password() {
        let dir = fake_proc(&["rustdesk"]);
        let host = Arc::new(MockHost::default());
        host.answer("rustdesk-status", 0, "password no\n1\n", "");
        for (rd, why) in [
            (
                rustdesk(dir.path(), None, Some("manuel"), true),
                "read-only",
            ),
            (
                rustdesk(dir.path(), None, Some("manuel"), false),
                "cannot run commands",
            ),
            (
                rustdesk(dir.path(), Some(&host), None, false),
                "CUTHULU_HOST_USER",
            ),
        ] {
            let st = rd.status().await;
            assert!(st.available && !st.can_set_password, "{why}");
            assert!(st.reason.as_deref().unwrap().contains(why), "{st:?}");
            let err = set(&rd, "pw").await.unwrap_err();
            assert!(err.to_string().contains(why), "{err}");
        }
    }

    async fn set(rd: &RustDesk, sudo: &str) -> Result<PasswordSet> {
        rd.set_password(&Password::new(sudo)).await
    }

    #[tokio::test]
    async fn sets_a_new_password_given_on_stdin() {
        let dir = fake_proc(&["rustdesk"]);
        let host = Arc::new(MockHost::default());
        host.answer("rustdesk-status", 0, "password no\n71223443\n", "");
        host.answer("rustdesk-password", 0, "", "");
        host.answer("rustdesk-status", 0, "password yes\n71223443\n", "");
        let rd = rustdesk(dir.path(), Some(&host), Some("manuel"), false);
        assert_eq!(rd.status().await.password_set, Some(false));

        let set = set(&rd, "sudo-pw").await.unwrap();
        let new = set.new_password.0.clone();
        assert_eq!(new.len(), PASSWORD_LEN);
        assert!(!format!("{set:?}").contains(&new), "Debug redacts");
        {
            let calls = host.calls.lock().unwrap();
            let (script, stdin) = &calls[1];
            assert_eq!(script.op, "rustdesk-password");
            assert_eq!(stdin, format!("sudo-pw\n{new}\n").as_bytes());
            assert!(!script.text.contains("sudo-pw") && !script.text.contains(&new));
        }
        assert_eq!(rd.status().await.password_set, Some(true), "cache dropped");
    }

    #[tokio::test]
    async fn wrong_passwords_share_the_limit_and_refusals_are_not_wrong() {
        let dir = fake_proc(&["rustdesk"]);
        let host = Arc::new(MockHost::default());
        host.answer("rustdesk-status", 0, "password yes\n1\n", "");
        let rd = rustdesk(dir.path(), Some(&host), Some("manuel"), false);
        for _ in 0..crate::envedit::MAX_FAILURES {
            host.answer("rustdesk-password", EXIT_SUDO, "", "Sorry, try again.");
            host.answer("rustdesk-status", 0, "password yes\n1\n", "");
            let err = set(&rd, "nope").await.unwrap_err();
            assert!(matches!(err, RustDeskError::WrongPassword), "{err}");
        }
        let err = set(&rd, "right").await.unwrap_err();
        assert!(matches!(err, RustDeskError::RateLimited(_)), "{err}");
        assert!(
            rd.limiter.check().is_err(),
            "the shared limiter counted them"
        );

        let rd = rustdesk(dir.path(), Some(&host), Some("manuel"), false);
        host.answer("rustdesk-status", 0, "password yes\n1\n", "");
        host.answer(
            "rustdesk-password",
            EXIT_SUDO,
            "",
            "manuel is not in the sudoers file.",
        );
        let err = set(&rd, "pw").await.unwrap_err();
        assert!(matches!(err, RustDeskError::NotSudoer(_)), "{err}");
        assert!(rd.limiter.check().is_ok());
    }

    #[tokio::test]
    async fn refuses_before_touching_the_host() {
        let dir = fake_proc(&["rustdesk"]);
        let host = Arc::new(MockHost::default());
        let rd = rustdesk(dir.path(), Some(&host), Some("manuel"), false);
        for bad in ["", "a\nb"] {
            let err = set(&rd, bad).await.unwrap_err();
            assert!(matches!(err, RustDeskError::BadPassword(_)), "{bad:?}");
        }
        let gone = fake_proc(&[]);
        let err = set(
            &rustdesk(gone.path(), Some(&host), Some("manuel"), false),
            "pw",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RustDeskError::Unsupported(_)), "{err}");
        assert_eq!(host.ops(), Vec::<&str>::new());
    }

    #[tokio::test]
    async fn rustdesk_failures_are_reported_without_the_password() {
        let dir = fake_proc(&["rustdesk"]);
        let host = Arc::new(MockHost::default());
        host.answer("rustdesk-status", 0, "password yes\n1\n", "");
        host.answer(
            "rustdesk-password",
            EXIT_NOT_SET,
            "",
            "Changing permanent password is disabled!\n",
        );
        let rd = rustdesk(dir.path(), Some(&host), Some("manuel"), false);
        let err = set(&rd, "pw").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "rustdesk did not set the password: Changing permanent password is disabled!"
        );
    }
}
