//! The host commands of the env editor: fixed `/bin/sh` templates whose only
//! variable parts are the unit, the env file path and user names from the
//! embedded catalog and `CUTHULU_HOST_USER`. Each is checked against a strict
//! charset and shell-quoted; secrets (the password, the new file) only ever
//! travel on stdin.

use std::fmt;

use crate::catalog::{Entry, UnitScope};
use crate::providers::HostScript;

/// The env file does not exist (or is not a regular file).
pub const EXIT_NO_FILE: i32 = 10;
/// A user name did not resolve to an account on the host.
pub const EXIT_NO_USER: i32 = 11;
/// `sudo` refused the password (or the user may not use sudo).
pub const EXIT_SUDO: i32 = 12;
/// The env file is larger than [`MAX_FILE_BYTES`].
pub const EXIT_TOO_LARGE: i32 = 13;
/// Reading, backing up or replacing the file failed.
pub const EXIT_FILE_IO: i32 = 14;
/// `systemctl restart` failed.
pub const EXIT_RESTART: i32 = 15;

/// Largest env file the editor handles.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;

/// A login name on the host that is not root: `[a-z_][a-z0-9_-]*` with an
/// optional trailing `$`, at most 32 bytes (the `useradd` rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostUser(String);

impl HostUser {
    /// # Errors
    /// When `name` is root or not a plain login name.
    pub fn parse(name: &str) -> Result<Self, &'static str> {
        let body = name.strip_suffix('$').unwrap_or(name);
        let valid = !body.is_empty()
            && name.len() <= 32
            && body
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
            && body
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b));
        if !valid {
            return Err("must be a login name (lowercase letters, digits, '_', '-')");
        }
        if name == "root" {
            return Err("must not be root: sudo never asks root for a password");
        }
        Ok(Self(name.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a catalog entry cannot be edited, or the password not be checked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    #[error("ENV_FILE `{0}` is not a plain absolute or ~/ path")]
    BadPath(String),
    #[error("UNIT `{0}` is not a plain unit name")]
    BadUnit(String),
    #[error("no host user to ask sudo for; set CUTHULU_HOST_USER (scripts/install.sh does)")]
    NoUser,
}

/// Where a catalog entry's env file lives.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnvPath {
    Absolute(String),
    /// Relative to the host user's home (`~/rest`).
    Home(String),
}

/// A catalog entry checked for use in host commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    path: EnvPath,
    unit: String,
    scope: UnitScope,
    /// The env file as written in the catalog, for display.
    pub env_file: String,
}

impl Target {
    /// # Errors
    /// When the entry's path or unit has characters outside the allowed sets.
    pub fn new(entry: &Entry) -> Result<Self, TargetError> {
        let bad_path = || TargetError::BadPath(entry.env_file.clone());
        let raw = entry.env_file.as_str();
        let (path, rest) = if let Some(rest) = raw.strip_prefix("~/") {
            (EnvPath::Home(rest.to_owned()), rest)
        } else if let Some(rest) = raw.strip_prefix('/') {
            (EnvPath::Absolute(raw.to_owned()), rest)
        } else {
            return Err(bad_path());
        };
        let segment_ok = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-+@".contains(&b))
        };
        if !rest.split('/').all(segment_ok) {
            return Err(bad_path());
        }
        let unit = entry.unit.as_str();
        let unit_ok = unit
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
            && unit
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._@-".contains(&b));
        if !unit_ok {
            return Err(TargetError::BadUnit(unit.to_owned()));
        }
        Ok(Self {
            path,
            unit: unit.to_owned(),
            scope: entry.scope,
            env_file: entry.env_file.clone(),
        })
    }

    /// Whether finding the file needs `CUTHULU_HOST_USER` (a `~/` path).
    #[must_use]
    pub fn needs_host_user(&self) -> bool {
        matches!(self.path, EnvPath::Home(_))
    }

    #[must_use]
    pub fn unit(&self) -> &str {
        &self.unit
    }

    #[must_use]
    pub fn scope(&self) -> UnitScope {
        self.scope
    }

    /// Shell lines that set `$f` to the env file, failing on a missing home.
    fn locate(&self, host_user: Option<&HostUser>) -> Result<String, TargetError> {
        Ok(match &self.path {
            EnvPath::Absolute(p) => format!("f={}\n", quote(p)),
            EnvPath::Home(rest) => {
                let user = host_user.ok_or(TargetError::NoUser)?;
                format!(
                    "home=$(getent passwd {u} | cut -d: -f6)\n\
                     [ -n \"$home\" ] || exit {EXIT_NO_USER}\n\
                     f=\"$home\"/{rest}\n",
                    u = quote(user.as_str()),
                    rest = quote(rest),
                )
            }
        })
    }
}

/// Owner and size of the env file, from [`probe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub uid: u32,
    pub owner: String,
    pub size: u64,
}

/// Quotes `s` for `/bin/sh` (single quotes; `'` becomes `'\''`).
#[must_use]
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Common start of every script: strict, C locale (stable messages).
fn prelude(target: &Target, host_user: Option<&HostUser>) -> Result<String, TargetError> {
    Ok(format!(
        "set -u\nexport LC_ALL=C\n{}[ -f \"$f\" ] || exit {EXIT_NO_FILE}\n",
        target.locate(host_user)?
    ))
}

/// Prints `uid owner size` of the env file. Needs no secret.
///
/// # Errors
/// [`TargetError::NoUser`] for a `~/` path without `CUTHULU_HOST_USER`.
pub fn probe(target: &Target, host_user: Option<&HostUser>) -> Result<HostScript, TargetError> {
    Ok(HostScript {
        op: "probe",
        text: format!(
            "{}stat -c '%u %U %s' -- \"$f\"\n",
            prelude(target, host_user)?
        ),
    })
}

/// Parses [`probe`]'s output.
#[must_use]
pub fn parse_probe(stdout: &str) -> Option<Probe> {
    let mut parts = stdout.split_whitespace();
    let probe = Probe {
        uid: parts.next()?.parse().ok()?,
        owner: parts.next()?.to_owned(),
        size: parts.next()?.parse().ok()?,
    };
    parts.next().is_none().then_some(probe)
}

/// The user whose sudo password unlocks the file: its owner when that is a
/// normal user, else `CUTHULU_HOST_USER`. Never root, since sudo would not
/// ask root for a password and every check would pass.
///
/// # Errors
/// [`TargetError::NoUser`] when the file is root's (or the owner has no
/// valid name) and `CUTHULU_HOST_USER` is unset.
pub fn resolve_user(probe: &Probe, host_user: Option<&HostUser>) -> Result<HostUser, TargetError> {
    if probe.uid != 0
        && let Ok(owner) = HostUser::parse(&probe.owner)
    {
        return Ok(owner);
    }
    host_user.cloned().ok_or(TargetError::NoUser)
}

/// Lines that check the password on stdin with `sudo` as `user`; the
/// credential cache this creates is dropped right away.
fn sudo_check(user: &HostUser) -> String {
    let u = quote(user.as_str());
    format!(
        "runuser -u {u} -- sudo -S -k -v -p '' || exit {EXIT_SUDO}\n\
         runuser -u {u} -- sudo -k 2>/dev/null\n"
    )
}

/// Checks the password (stdin) as `user`, then prints the env file.
///
/// # Errors
/// [`TargetError::NoUser`] for a `~/` path without `CUTHULU_HOST_USER`.
pub fn read(
    target: &Target,
    host_user: Option<&HostUser>,
    user: &HostUser,
) -> Result<HostScript, TargetError> {
    Ok(HostScript {
        op: "read",
        text: format!(
            "{}[ \"$(stat -c %s -- \"$f\")\" -le {MAX_FILE_BYTES} ] || exit {EXIT_TOO_LARGE}\n\
             {}cat -- \"$f\" || exit {EXIT_FILE_IO}\n",
            prelude(target, host_user)?,
            sudo_check(user),
        ),
    })
}

/// Replaces the env file with stdin and restarts the unit: a `.bak` copy
/// first (one, overwritten each time), then a temp file in the same
/// directory with the original owner and mode, renamed over the file.
/// `no_block` queues the restart without waiting (for Cuthulu itself, which
/// the restart stops).
///
/// # Errors
/// [`TargetError::NoUser`] for a `~/` path without `CUTHULU_HOST_USER`.
pub fn write(
    target: &Target,
    host_user: Option<&HostUser>,
    user: &HostUser,
    no_block: bool,
) -> Result<HostScript, TargetError> {
    let unit = quote(&target.unit);
    let no_block = if no_block { " --no-block" } else { "" };
    let restart = match target.scope {
        UnitScope::System => format!("systemctl{no_block} restart {unit}"),
        UnitScope::User => {
            let u = quote(user.as_str());
            format!(
                "uid=$(id -u -- {u}) || exit {EXIT_NO_USER}\n\
                 runuser -u {u} -- env XDG_RUNTIME_DIR=/run/user/\"$uid\" \
                 systemctl --user{no_block} restart {unit}"
            )
        }
    };
    Ok(HostScript {
        op: "write",
        text: format!(
            "{prelude}\
             fail() {{ rm -f -- \"${{b:-}}\" \"${{t:-}}\"; exit {EXIT_FILE_IO}; }}\n\
             b=$(mktemp -- \"$f.bak.XXXXXX\") || fail\n\
             cp -p -- \"$f\" \"$b\" && mv -f -- \"$b\" \"$f.bak\" || fail\n\
             b=\n\
             t=$(mktemp -- \"$f.XXXXXX\") || fail\n\
             cat > \"$t\" || fail\n\
             chown --reference=\"$f\" -- \"$t\" && chmod --reference=\"$f\" -- \"$t\" || fail\n\
             mv -f -- \"$t\" \"$f\" || fail\n\
             {restart} || exit {EXIT_RESTART}\n",
            prelude = prelude(target, host_user)?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(env_file: &str, unit: &str, scope: UnitScope) -> Entry {
        Entry {
            name: "tool".into(),
            container: "tool".into(),
            unit: unit.into(),
            scope,
            env_file: env_file.into(),
        }
    }

    fn user(name: &str) -> HostUser {
        HostUser::parse(name).unwrap()
    }

    #[test]
    fn quotes_for_sh() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn host_user_names() {
        for ok in ["manuel", "_svc", "a-b_9", "machine$"] {
            assert!(HostUser::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "root",
            "Root",
            "9lives",
            "-x",
            "a b",
            "a;b",
            "a'b",
            "$",
            "x$y",
            "abcdefghijklmnopqrstuvwxyz0123456",
        ] {
            assert!(HostUser::parse(bad).is_err(), "{bad}");
        }
        assert!(HostUser::parse("root").unwrap_err().contains("root"));
    }

    #[test]
    fn target_rejects_odd_paths_and_units() {
        for bad in [
            "etc/x",
            "/etc/../shadow",
            "/etc/./x",
            "//etc/x",
            "/etc/x/",
            "/etc/a b",
            "/etc/$(id)",
            "/etc/x'y",
            "~user/x",
            "~/",
            "",
        ] {
            assert_eq!(
                Target::new(&entry(bad, "u", UnitScope::System)),
                Err(TargetError::BadPath(bad.into())),
                "{bad}"
            );
        }
        for bad in ["-u", "a b", "a;b", "a'b", "u/x", ""] {
            assert_eq!(
                Target::new(&entry("/etc/x", bad, UnitScope::System)),
                Err(TargetError::BadUnit(bad.into())),
                "{bad}"
            );
        }
        let t = Target::new(&entry("~/.config/t-1/env", "t@1.x", UnitScope::User)).unwrap();
        assert!(t.needs_host_user());
    }

    #[test]
    fn user_is_the_owner_unless_root() {
        let me = user("manuel");
        let probe = |uid, owner: &str| Probe {
            uid,
            owner: owner.into(),
            size: 10,
        };
        assert_eq!(
            resolve_user(&probe(1000, "alice"), Some(&me)),
            Ok(user("alice"))
        );
        assert_eq!(resolve_user(&probe(1000, "alice"), None), Ok(user("alice")));
        assert_eq!(resolve_user(&probe(0, "root"), Some(&me)), Ok(me.clone()));
        assert_eq!(
            resolve_user(&probe(0, "root"), None),
            Err(TargetError::NoUser)
        );
        // A uid without a name, or a name that would be root, falls back too.
        assert_eq!(
            resolve_user(&probe(1234, "UNKNOWN"), Some(&me)),
            Ok(me.clone())
        );
        assert_eq!(
            resolve_user(&probe(5, "root"), None),
            Err(TargetError::NoUser)
        );
    }

    #[test]
    fn probe_output() {
        assert_eq!(
            parse_probe("1000 manuel 120\n"),
            Some(Probe {
                uid: 1000,
                owner: "manuel".into(),
                size: 120,
            })
        );
        for bad in ["", "x manuel 1", "0 root", "0 root 1 extra"] {
            assert_eq!(parse_probe(bad), None, "{bad}");
        }
    }

    #[test]
    fn scripts_quote_every_variable_part() {
        let t = Target::new(&entry("/etc/tool/env", "tool", UnitScope::System)).unwrap();
        let me = user("manuel");
        let p = probe(&t, None).unwrap();
        assert_eq!(p.op, "probe");
        assert!(p.text.contains("f='/etc/tool/env'\n"), "{}", p.text);

        let r = read(&t, None, &me).unwrap().text;
        assert!(
            r.contains("runuser -u 'manuel' -- sudo -S -k -v -p '' || exit 12\n"),
            "{r}"
        );
        assert!(r.contains("cat -- \"$f\""), "{r}");

        let w = write(&t, None, &me, false).unwrap().text;
        assert!(w.contains("systemctl restart 'tool' || exit 15\n"), "{w}");
        assert!(w.contains("mv -f -- \"$b\" \"$f.bak\""), "{w}");
        assert!(
            !w.contains("sudo"),
            "the write never sees the password: {w}"
        );
        let w = write(&t, None, &me, true).unwrap().text;
        assert!(w.contains("systemctl --no-block restart 'tool'"), "{w}");
    }

    #[test]
    fn user_scope_scripts_use_the_users_manager_and_home() {
        let t = Target::new(&entry("~/.config/tool/env", "tool", UnitScope::User)).unwrap();
        let me = user("manuel");
        assert_eq!(probe(&t, None), Err(TargetError::NoUser));
        let p = probe(&t, Some(&me)).unwrap().text;
        assert!(p.contains("getent passwd 'manuel'"), "{p}");
        assert!(p.contains("f=\"$home\"/'.config/tool/env'\n"), "{p}");

        let w = write(&t, Some(&me), &user("alice"), false).unwrap().text;
        assert!(w.contains("uid=$(id -u -- 'alice')"), "{w}");
        assert!(
            w.contains(
                "runuser -u 'alice' -- env XDG_RUNTIME_DIR=/run/user/\"$uid\" \
                 systemctl --user restart 'tool' || exit 15"
            ),
            "{w}"
        );
    }

    /// The templates must be valid `sh`; run them through `sh -n` when available.
    #[test]
    fn scripts_parse_as_sh() {
        let t = Target::new(&entry("~/.config/tool/env", "tool", UnitScope::User)).unwrap();
        let me = user("manuel");
        for s in [
            probe(&t, Some(&me)).unwrap(),
            read(&t, Some(&me), &me).unwrap(),
            write(&t, Some(&me), &me, true).unwrap(),
        ] {
            let Ok(out) = std::process::Command::new("sh")
                .args(["-n", "-c", &s.text])
                .output()
            else {
                return;
            };
            assert!(
                out.status.success(),
                "{}: {}",
                s.op,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
