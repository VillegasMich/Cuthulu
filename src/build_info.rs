//! What this binary is: the package version and, when it was injected at
//! build time, the git commit it was built from.
//!
//! The commit comes from `CUTHULU_BUILD_SHA` at *compile* time (the image's
//! `GIT_SHA` build arg, which CI sets); it is not runtime configuration.
//! Without it (e.g. `cargo run`) only the version is shown.

use serde::Serialize;

/// `Cargo.toml`'s version, e.g. `0.1.0`. Releases are tagged `v<VERSION>`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The project's GitHub repository, `Cargo.toml`'s `repository`.
pub const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// GitHub release page of [`VERSION`], under [`REPO_URL`] (`concat!` needs
/// the literal, hence the repeated `env!`).
pub const RELEASE_URL: &str = concat!(
    env!("CARGO_PKG_REPOSITORY"),
    "/releases/tag/v",
    env!("CARGO_PKG_VERSION")
);

/// Body of `GET /api/version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BuildInfo {
    pub version: &'static str,
    /// Full commit sha, `null` when none was injected.
    pub git_sha: Option<&'static str>,
}

#[must_use]
pub fn info() -> BuildInfo {
    BuildInfo {
        version: VERSION,
        git_sha: git_sha(),
    }
}

/// The commit this binary was built from, if injected at build time.
#[must_use]
pub fn git_sha() -> Option<&'static str> {
    parse_sha(option_env!("CUTHULU_BUILD_SHA"))
}

/// The first 7 characters of [`git_sha`], for display.
#[must_use]
pub fn short_sha() -> Option<&'static str> {
    git_sha().map(|s| &s[..7])
}

/// Accepts a hex sha of 7 to 64 characters (SHA-1 or SHA-256); an empty or
/// malformed value counts as none.
fn parse_sha(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim)
        .filter(|s| (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_injected_sha() {
        let full = "219051a0e3c1b2f4d5e6a7b8c9d0e1f2a3b4c5d6";
        assert_eq!(parse_sha(Some(full)), Some(full));
        assert_eq!(parse_sha(Some(" 219051a\n")), Some("219051a"));
    }

    #[test]
    fn ignores_missing_or_malformed_sha() {
        for raw in [
            None,
            Some(""),
            Some("  "),
            Some("219051"),
            Some("not-a-sha-at-all"),
        ] {
            assert_eq!(parse_sha(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn repo_url_is_the_github_repository() {
        assert_eq!(REPO_URL, "https://github.com/VillegasMich/cuthulu");
        assert!(RELEASE_URL.starts_with(REPO_URL));
    }

    #[test]
    fn release_url_points_at_the_version_tag() {
        assert_eq!(
            RELEASE_URL,
            format!("https://github.com/VillegasMich/cuthulu/releases/tag/v{VERSION}")
        );
    }
}
