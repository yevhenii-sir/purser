//! URL, form and header encodings shared by the store and integration clients.

use base64::Engine;

/// Percent-encodes a URL component: RFC 3986 unreserved characters stay, everything else is `%XX`.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` escapes (a malformed escape stays as it is; `+` is not a space).
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let hex = |b: u8| (b as char).to_digit(16);
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `k=v&...` with both sides percent-encoded (`%20` for spaces).
pub fn query_string(pairs: &[(&str, String)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v))).collect::<Vec<_>>().join("&")
}

/// `application/x-www-form-urlencoded` body (`+` for spaces).
pub fn form_urlencode(pairs: &[(&str, String)]) -> String {
    let enc = |s: &str| percent_encode(s).replace("%20", "+");
    pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

/// The value of the query parameter `name` in a raw query string, percent-decoded.
pub fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| percent_decode(v))
}

/// Compares secrets without an early exit on the first differing byte.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Authorization: Basic ...` header value.
pub fn basic_auth(user: &str, password: &str) -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}")))
}

/// An `https://` URL of a public host name: no credentials in it, no IP literal, no local name
/// (webhooks and alert channels receive player data and are set by editors).
pub fn check_public_https_url(what: &str, url: &str) -> Result<(), String> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(format!("{what} must be an https:// URL"));
    };
    if url.len() > 2048 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{what} is not a valid URL"));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once(':').map_or(authority, |(h, port)| if port.bytes().all(|b| b.is_ascii_digit()) { h } else { authority });
    let host = host.to_ascii_lowercase();
    let ip_literal = host.starts_with('[') || host.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    let local = host == "localhost" || [".localhost", ".local", ".internal"].iter().any(|s| host.ends_with(s));
    if host.is_empty() || authority.contains('@') || ip_literal || local || !host.contains('.') {
        return Err(format!("{what} must name a public host (no user info, IP address or local name)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes() {
        assert_eq!(percent_encode("a b/c+=~"), "a%20b%2Fc%2B%3D~");
        assert_eq!(percent_decode("a%20b%2Fc%2B%3D~"), "a b/c+=~");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode("plus+stays"), "plus+stays");
        assert_eq!(form_urlencode(&[("a b", "c d".into())]), "a+b=c+d");
        assert_eq!(query_string(&[("a b", "c&d".into())]), "a%20b=c%26d");
    }

    #[test]
    fn reads_query_params() {
        assert_eq!(query_param("app=com.x&secret=a%2Bb", "secret").as_deref(), Some("a+b"));
        assert_eq!(query_param("app=com.x", "secret"), None);
        assert_eq!(query_param("", "app"), None);
    }

    #[test]
    fn public_https_urls_only() {
        assert!(check_public_https_url("url", "https://hooks.example.com/billing?x=1").is_ok());
        assert!(check_public_https_url("url", "https://hooks.example.com:8443/b").is_ok());
        for bad in [
            "http://hooks.example.com/b",
            "https://",
            "https://user:pw@hooks.example.com/b",
            "https://hooks.example.com@evil.example/b",
            "https://169.254.169.254/latest",
            "https://[::1]/x",
            "https://localhost/x",
            "https://printer.local/x",
            "https://intranet/x",
            "https://hooks.example.com/a b",
        ] {
            assert!(check_public_https_url("url", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn compares_and_builds_auth() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert_eq!(basic_auth("user", "pass"), "Basic dXNlcjpwYXNz");
    }
}
