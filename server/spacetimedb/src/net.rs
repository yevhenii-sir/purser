//! Thin wrapper over the SpacetimeDB HTTP client.
//! Every outbound request gets [`TIMEOUT`] (the host allows up to 180 s, 30 s when none is set; the
//! 500 ms in the `spacetimedb` crate docs is outdated). A procedure waits for the reply, so keep
//! calls few per run. Since SpacetimeDB 2.12 the host negotiates HTTP/2 when the remote supports
//! it (Google APIs do), falling back to HTTP/1.1.

use spacetimedb::http::{Body, HttpClient, Timeout};
use spacetimedb::TimeDuration;

/// Time limit of one outbound request.
pub const TIMEOUT_SECS: i64 = 10;

pub struct HttpResult {
    pub status: u16,
    pub body: String,
}

impl HttpResult {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Short description for logs / `last_error`.
    pub fn describe(&self) -> String {
        let body: String = self.body.chars().take(300).collect();
        format!("HTTP {}: {}", self.status, body)
    }
}

pub fn send(
    http: &HttpClient,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: impl Into<Body>,
) -> Result<HttpResult, String> {
    let mut builder = http::Request::builder()
        .method(method)
        .uri(url)
        .extension(Timeout::from(TimeDuration::from_micros(TIMEOUT_SECS * 1_000_000)));
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let request = builder.body(body.into()).map_err(|e| format!("bad request {url}: {e}"))?;
    let response = http.send(request).map_err(|e| format!("{method} {url} failed: {e}"))?;
    let (parts, body) = response.into_parts();
    Ok(HttpResult { status: parts.status.as_u16(), body: body.into_string_lossy() })
}

pub fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}
