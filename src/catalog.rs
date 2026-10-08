//! The companion services catalog (`deploy/companions/*.conf`), embedded at
//! build time so the binary (a `scratch` image) needs no file at runtime.
//!
//! Parsed with the same rules as `companion_parse` in `scripts/install.sh`:
//! `KEY=value` lines, blank lines and `#` comments; the value is the rest of
//! the line, verbatim. Cuthulu uses it to map a container to its systemd unit
//! and env file (see [`crate::envedit`]).

use std::fmt;

use rust_embed::Embed;

#[derive(Embed)]
#[folder = "deploy/companions/"]
struct Files;

/// Every entry needs these; `SUGGEST` (and any unknown key) is optional.
const REQUIRED: [&str; 8] = [
    "NAME",
    "DESCRIPTION",
    "REPO",
    "IMAGE",
    "CONTAINER",
    "UNIT",
    "UNIT_SCOPE",
    "ENV_FILE",
];

/// Which systemd instance runs a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitScope {
    /// `systemctl`
    System,
    /// `systemctl --user`, as the user owning the unit.
    User,
}

impl fmt::Display for UnitScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::System => "system",
            Self::User => "user",
        })
    }
}

/// One catalog entry, reduced to what Cuthulu needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// Name of the container the service runs as.
    pub container: String,
    /// systemd unit, without a `.service` suffix.
    pub unit: String,
    pub scope: UnitScope,
    /// The service's settings file; `~/…` is relative to the host user's home.
    pub env_file: String,
}

/// A catalog file that does not follow the format. Names the line, never a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{file}: {reason}")]
pub struct CatalogError {
    file: String,
    reason: String,
}

/// The embedded catalog, in file name order. Malformed entries are skipped
/// with a warning (a test keeps the shipped ones valid).
#[must_use]
pub fn embedded() -> Vec<Entry> {
    let mut names: Vec<_> = Files::iter().filter(|f| f.ends_with(".conf")).collect();
    names.sort();
    names
        .iter()
        .filter_map(|file| {
            let data = Files::get(file)?.data;
            let text = String::from_utf8_lossy(&data);
            parse(file, &text)
                .inspect_err(|e| tracing::warn!(error = %e, "skipping catalog entry"))
                .ok()
        })
        .collect()
}

/// The entry whose `CONTAINER` is `container`.
#[must_use]
pub fn find<'a>(entries: &'a [Entry], container: &str) -> Option<&'a Entry> {
    entries.iter().find(|e| e.container == container)
}

/// Parses one catalog file named `file` (`<NAME>.conf`).
///
/// # Errors
/// When a line is not `KEY=value`, a key repeats, a required key is missing or
/// empty, or a value breaks the format's rules.
pub fn parse(file: &str, text: &str) -> Result<Entry, CatalogError> {
    let fail = |reason: String| CatalogError {
        file: file.to_owned(),
        reason,
    };
    let mut kv: Vec<(&str, &str)> = Vec::new();
    for (n, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|(k, _)| is_key(k)) else {
            return Err(fail(format!("line {}: expected KEY=value", n + 1)));
        };
        if kv.iter().any(|(k, _)| *k == key) {
            return Err(fail(format!("line {}: {key} is set twice", n + 1)));
        }
        kv.push((key, value));
    }
    let get = |key: &str| kv.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
    if let Some(key) = REQUIRED.iter().find(|k| get(k).is_none_or(str::is_empty)) {
        return Err(fail(format!("{key} is missing or empty")));
    }
    let field = |key: &str| get(key).unwrap_or_default();

    let name = field("NAME");
    if !(starts_alnum(name)
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)))
    {
        return Err(fail(
            "NAME must be lowercase letters, digits, '.', '_' or '-'".into(),
        ));
    }
    if file.strip_suffix(".conf") != Some(name) {
        return Err(fail("NAME must match the file name".into()));
    }
    let scope = match field("UNIT_SCOPE") {
        "system" => UnitScope::System,
        "user" => UnitScope::User,
        _ => return Err(fail("UNIT_SCOPE must be system or user".into())),
    };
    if !matches!(get("SUGGEST").unwrap_or("true"), "true" | "false") {
        return Err(fail("SUGGEST must be true or false".into()));
    }
    let container = field("CONTAINER");
    if !(starts_alnum(container) && container.bytes().all(|b| name_byte(b, b"_.-"))) {
        return Err(fail("CONTAINER is not a container name".into()));
    }
    let unit = field("UNIT");
    if !(starts_alnum(unit) && unit.bytes().all(|b| name_byte(b, b"_.@-"))) {
        return Err(fail("UNIT is not a unit name".into()));
    }
    Ok(Entry {
        name: name.to_owned(),
        container: container.to_owned(),
        unit: unit.strip_suffix(".service").unwrap_or(unit).to_owned(),
        scope,
        env_file: field("ENV_FILE").to_owned(),
    })
}

/// `[A-Z][A-Z0-9_]*`, as in `install.sh`.
fn is_key(k: &str) -> bool {
    k.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        && k.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn starts_alnum(s: &str) -> bool {
    s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
}

fn name_byte(b: u8, extra: &[u8]) -> bool {
    b.is_ascii_alphanumeric() || extra.contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "# comment\nNAME=tool\nDESCRIPTION=Does = things # verbatim\n\
                        REPO=https://example.com/tool\nIMAGE=me/tool\n\n  \nCONTAINER=tool-c\r\n\
                        UNIT=tool.service\nUNIT_SCOPE=user\nENV_FILE=~/.config/tool/env\nEXTRA=x\n";

    #[test]
    fn parses_like_install_sh() {
        let e = parse("tool.conf", GOOD).unwrap();
        assert_eq!(
            e,
            Entry {
                name: "tool".into(),
                container: "tool-c".into(),
                unit: "tool".into(),
                scope: UnitScope::User,
                env_file: "~/.config/tool/env".into(),
            }
        );
    }

    #[test]
    fn rejects_malformed_files() {
        let without = |key: &str| {
            GOOD.lines()
                .filter(|l| !l.starts_with(&format!("{key}=")))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let cases = [
            (format!("{GOOD}NAME=again\n"), "set twice"),
            (format!("{GOOD}not a pair\n"), "expected KEY=value"),
            (format!("{GOOD}lower=x\n"), "expected KEY=value"),
            (format!("{GOOD} INDENTED=x\n"), "expected KEY=value"),
            (without("UNIT"), "UNIT is missing"),
            (
                GOOD.replace("ENV_FILE=~/.config/tool/env", "ENV_FILE="),
                "ENV_FILE is missing",
            ),
            (
                GOOD.replace("UNIT_SCOPE=user", "UNIT_SCOPE=global"),
                "UNIT_SCOPE",
            ),
            (format!("{GOOD}SUGGEST=maybe\n"), "SUGGEST"),
            (
                GOOD.replace("CONTAINER=tool-c", "CONTAINER=-x"),
                "CONTAINER",
            ),
            (GOOD.replace("UNIT=tool.service", "UNIT=a b"), "UNIT is not"),
            (GOOD.replace("NAME=tool", "NAME=Tool"), "NAME must be"),
        ];
        for (text, want) in cases {
            let err = parse("tool.conf", &text).unwrap_err().to_string();
            assert!(err.contains(want), "{want}: {err}");
        }
        let err = parse("other.conf", GOOD).unwrap_err().to_string();
        assert!(err.contains("match the file name"), "{err}");
    }

    #[test]
    fn embedded_catalog_has_the_shipped_services() {
        let entries = embedded();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "auto-git-commit-tool",
                "claude-session-starter",
                "cuthulu",
                "producer-tag-on-merge"
            ]
        );
        let me = find(&entries, "cuthulu").unwrap();
        assert_eq!((me.unit.as_str(), me.scope), ("cuthulu", UnitScope::System));
        assert_eq!(me.env_file, "/etc/cuthulu/.env");
        let user = find(&entries, "producer-tag-on-merge").unwrap();
        assert_eq!(user.scope, UnitScope::User);
        assert!(find(&entries, "web").is_none());
    }
}
