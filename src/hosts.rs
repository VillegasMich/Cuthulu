//! Which `Host` names the server answers to (DNS-rebinding protection).
//!
//! A page on `evil.example` can repoint its own name at this machine; the
//! browser then treats it as same-origin and sends `Host: evil.example`.
//! Refusing names an attacker could control closes that hole. Accepted
//! without configuration: IP literals, single-label names (`localhost`,
//! `MagicDNS` short names) and suffixes nobody can register publicly
//! (`.ts.net`, `.local`, `.lan`, `.home.arpa`, `.internal`, `.localhost`).

/// Suffixes accepted out of the box: Tailscale's `MagicDNS` domain and names
/// reserved for private networks.
const PRIVATE_SUFFIXES: [&str; 6] = [
    ".ts.net",
    ".local",
    ".lan",
    ".home.arpa",
    ".internal",
    ".localhost",
];

/// Extra names from `CUTHULU_ALLOWED_HOSTS`, on top of the built-in rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowedHosts {
    /// `*`: answer to any name (turns the check off).
    any: bool,
    /// Exact names, lowercase.
    names: Vec<String>,
    /// From `.example.com`: the name itself and its subdomains, lowercase,
    /// without the leading dot.
    domains: Vec<String>,
}

impl AllowedHosts {
    /// Parses a comma-separated list of `name`, `.domain` or `*`.
    ///
    /// # Errors
    /// Returns the reason when an entry is not a bare host name (ports,
    /// schemes and paths are refused).
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut out = Self::default();
        for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            if entry == "*" {
                out.any = true;
                continue;
            }
            let (domain, name) = match entry.strip_prefix('.') {
                Some(rest) => (true, rest),
                None => (false, entry),
            };
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            let valid = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
            if !valid {
                return Err(format!(
                    "`{entry}` is not a host name (expected e.g. box.example.com, .example.com or *)"
                ));
            }
            if domain {
                out.domains.push(name);
            } else {
                out.names.push(name);
            }
        }
        Ok(out)
    }

    /// Whether a request with this `Host` header value may be served.
    #[must_use]
    pub fn allows(&self, host_header: &str) -> bool {
        if self.any {
            return true;
        }
        let host = host_name(host_header)
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }
        host.parse::<std::net::IpAddr>().is_ok()
            || !host.contains('.')
            || PRIVATE_SUFFIXES.iter().any(|s| host.ends_with(s))
            || self.names.contains(&host)
            || self.domains.iter().any(|d| {
                host == *d
                    || host
                        .strip_suffix(d.as_str())
                        .is_some_and(|p| p.ends_with('.'))
            })
    }
}

/// The host part of a `Host` header: without port, IPv6 without brackets.
fn host_name(value: &str) -> &str {
    if let Some(rest) = value.strip_prefix('[') {
        return rest.split_once(']').map_or(rest, |(ip, _)| ip);
    }
    value.rsplit_once(':').map_or(value, |(host, _)| host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> AllowedHosts {
        AllowedHosts::default()
    }

    #[test]
    fn accepts_local_and_tailnet_names_without_configuration() {
        for host in [
            "localhost",
            "localhost:8686",
            "127.0.0.1",
            "127.0.0.1:80",
            "[::1]:8686",
            "[fd7a:115c:a1e0::1]",
            "100.115.90.103",
            "192.168.1.20:80",
            "box",
            "Box:80",
            "box.tail1234.ts.net",
            "cuthulu.tail1234.ts.net.",
            "box.local",
            "box.lan",
            "box.home.arpa",
            "box.internal",
            "app.localhost",
        ] {
            assert!(builtin().allows(host), "{host}");
        }
    }

    #[test]
    fn rejects_public_names() {
        for host in [
            "evil.example",
            "evil.example:80",
            "ts.net.evil.example",
            "evilts.net",
            "box.example.com",
            "",
            ":80",
        ] {
            assert!(!builtin().allows(host), "{host}");
        }
    }

    #[test]
    fn extra_names_and_domains() {
        let a = AllowedHosts::parse(" box.example.com , .home.example.org,").unwrap();
        assert!(a.allows("box.example.com:80"));
        assert!(a.allows("BOX.example.com"));
        assert!(!a.allows("other.example.com"));
        assert!(a.allows("home.example.org"));
        assert!(a.allows("nas.home.example.org"));
        assert!(!a.allows("evilhome.example.org"));
        assert!(a.allows("box"), "built-in rules still apply");
    }

    #[test]
    fn star_allows_everything() {
        assert!(AllowedHosts::parse("*").unwrap().allows("evil.example"));
    }

    #[test]
    fn rejects_entries_that_are_not_host_names() {
        for bad in ["http://box.example.com", "box.example.com:80", "box/x", "."] {
            assert!(AllowedHosts::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(AllowedHosts::parse(" , ").unwrap(), AllowedHosts::default());
    }
}
