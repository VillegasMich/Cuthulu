//! CSRF protection for state-changing requests.
//!
//! The dashboard sends `X-Cuthulu: 1` on every POST. A custom header cannot be
//! set by a cross-site form, and a cross-site `fetch` with it triggers a CORS
//! preflight that this server never approves. `Origin` and `Sec-Fetch-Site`
//! are checked as well when the browser sends them.

use axum::http::HeaderMap;
use axum::http::header::{HOST, ORIGIN};

use super::ApiError;

pub const HEADER: &str = "x-cuthulu";

pub fn same_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let deny = |why: &str| {
        Err(ApiError::Forbidden(format!(
            "cross-origin request rejected: {why}"
        )))
    };

    if header(HEADER) != Some("1") {
        return deny("missing X-Cuthulu header");
    }
    if let Some(site) = header("sec-fetch-site")
        && !matches!(site, "same-origin" | "none")
    {
        return deny("sec-fetch-site");
    }
    if let Some(origin) = header(ORIGIN.as_str()) {
        let origin_host = origin.split_once("://").map_or(origin, |(_, rest)| rest);
        if Some(origin_host) != header(HOST.as_str()) {
            return deny("origin does not match host");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn accepts_same_origin() {
        let h = headers(&[
            (HEADER, "1"),
            ("host", "localhost:8686"),
            ("origin", "http://localhost:8686"),
            ("sec-fetch-site", "same-origin"),
        ]);
        assert!(same_origin(&h).is_ok());
    }

    #[test]
    fn accepts_port_less_host() {
        // Port 80 (or 443 behind a proxy): the browser sends neither side a port.
        for origin in ["http://box.tail1234.ts.net", "https://box.tail1234.ts.net"] {
            let h = headers(&[
                (HEADER, "1"),
                ("host", "box.tail1234.ts.net"),
                ("origin", origin),
                ("sec-fetch-site", "same-origin"),
            ]);
            assert!(same_origin(&h).is_ok(), "{origin}");
        }
        let short = headers(&[(HEADER, "1"), ("host", "box"), ("origin", "http://box")]);
        assert!(same_origin(&short).is_ok());
    }

    #[test]
    fn rejects_origin_whose_port_differs_from_host() {
        let h = headers(&[
            (HEADER, "1"),
            ("host", "box.tail1234.ts.net"),
            ("origin", "http://box.tail1234.ts.net:8686"),
        ]);
        assert!(same_origin(&h).is_err());
    }

    #[test]
    fn accepts_non_browser_clients_with_header() {
        assert!(same_origin(&headers(&[(HEADER, "1")])).is_ok());
    }

    #[test]
    fn rejects_cross_site() {
        assert!(same_origin(&headers(&[])).is_err());
        assert!(
            same_origin(&headers(&[
                (HEADER, "1"),
                ("host", "localhost:8686"),
                ("origin", "http://evil.example"),
            ]))
            .is_err()
        );
        assert!(same_origin(&headers(&[(HEADER, "1"), ("sec-fetch-site", "cross-site")])).is_err());
    }
}
