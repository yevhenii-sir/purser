//! Moving to another server (docs/moving-servers.md): the old database first pauses its routes
//! (clients and stores get `503` + `Retry-After` and try again, nothing is lost), the database is
//! copied, then the old one forwards every route to the new one until nothing reaches it any more.

use serde_json::Value;

/// What the old server does with its routes.
pub mod mode {
    /// Every client, server-key and store route answers `503` with `Retry-After`; the worker waits.
    pub const PAUSED: &str = "paused";
    /// Every such route is sent on to the new server and its answer returned.
    pub const FORWARDING: &str = "forwarding";
    pub const ALL: &[&str] = &[PAUSED, FORWARDING];
}

/// Seconds a paused route asks callers to wait.
pub const RETRY_AFTER_SECS: u64 = 60;

/// Set on a forwarded request: a server that is forwarding itself refuses such a request instead of
/// sending it on again (a target pointing back at a forwarding server would loop).
pub const FORWARDED_HEADER: &str = "x-purser-forwarded";

/// Error code of a paused or failed forward.
pub const ERR_MOVING: &str = "ERR_MOVING";

/// The new server's route base: an https URL of a public host (`https://<host>/v1/database/<db>/route`
/// or a stable address), no query or fragment, no trailing slash.
pub fn check_target(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    crate::encoding::check_public_https_url("the new server's address", url).map_err(|e| format!("[ERR_BAD_TARGET] {e}"))?;
    if url.contains(['?', '#']) {
        return Err("[ERR_BAD_TARGET] the new server's address is its route base, without query or fragment".into());
    }
    Ok(url.to_string())
}

/// The part of a request path after the database's route base: `/v1/database/<db>/route/verify/google`
/// → `/verify/google` (a path without `/route/` is taken as it is).
pub fn route_path(path: &str) -> &str {
    match path.rfind("/route/") {
        Some(i) => &path[i + "/route".len()..],
        None => path,
    }
}

/// Where a forwarded request goes.
pub fn forward_url(target: &str, path: &str, query: Option<&str>) -> String {
    let path = route_path(path);
    match query.filter(|q| !q.is_empty()) {
        Some(q) => format!("{target}{path}?{q}"),
        None => format!("{target}{path}"),
    }
}

/// Headers that belong to one connection, or that the client library sets itself.
const NOT_FORWARDED: &[&str] = &[
    "host", "content-length", "connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade",
    "proxy-authorization", "proxy-authenticate", "proxy-connection", "accept-encoding",
];

/// A request header sent on to the new server: everything the routes read (`Authorization`, store
/// signatures, Telegram's secret token), not the connection's own headers.
pub fn forwards_request_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    !NOT_FORWARDED.contains(&name.as_str()) && name != FORWARDED_HEADER
}

/// A response header returned to the caller (`Content-Type`, `Retry-After`, `X-Purser-Signature`, ...).
pub fn returns_response_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    !NOT_FORWARDED.contains(&name.as_str()) && name != "content-encoding"
}

/// The build a request is about, for the counts of what still reaches the old server: the `app`
/// query parameter of store notifications, else `app_id` of a JSON body. Empty when neither.
pub fn app_of(query: &str, body: &[u8]) -> String {
    let from_query = crate::encoding::query_param(query, "app").unwrap_or_default();
    let found = if from_query.is_empty() {
        serde_json::from_slice::<Value>(body).ok().and_then(|v| v["app_id"].as_str().map(str::to_string)).unwrap_or_default()
    } else {
        from_query
    };
    clean(&found, 128)
}

/// The route of a request as the counts name it: `/verify/google`, at most 64 characters.
pub fn route_label(path: &str) -> String {
    clean(route_path(path), 64)
}

fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':')).take(max).collect()
}

/// The answer of a paused route.
pub fn paused_body() -> Value {
    serde_json::json!({
        "status": "error",
        "code": ERR_MOVING,
        "message": format!("this billing server is moving to another one: try again in {RETRY_AFTER_SECS} seconds"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_public_https_route_bases() {
        assert_eq!(check_target(" https://stdb.example.com/v1/database/new/route/ ").unwrap(), "https://stdb.example.com/v1/database/new/route");
        assert_eq!(check_target("https://billing.example.com").unwrap(), "https://billing.example.com");
        for bad in ["http://stdb.example.com/route", "https://localhost:3000/route", "https://10.1.2.3/route", "https://x.example.com/route?a=1", ""] {
            assert!(check_target(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn requests_go_to_the_same_route_of_the_new_server() {
        assert_eq!(route_path("/v1/database/old/route/verify/google"), "/verify/google");
        assert_eq!(route_path("/notify/apple"), "/notify/apple");
        assert_eq!(
            forward_url("https://new.example.com/v1/database/new/route", "/v1/database/old/route/notify/google", Some("app=com.game&secret=s")),
            "https://new.example.com/v1/database/new/route/notify/google?app=com.game&secret=s"
        );
        assert_eq!(forward_url("https://billing.example.com", "/entitlements", Some("")), "https://billing.example.com/entitlements");
    }

    #[test]
    fn signatures_and_tokens_go_along_connection_headers_do_not() {
        for keep in ["Authorization", "Content-Type", "Stripe-Signature", "X-Telegram-Bot-Api-Secret-Token", "x-amz-sns-message-type", "X-Purser-Client"] {
            assert!(forwards_request_header(keep), "{keep}");
        }
        for drop in ["Host", "Content-Length", "Connection", "Transfer-Encoding", "Accept-Encoding", "X-Purser-Forwarded"] {
            assert!(!forwards_request_header(drop), "{drop}");
        }
        assert!(returns_response_header("X-Purser-Signature") && returns_response_header("Retry-After") && returns_response_header("content-type"));
        assert!(!returns_response_header("Content-Length") && !returns_response_header("Content-Encoding"));
    }

    #[test]
    fn counts_name_the_build_and_the_route() {
        assert_eq!(app_of("app=com.game&secret=x", b""), "com.game");
        assert_eq!(app_of("", br#"{"app_id":"com.game.ios","user_id":"p"}"#), "com.game.ios");
        assert_eq!(app_of("", b"not json"), "");
        assert_eq!(app_of("app=a%3Cscript%3E", b""), "ascript");
        assert_eq!(route_label("/v1/database/old/route/verify/google"), "/verify/google");
    }
}
