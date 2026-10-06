//! Optional `.env` file in the working directory, so `cargo run` picks up
//! the same settings as `docker compose` (which reads that file too).
//!
//! The file never overrides the real environment, and it is never written
//! into the process environment: [`EnvFile::lookup`] layers it under
//! `std::env::var` for [`crate::config::Config::from_lookup`].
//!
//! Syntax follows Compose's `.env` rules, minus interpolation: `KEY=value`
//! per line, blank lines and `#` comments ignored, an optional `export `
//! prefix, values in `'single'` (literal) or `"double"` quotes (with `\n`,
//! `\t`, `\"`, `\\` escapes), and ` # comment` after an unquoted value.
//! Errors name the line and key, never the value (it may be a password).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Default file name, relative to the working directory.
pub const FILE_NAME: &str = ".env";

#[derive(Debug, thiserror::Error)]
pub enum EnvFileError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{path}:{line}: {reason}")]
    Syntax {
        path: PathBuf,
        line: usize,
        reason: String,
    },
}

/// Variables read from a `.env` file; empty when there is none.
#[derive(Debug, Default)]
pub struct EnvFile {
    path: Option<PathBuf>,
    vars: HashMap<String, String>,
}

impl EnvFile {
    /// Reads `path` if it exists. A missing file is not an error.
    pub fn load(path: &Path) -> Result<Self, EnvFileError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(EnvFileError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let vars = parse(&text).map_err(|(line, reason)| EnvFileError::Syntax {
            path: path.to_owned(),
            line,
            reason,
        })?;
        Ok(Self {
            path: Some(path.to_owned()),
            vars: vars.into_iter().collect(),
        })
    }

    /// The file that was read, if any.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Names of the variables set in the file, sorted (for logging).
    #[must_use]
    pub fn keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = self.vars.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    /// `key` from the real environment, else from the file.
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .or_else(|| self.vars.get(key).cloned())
    }
}

/// Parses `.env` text into `(key, value)` pairs; a later key wins.
/// Errors are `(1-based line, reason)`.
pub fn parse(text: &str) -> Result<Vec<(String, String)>, (usize, String)> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let Some((key, value)) = line.split_once('=') else {
            return Err((i + 1, "expected KEY=value".to_owned()));
        };
        let key = key.trim();
        let valid = key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err((i + 1, format!("invalid variable name `{key}`")));
        }
        let value = parse_value(value.trim_start())
            .map_err(|reason| (i + 1, format!("{key}: {reason}")))?;
        out.push((key.to_owned(), value));
    }
    Ok(out)
}

fn parse_value(v: &str) -> Result<String, String> {
    let (value, rest) = match v.chars().next() {
        Some('\'') => {
            let end = v[1..]
                .find('\'')
                .ok_or("unterminated single quote (multi-line values are not supported)")?;
            (v[1..=end].to_owned(), &v[end + 2..])
        }
        Some('"') => {
            let mut value = String::new();
            let mut chars = v[1..].char_indices();
            let mut end = None;
            while let Some((at, c)) = chars.next() {
                match c {
                    '"' => {
                        end = Some(at + 2);
                        break;
                    }
                    '\\' => match chars.next().map(|(_, c)| c) {
                        Some('n') => value.push('\n'),
                        Some('t') => value.push('\t'),
                        Some('r') => value.push('\r'),
                        Some(c @ ('"' | '\\' | '$')) => value.push(c),
                        Some(c) => {
                            value.push('\\');
                            value.push(c);
                        }
                        None => break,
                    },
                    c => value.push(c),
                }
            }
            let end =
                end.ok_or("unterminated double quote (multi-line values are not supported)")?;
            (value, &v[end..])
        }
        _ => {
            // An unquoted value ends at ` #` (a comment) or the line end.
            let cut = v
                .char_indices()
                .find(|&(at, c)| c == '#' && v[..at].ends_with([' ', '\t']))
                .map_or(v.len(), |(at, _)| at);
            return Ok(v[..cut].trim_end().to_owned());
        }
    };
    let rest = rest.trim_start();
    if !(rest.is_empty() || rest.starts_with('#')) {
        return Err("unexpected text after the closing quote".to_owned());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todos::tests::TempDir;

    fn pairs(text: &str) -> Vec<(String, String)> {
        parse(text).unwrap()
    }

    fn one(text: &str) -> String {
        let mut p = pairs(text);
        assert_eq!(p.len(), 1, "{text:?}");
        p.remove(0).1
    }

    #[test]
    fn plain_lines_comments_and_export() {
        let text = "# settings\n\nCUTHULU_BIND=127.0.0.1:9000\n  export RUST_LOG = debug \n\
                    CUTHULU_SMTP_HOST=\n";
        assert_eq!(
            pairs(text),
            [
                ("CUTHULU_BIND".into(), "127.0.0.1:9000".into()),
                ("RUST_LOG".into(), "debug".into()),
                ("CUTHULU_SMTP_HOST".into(), String::new()),
            ]
        );
    }

    #[test]
    fn values() {
        assert_eq!(one("A=x # comment"), "x");
        assert_eq!(one("A=pa#ss"), "pa#ss", "# inside a word is kept");
        assert_eq!(one("A=a=b=c"), "a=b=c");
        assert_eq!(one(r"A='a\nb # c'"), r"a\nb # c");
        assert_eq!(
            one(r#"A="a\nb \"q\" \\ # c" # comment"#),
            "a\nb \"q\" \\ # c"
        );
        assert_eq!(one("A=\"  spaced  \""), "  spaced  ");
        assert_eq!(
            one("A=https://hc-ping.com/abc?x=1"),
            "https://hc-ping.com/abc?x=1"
        );
    }

    #[test]
    fn errors_name_the_line_never_the_value() {
        for (text, line) in [
            ("A=1\nnot a pair\n", 2),
            ("1A=x", 1),
            ("A B=x", 1),
            ("PASS=\"hunter2", 1),
            ("PASS='hunter2", 1),
            ("PASS=\"hunter2\" trailing", 1),
        ] {
            let (at, reason) = parse(text).unwrap_err();
            assert_eq!(at, line, "{text:?}");
            assert!(!reason.contains("hunter2"), "{reason}");
        }
    }

    #[test]
    fn load_missing_present_and_bad() {
        let dir = TempDir::new();
        let path = dir.path().join(FILE_NAME);
        let none = EnvFile::load(&path).unwrap();
        assert!(none.path().is_none() && none.keys().is_empty());

        fs::write(&path, "CUTHULU_TEST_ENVFILE_ONLY=from-file\nB=2\nB=3\n").unwrap();
        let file = EnvFile::load(&path).unwrap();
        assert_eq!(file.path(), Some(path.as_path()));
        assert_eq!(file.keys(), ["B", "CUTHULU_TEST_ENVFILE_ONLY"]);
        assert_eq!(file.lookup("B").as_deref(), Some("3"), "later wins");
        assert_eq!(
            file.lookup("CUTHULU_TEST_ENVFILE_ONLY").as_deref(),
            Some("from-file")
        );
        // The real environment wins over the file.
        assert_eq!(
            EnvFile {
                path: None,
                vars: [("PATH".to_owned(), "nope".to_owned())].into(),
            }
            .lookup("PATH"),
            std::env::var("PATH").ok()
        );

        fs::write(&path, "CUTHULU_SMTP_PASSWORD=\"secret\n").unwrap();
        let err = EnvFile::load(&path).unwrap_err().to_string();
        assert!(err.contains(":1:") && !err.contains("secret"), "{err}");
    }
}
