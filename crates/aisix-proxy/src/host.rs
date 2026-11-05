//! Inbound authority shared by URL rewriting and passthrough routing.

use axum::extract::Request;
use axum::http::header;

/// The URI authority takes precedence over `Host` for absolute-form requests
/// (RFC 9112 section 3.2.2). Origin-form requests use `Host`.
/// Lowercased, `:port` stripped.
///
/// `None` for a value that is not a hostname, or a bracketed IPv6 literal,
/// with an optional port: such a request matches no host-scoped route. Every
/// host match reads this, and a `preserve_host` route builds its upstream URL
/// from it, where `evil%2ecom#.example.com` would pass a `*.example.com`
/// allowlist and then parse to the host `evil.com`.
pub(crate) fn inbound_host(req: &Request) -> Option<String> {
    let raw = req.uri().authority().map(|a| a.as_str()).or_else(|| {
        req.headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
    })?;
    let no_port = raw.rsplit_once(':').map_or(raw, |(head, port)| {
        // Only treat the suffix as a port when it is all digits — an
        // IPv6 literal's last group would otherwise be truncated.
        if port.chars().all(|c| c.is_ascii_digit()) {
            head
        } else {
            raw
        }
    });
    let host = no_port.trim_end_matches('.');
    is_hostname(host).then(|| host.to_ascii_lowercase())
}

fn is_hostname(host: &str) -> bool {
    if let Some(literal) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return literal.parse::<std::net::Ipv6Addr>().is_ok();
    }
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn host_of(value: &str) -> Option<String> {
        let req = Request::builder()
            .uri("/x")
            .header(header::HOST, value)
            .body(Body::empty())
            .unwrap();
        inbound_host(&req)
    }

    #[test]
    fn a_hostname_keeps_matching_without_its_port() {
        for (value, want) in [
            ("API.Example.com", "api.example.com"),
            ("api.example.com:8443", "api.example.com"),
            ("api.example.com.", "api.example.com"),
            ("my_service:9000", "my_service"),
            ("[2001:db8::1]:443", "[2001:db8::1]"),
            ("[::1]", "[::1]"),
        ] {
            assert_eq!(host_of(value).as_deref(), Some(want), "{value}");
        }
    }

    #[test]
    fn a_value_a_url_parser_reads_as_another_host_is_no_host() {
        for value in [
            "evil%2ecom#.example.com",
            "evil%2ecom:8443#.example.com",
            "evil/.example.com",
            "evil?.example.com",
            "evil\\.example.com",
            "user@evil.example.com",
            "evil:443.example.com",
            "[not-an-address]",
            "",
        ] {
            assert_eq!(host_of(value), None, "{value}");
        }
    }

    #[test]
    fn an_absolute_form_authority_with_userinfo_is_no_host() {
        let req = Request::builder()
            .uri("http://user@evil.example.com/x")
            .body(Body::empty())
            .unwrap();
        assert_eq!(inbound_host(&req), None);
    }
}
