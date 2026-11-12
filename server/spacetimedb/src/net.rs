//! Thin wrapper over the SpacetimeDB HTTP client.
//! Every outbound request gets [`TIMEOUT_SECS`] (the host allows up to 180 s, 30 s when none is set; the
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
    let shown = shown_url(url);
    let request = builder.body(body.into()).map_err(|e| format!("bad request {shown}: {}", hide_query(&e.to_string(), url)))?;
    let response = http.send(request).map_err(|e| format!("{method} {shown} failed: {}", hide_query(&e.to_string(), url)))?;
    let (parts, body) = response.into_parts();
    Ok(HttpResult { status: parts.status.as_u16(), body: body.into_string_lossy() })
}

/// `url` without its query and fragment: what an error may show. Query strings carry API keys
/// (GA4 `api_secret`, AppMetrica `post_api_key`), and errors reach logs, the admin page and alerts.
pub fn shown_url(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// A transport error with the query of `url` (and every longer query value, in whatever encoding the
/// host repeated it) removed.
fn hide_query(error: &str, url: &str) -> String {
    let shown = shown_url(url);
    let query = &url[shown.len()..];
    if query.is_empty() {
        return error.to_string();
    }
    let mut out = error.replace(query, "");
    for value in query[1..].split(['&', '#']).filter_map(|kv| kv.split_once('=').map(|(_, v)| v)) {
        if value.len() >= 6 {
            out = out.replace(value, "<hidden>");
        }
    }
    out
}

pub fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}
