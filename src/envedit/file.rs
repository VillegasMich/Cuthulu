//! An env file as lines, for editing without disturbing what is not edited.
//!
//! Values are the raw text after the first `=`, exactly as in the file: the
//! files are read by `docker run --env-file` (value verbatim, quotes
//! included), Compose (strips quotes) and systemd, so the editor never adds or
//! removes quotes or escapes. Comments, blank lines, unrecognised lines, the
//! order of keys and untouched lines are kept byte for byte.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};

use serde::{Deserialize, Serialize};

/// Most variables accepted in one save.
pub const MAX_VARS: usize = 500;
/// Longest value accepted, in bytes.
pub const MAX_VALUE_BYTES: usize = 8 * 1024;

/// Key substrings (case-insensitive) whose values are masked in the editor.
const SECRET_WORDS: [&str; 7] = ["TOKEN", "PASSWORD", "PASS", "SECRET", "KEY", "URL", "AUTH"];

/// One `KEY=value` pair as the editor shows and sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Var {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error(
        "`{0}` is not a valid variable name (letters, digits and _, not starting with a digit)"
    )]
    BadKey(String),
    #[error("{0} appears twice")]
    Duplicate(String),
    #[error("the value of {0} contains a line break or NUL")]
    BadValue(String),
    #[error("the value of {0} is longer than {MAX_VALUE_BYTES} bytes")]
    TooLong(String),
    #[error("more than {MAX_VARS} variables")]
    TooMany,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    /// `prefix KEY=value`; `prefix` is leading blanks and an optional `export `.
    Var {
        prefix: String,
        key: String,
        value: String,
        cr: bool,
    },
    /// Anything else (comments, blanks, lines the editor does not touch), raw.
    Other(String),
}

/// A parsed env file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvText {
    lines: Vec<Line>,
    /// Whether the last line ended with a newline.
    final_newline: bool,
}

/// `[A-Za-z_][A-Za-z0-9_]*`
#[must_use]
pub fn valid_key(key: &str) -> bool {
    key.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Whether the editor masks the value of `key` until revealed.
#[must_use]
pub fn is_secret(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    SECRET_WORDS.iter().any(|w| key.contains(w))
}

/// A short fingerprint of a file's contents, so a save can tell the file
/// changed since it was loaded. Not a security measure.
#[must_use]
pub fn version(text: &str) -> String {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    format!("{:016x}", h.finish())
}

impl EnvText {
    /// Splits `text` into lines.
    ///
    /// # Errors
    /// [`EditError::Duplicate`] when a key is set twice: which one wins
    /// differs between readers, so the editor leaves that to a human.
    pub fn parse(text: &str) -> Result<Self, EditError> {
        let final_newline = text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        let mut seen = HashSet::new();
        let mut lines = Vec::new();
        if !text.is_empty() {
            for raw in body.split('\n') {
                let line = parse_line(raw);
                if let Line::Var { key, .. } = &line
                    && !seen.insert(key.clone())
                {
                    return Err(EditError::Duplicate(key.clone()));
                }
                lines.push(line);
            }
        }
        Ok(Self {
            lines,
            final_newline,
        })
    }

    /// The variables, in file order.
    #[must_use]
    pub fn vars(&self) -> Vec<Var> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                Line::Var { key, value, .. } => Some(Var {
                    key: key.clone(),
                    value: value.clone(),
                }),
                Line::Other(_) => None,
            })
            .collect()
    }

    /// The file with exactly the variables `want`: changed values are
    /// rewritten in place, keys missing from `want` are removed, new keys are
    /// appended in `want`'s order. Every other line is kept as is.
    ///
    /// # Errors
    /// When `want` has an invalid or repeated key, or a value with a line
    /// break, NUL, or over [`MAX_VALUE_BYTES`].
    pub fn apply(&self, want: &[Var]) -> Result<String, EditError> {
        validate(want)?;
        let wanted = |key: &str| want.iter().find(|v| v.key == key);
        let mut out = String::new();
        let mut kept = HashSet::new();
        let mut first = true;
        let mut push = |out: &mut String, line: &str| {
            if !first {
                out.push('\n');
            }
            first = false;
            out.push_str(line);
        };
        for line in &self.lines {
            match line {
                Line::Other(raw) => push(&mut out, raw),
                Line::Var {
                    prefix,
                    key,
                    value,
                    cr,
                } => {
                    let Some(new) = wanted(key) else { continue };
                    kept.insert(key.as_str());
                    let value = if new.value == *value {
                        value
                    } else {
                        &new.value
                    };
                    let cr = if *cr { "\r" } else { "" };
                    push(&mut out, &format!("{prefix}{key}={value}{cr}"));
                }
            }
        }
        for v in want.iter().filter(|v| !kept.contains(v.key.as_str())) {
            push(&mut out, &format!("{}={}", v.key, v.value));
        }
        if !first && (self.final_newline || want.iter().any(|v| !kept.contains(v.key.as_str()))) {
            out.push('\n');
        }
        Ok(out)
    }
}

fn parse_line(raw: &str) -> Line {
    let (text, cr) = raw.strip_suffix('\r').map_or((raw, false), |t| (t, true));
    let blanks = text.len() - text.trim_start_matches([' ', '\t']).len();
    let after_blanks = &text[blanks..];
    let export = after_blanks
        .strip_prefix("export")
        .filter(|r| r.starts_with([' ', '\t']))
        .map_or(0, |r| {
            6 + (r.len() - r.trim_start_matches([' ', '\t']).len())
        });
    let start = blanks + export;
    match text[start..].split_once('=') {
        Some((key, value)) if valid_key(key) => Line::Var {
            prefix: text[..start].to_owned(),
            key: key.to_owned(),
            value: value.to_owned(),
            cr,
        },
        _ => Line::Other(raw.to_owned()),
    }
}

fn validate(want: &[Var]) -> Result<(), EditError> {
    if want.len() > MAX_VARS {
        return Err(EditError::TooMany);
    }
    let mut seen = HashSet::new();
    for v in want {
        if !valid_key(&v.key) {
            return Err(EditError::BadKey(v.key.chars().take(64).collect()));
        }
        if !seen.insert(v.key.as_str()) {
            return Err(EditError::Duplicate(v.key.clone()));
        }
        if v.value.contains(['\n', '\r', '\0']) {
            return Err(EditError::BadValue(v.key.clone()));
        }
        if v.value.len() > MAX_VALUE_BYTES {
            return Err(EditError::TooLong(v.key.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(key: &str, value: &str) -> Var {
        Var {
            key: key.into(),
            value: value.into(),
        }
    }

    const FILE: &str = "# settings, written by install.sh\n\
                        GITHUB_TOKEN=ghp_abc\n\
                        \n\
                        export  QUOTED=\"a b\" # stays\n  \
                        INDENTED='x'\n\
                        not a var line\n\
                        KEY = spaced\n\
                        URL=https://x/?a=b\n";

    #[test]
    fn reads_raw_values_in_order() {
        let env = EnvText::parse(FILE).unwrap();
        assert_eq!(
            env.vars(),
            [
                var("GITHUB_TOKEN", "ghp_abc"),
                var("QUOTED", "\"a b\" # stays"),
                var("INDENTED", "'x'"),
                var("URL", "https://x/?a=b"),
            ]
        );
    }

    #[test]
    fn unchanged_round_trips_byte_for_byte() {
        for text in [
            FILE,
            "",
            "A=1",
            "A=1\r\nB=2\r\n",
            "\n\n# only comments\n",
            "A=\n",
        ] {
            let env = EnvText::parse(text).unwrap();
            assert_eq!(env.apply(&env.vars()).unwrap(), text, "{text:?}");
        }
    }

    #[test]
    fn edits_only_the_changed_lines() {
        let env = EnvText::parse(FILE).unwrap();
        let mut want = env.vars();
        want[0].value = "ghp_new".into();
        want[1].value = "unquoted now".into();
        let out = env.apply(&want).unwrap();
        assert_eq!(
            out,
            FILE.replace("GITHUB_TOKEN=ghp_abc", "GITHUB_TOKEN=ghp_new")
                .replace("QUOTED=\"a b\" # stays", "QUOTED=unquoted now")
        );
        // The prefix (`export  `) is kept.
        assert!(out.contains("export  QUOTED=unquoted now\n"));
    }

    #[test]
    fn deletes_and_adds_keys() {
        let env = EnvText::parse("# c\nA=1\nB=2\nC=3\n").unwrap();
        let out = env
            .apply(&[
                var("C", "3"),
                var("NEW", "x y"),
                var("A", "1"),
                var("Z", ""),
            ])
            .unwrap();
        assert_eq!(out, "# c\nA=1\nC=3\nNEW=x y\nZ=\n");

        // Appending to a file without a final newline adds one in between.
        let env = EnvText::parse("A=1").unwrap();
        assert_eq!(
            env.apply(&[var("A", "1"), var("B", "2")]).unwrap(),
            "A=1\nB=2\n"
        );
        // Everything deleted: comments stay.
        let env = EnvText::parse("# keep\nA=1\n").unwrap();
        assert_eq!(env.apply(&[]).unwrap(), "# keep\n");
        assert_eq!(
            EnvText::parse("").unwrap().apply(&[var("A", "1")]).unwrap(),
            "A=1\n"
        );
    }

    #[test]
    fn values_are_written_verbatim() {
        let env = EnvText::parse("A=1\n").unwrap();
        for value in [
            "'single'",
            "\"double\"",
            "a # b",
            " lead",
            "trail ",
            "$HOME",
            "\\n",
        ] {
            let out = env.apply(&[var("A", value)]).unwrap();
            assert_eq!(out, format!("A={value}\n"));
            assert_eq!(EnvText::parse(&out).unwrap().vars(), [var("A", value)]);
        }
    }

    #[test]
    fn rejects_bad_input() {
        let env = EnvText::parse("A=1\n").unwrap();
        let cases = [
            (vec![var("1A", "x")], EditError::BadKey("1A".into())),
            (vec![var("A-B", "x")], EditError::BadKey("A-B".into())),
            (vec![var("", "x")], EditError::BadKey(String::new())),
            (
                vec![var("A", "1"), var("A", "2")],
                EditError::Duplicate("A".into()),
            ),
            (vec![var("A", "x\ny")], EditError::BadValue("A".into())),
            (vec![var("A", "x\ry")], EditError::BadValue("A".into())),
            (vec![var("A", "x\0")], EditError::BadValue("A".into())),
            (
                vec![var("A", &"x".repeat(MAX_VALUE_BYTES + 1))],
                EditError::TooLong("A".into()),
            ),
        ];
        for (want, err) in cases {
            assert_eq!(env.apply(&want), Err(err));
        }
        let many: Vec<Var> = (0..=MAX_VARS).map(|i| var(&format!("K{i}"), "")).collect();
        assert_eq!(env.apply(&many), Err(EditError::TooMany));
        // Errors name the key, never the value.
        let err = env.apply(&[var("A", "hunter2\n")]).unwrap_err().to_string();
        assert!(!err.contains("hunter2"), "{err}");
    }

    #[test]
    fn duplicate_keys_in_the_file_are_refused() {
        assert_eq!(
            EnvText::parse("A=1\nexport A=2\n"),
            Err(EditError::Duplicate("A".into()))
        );
    }

    #[test]
    fn masks_secret_looking_keys() {
        for key in [
            "GITHUB_TOKEN",
            "CUTHULU_SMTP_PASSWORD",
            "SMTP_PASS",
            "client_secret",
            "API_KEY",
            "CUTHULU_HEALTHCHECK_URL",
            "TS_AUTHKEY",
            "BASIC_AUTH",
        ] {
            assert!(is_secret(key), "{key}");
        }
        for key in ["IMAGE", "POLL_INTERVAL_SECONDS", "DOCKER_GID", "RUST_LOG"] {
            assert!(!is_secret(key), "{key}");
        }
    }

    #[test]
    fn version_tracks_content() {
        assert_eq!(version("A=1\n"), version("A=1\n"));
        assert_ne!(version("A=1\n"), version("A=2\n"));
        assert_eq!(version("").len(), 16);
    }
}
