//! The host command behind start, stop and restart of a catalog service: a
//! fixed `/bin/sh` template whose only variable parts are the unit (from the
//! embedded catalog) and `CUTHULU_HOST_USER`, both checked and shell-quoted.

use crate::catalog::{Entry, UnitScope};
use crate::envedit::host::{self, HostUser, TargetError, quote};
use crate::model::Action;
use crate::providers::HostScript;

pub use crate::envedit::host::EXIT_NO_USER;

/// The unit is not installed on the host (`LoadState=not-found`); the
/// caller falls back to acting on the container.
pub const EXIT_NOT_LOADED: i32 = 20;
/// `systemctl show` failed: the unit's manager could not be asked.
pub const EXIT_MANAGER: i32 = 21;
/// The `systemctl` action itself failed.
pub const EXIT_SYSTEMCTL: i32 = 22;

/// The command line shown to the operator, e.g. `systemctl --user stop tool`.
#[must_use]
pub fn display(unit: &str, scope: UnitScope, action: Action, no_block: bool) -> String {
    let user = if scope == UnitScope::User {
        " --user"
    } else {
        ""
    };
    let no_block = if no_block { " --no-block" } else { "" };
    format!("systemctl{user}{no_block} {} {unit}", action.as_str())
}

/// Runs `action` on `entry`'s unit, after checking the unit is installed.
/// `no_block` queues the job without waiting (for Cuthulu itself, which a
/// restart stops before the reply could go out).
///
/// # Errors
/// [`TargetError::NoUser`] for a user unit without `CUTHULU_HOST_USER`,
/// [`TargetError::BadUnit`] for a unit name outside the allowed set.
pub fn action(
    entry: &Entry,
    action: Action,
    host_user: Option<&HostUser>,
    no_block: bool,
) -> Result<HostScript, TargetError> {
    let unit = quote(host::check_unit(&entry.unit)?);
    let (setup, systemctl) = host::systemctl(entry.scope, host_user)?;
    let no_block = if no_block { " --no-block" } else { "" };
    Ok(HostScript {
        op: action.as_str(),
        text: format!(
            "set -u\n\
             export LC_ALL=C\n\
             {setup}\
             load=$({systemctl} show -p LoadState --value {unit}) || exit {EXIT_MANAGER}\n\
             [ \"$load\" != not-found ] || exit {EXIT_NOT_LOADED}\n\
             {systemctl}{no_block} {verb} {unit} || exit {EXIT_SYSTEMCTL}\n",
            verb = action.as_str(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(unit: &str, scope: UnitScope) -> Entry {
        Entry {
            name: "tool".into(),
            container: "tool".into(),
            unit: unit.into(),
            scope,
            env_file: "/etc/tool/env".into(),
        }
    }

    fn user(name: &str) -> HostUser {
        HostUser::parse(name).unwrap()
    }

    #[test]
    fn system_units_use_the_system_manager() {
        let s = action(&entry("tool", UnitScope::System), Action::Stop, None, false).unwrap();
        assert_eq!(s.op, "stop");
        assert_eq!(
            s.text,
            "set -u\n\
             export LC_ALL=C\n\
             load=$(systemctl show -p LoadState --value 'tool') || exit 21\n\
             [ \"$load\" != not-found ] || exit 20\n\
             systemctl stop 'tool' || exit 22\n"
        );
        let me = user("manuel");
        let s = action(
            &entry("tool", UnitScope::System),
            Action::Start,
            Some(&me),
            false,
        )
        .unwrap();
        assert!(!s.text.contains("runuser"), "{}", s.text);
        assert!(s.text.contains("systemctl start 'tool' || exit 22"));
    }

    #[test]
    fn user_units_run_in_the_host_users_manager() {
        let me = user("manuel");
        let s = action(
            &entry("tool", UnitScope::User),
            Action::Restart,
            Some(&me),
            false,
        )
        .unwrap();
        let sc = "runuser -u 'manuel' -- env XDG_RUNTIME_DIR=/run/user/\"$uid\" systemctl --user";
        assert!(
            s.text.contains("uid=$(id -u -- 'manuel') || exit 11\n"),
            "{}",
            s.text
        );
        assert!(
            s.text
                .contains(&format!("load=$({sc} show -p LoadState --value 'tool')")),
            "{}",
            s.text
        );
        assert!(
            s.text.contains(&format!("{sc} restart 'tool' || exit 22")),
            "{}",
            s.text
        );
        assert_eq!(
            action(&entry("tool", UnitScope::User), Action::Stop, None, false),
            Err(TargetError::NoUser)
        );
    }

    #[test]
    fn restarting_itself_does_not_wait() {
        let s = action(
            &entry("cuthulu", UnitScope::System),
            Action::Restart,
            None,
            true,
        )
        .unwrap();
        assert!(
            s.text
                .contains("systemctl --no-block restart 'cuthulu' || exit 22"),
            "{}",
            s.text
        );
        // The unit check still waits: it is quick and decides the fallback.
        assert!(s.text.contains("systemctl show -p LoadState"));
    }

    #[test]
    fn a_missing_unit_has_its_own_exit_code() {
        let s = action(
            &entry("tool", UnitScope::System),
            Action::Start,
            None,
            false,
        )
        .unwrap();
        assert!(s.text.contains(&format!(
            "[ \"$load\" != not-found ] || exit {EXIT_NOT_LOADED}\n"
        )));
        assert!(![0, EXIT_NO_USER, EXIT_MANAGER, EXIT_SYSTEMCTL].contains(&EXIT_NOT_LOADED));
    }

    #[test]
    fn odd_unit_names_never_reach_the_script() {
        for bad in ["-u", "a b", "a;b", "a'b", "$(id)", ""] {
            assert_eq!(
                action(&entry(bad, UnitScope::System), Action::Stop, None, false),
                Err(TargetError::BadUnit(bad.into())),
                "{bad}"
            );
        }
    }

    #[test]
    fn display_names_the_command() {
        assert_eq!(
            display("tool", UnitScope::System, Action::Stop, false),
            "systemctl stop tool"
        );
        assert_eq!(
            display("tool", UnitScope::User, Action::Start, false),
            "systemctl --user start tool"
        );
        assert_eq!(
            display("cuthulu", UnitScope::System, Action::Restart, true),
            "systemctl --no-block restart cuthulu"
        );
    }

    /// The template must be valid `sh`; checked with `sh -n` when available.
    #[test]
    fn script_parses_as_sh() {
        let me = user("manuel");
        for (scope, no_block) in [(UnitScope::User, false), (UnitScope::System, true)] {
            let s = action(&entry("tool", scope), Action::Restart, Some(&me), no_block).unwrap();
            let Ok(out) = std::process::Command::new("sh")
                .args(["-n", "-c", &s.text])
                .output()
            else {
                return;
            };
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
