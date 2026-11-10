//! Setup checks ("doctor"): the report format and pure classifiers for the probes the module
//! makes against the stores and the game. The module only does HTTP; every decision is here.

use crate::event::{args_format, delivery};
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Not applicable / not run (e.g. offline mode).
    Skip,
    Info,
    Ok,
    Warn,
    Fail,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Check {
    pub id: String,
    pub title: String,
    pub level: Level,
    pub detail: String,
    /// What to do about it; empty when nothing.
    pub fix: String,
}

impl Check {
    pub fn new(id: &str, title: &str, level: Level, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Check { id: id.into(), title: title.into(), level, detail: detail.into(), fix: fix.into() }
    }
}

/// Result of a classifier: level, detail, fix.
pub type Verdict = (Level, String, String);

fn v(level: Level, detail: impl Into<String>, fix: impl Into<String>) -> Verdict {
    (level, detail.into(), fix.into())
}

#[derive(Default, Debug)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn add(&mut self, id: &str, title: &str, (level, detail, fix): Verdict) {
        self.checks.push(Check::new(id, title, level, detail, fix));
    }

    /// The worst level among the checks (`ok` for an empty report).
    pub fn summary(&self) -> Level {
        self.checks.iter().map(|c| c.level).filter(|l| *l >= Level::Ok).max().unwrap_or(Level::Ok)
    }

    pub fn to_json(&self, app_id: &str, checked_at_ms: i64, urls: Value) -> Value {
        let count = |l: Level| self.checks.iter().filter(|c| c.level == l).count();
        json!({
            "app_id": app_id,
            "checked_at_ms": checked_at_ms,
            "summary": self.summary(),
            "counts": { "ok": count(Level::Ok), "warn": count(Level::Warn), "fail": count(Level::Fail) },
            "checks": self.checks,
            "urls": urls,
        })
    }
}

/// Secrets a build needs, with whether a missing one is a failure (`true`) or a warning.
/// `store`: the build's store id (a platform id is accepted too and means its default store).
/// The store's part comes from [`crate::registry`]; optional secrets are not listed.
pub fn required_secrets(store: &str, delivery_mode: &str) -> Vec<(&'static str, bool)> {
    use crate::registry::{self, Need};
    let info = registry::store(store).or_else(|| registry::build_store(store, "").ok());
    let mut out: Vec<(&'static str, bool)> = info
        .map(|s| s.secrets.iter().filter(|x| x.need != Need::Optional).map(|x| (x.name, x.need == Need::Required)).collect())
        .unwrap_or_default();
    match delivery_mode {
        delivery::SPACETIMEDB => out.push(("spacetime_token", true)),
        delivery::WEBHOOK => out.push(("webhook_secret", true)),
        _ => {}
    }
    out
}

/// `GET .../purchases/voidedpurchases?maxResults=1`: needs the same permission as purchase lookups.
pub fn classify_google_access(status: u16, client_email: &str) -> Verdict {
    let invite = format!(
        "In Play Console > Users and permissions, invite {client_email} and grant \"View financial data\" and \
         \"Manage orders and subscriptions\" for this app. New permissions can take up to 24 hours to apply."
    );
    match status {
        200 => v(Level::Ok, "Google Play Developer API accepts the service account", ""),
        401 | 403 => v(Level::Fail, format!("Google Play rejected the service account (HTTP {status})"), invite),
        404 => v(
            Level::Fail,
            "This package is not in the developer account the service account belongs to",
            "Check app_id, or upload the service account of the account that publishes this app",
        ),
        _ => v(Level::Warn, format!("Google Play answered HTTP {status}"), "Try the check again later"),
    }
}

/// `POST /inApps/v1/notifications/test` of the App Store Server API.
pub fn classify_apple_test(status: u16, environment: &str) -> Verdict {
    match status {
        200 => v(Level::Ok, format!("App Store Server API accepts the key ({environment})"), ""),
        401 => v(
            Level::Fail,
            "App Store rejected the API key (HTTP 401)",
            "Use an In-App Purchase key (App Store Connect > Users and Access > Integrations > In-App Purchase): \
             issuer id, key id and the .p8 file must belong together",
        ),
        404 => v(
            Level::Warn,
            format!("App Store does not know this bundle id in {environment}"),
            "Check app_id; an app that is not live yet only exists in sandbox",
        ),
        429 => v(Level::Warn, "App Store rate limit, try again in a minute", ""),
        _ => v(Level::Warn, format!("App Store answered HTTP {status}"), "Try the check again later"),
    }
}

/// `GET /inApps/v1/notifications/test/{token}`: did Apple manage to deliver the test notification?
pub fn classify_apple_test_status(status: u16, body: &str) -> Verdict {
    if status != 200 {
        return v(Level::Info, format!("Apple has no result for the last test notification yet (HTTP {status})"), "");
    }
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let result = parsed["sendAttempts"]
        .as_array()
        .and_then(|a| a.last())
        .and_then(|a| a["sendAttemptResult"].as_str())
        .unwrap_or("")
        .to_string();
    match result.as_str() {
        "SUCCESS" => v(Level::Ok, "Apple delivered the test notification to the server", ""),
        "" => v(Level::Info, "Apple has not tried to deliver the test notification yet", "Run the check again in a minute"),
        other => v(
            Level::Fail,
            format!("Apple could not deliver the test notification: {other}"),
            "In App Store Connect > App Information > App Store Server Notifications set Version 2 and the URL shown \
             below for both Production and Sandbox",
        ),
    }
}

/// `GET /v1/identity/{hex}/verify` with the game token: does the token belong to that identity?
pub fn classify_identity_verify(status: u16) -> Verdict {
    match status {
        200 | 204 => v(Level::Ok, "The SpacetimeDB server accepts the billing token", ""),
        400 => v(Level::Fail, "The token does not match its identity", "Create a new token with POST /v1/identity"),
        401 => v(Level::Fail, "The SpacetimeDB server rejects the billing token", "Store a valid spacetime_token"),
        _ => v(Level::Warn, format!("SpacetimeDB answered HTTP {status} to the token check"), ""),
    }
}

/// Number of reducer parameters the billing calls with `format`.
pub fn expected_reducer_params(format: &str, is_grant: bool) -> usize {
    match (format, is_grant) {
        (args_format::LEGACY, true) => 4,
        (args_format::LEGACY, false) => 5,
        _ => 1,
    }
}

/// Checks a reducer of the game module against the schema (`GET /v1/database/{db}/schema?version=9`).
pub fn reducer_shape(schema_json: &str, reducer: &str, format: &str, is_grant: bool) -> Result<(), String> {
    let schema: Value = serde_json::from_str(schema_json).map_err(|_| "the schema is not JSON".to_string())?;
    let reducers = schema["reducers"].as_array().ok_or("the schema has no reducers")?;
    let found = reducers.iter().find(|r| r["name"].as_str() == Some(reducer));
    let Some(found) = found else {
        let names: Vec<&str> = reducers.iter().filter_map(|r| r["name"].as_str()).filter(|n| *n != "init").collect();
        return Err(format!("reducer {reducer} not found (module has: {})", names.join(", ")));
    };
    let params = found["params"]["elements"].as_array().map(|a| a.len()).unwrap_or(0);
    let expected = expected_reducer_params(format, is_grant);
    if params != expected {
        return Err(format!("reducer {reducer} takes {params} argument(s), {format} delivery sends {expected}"));
    }
    Ok(())
}

/// Identity a SpacetimeDB token belongs to (`hex_identity` claim) and whether it expires.
pub fn spacetime_token_identity(token: &str) -> Result<(String, bool), String> {
    let claims = crate::jwt::decode_jws_payload(token).map_err(|e| format!("not a SpacetimeDB token: {e}"))?;
    let hex = claims["hex_identity"].as_str().ok_or("the token has no hex_identity claim")?.to_string();
    let expires = claims.get("exp").is_some_and(|e| !e.is_null());
    Ok((hex, expires))
}

/// Body of the opt-in webhook ping. Games must answer 2xx to unknown events and ignore them.
pub fn webhook_ping_body(app_id: &str, now_ms: i64) -> String {
    json!({ "event": "ping", "app_id": app_id, "timestamp_ms": now_ms }).to_string()
}

/// Amazon RVS called with the real shared secret and a made-up user / receipt: 496 means the
/// secret is wrong; "no such receipt / user" means Amazon got past the secret.
pub fn classify_amazon_secret(status: u16) -> Verdict {
    match status {
        496 => v(
            Level::Fail,
            "Amazon rejected the shared secret (HTTP 496)",
            "Copy the shared key from Amazon Developer Console > Apps & Services > Shared Key of the account that publishes this app",
        ),
        400 | 410 | 497 => v(Level::Ok, "Amazon RVS accepts the shared secret (checked with a made-up receipt)", ""),
        429 => v(Level::Warn, "Amazon RVS rate limit, try again in a minute", ""),
        _ => v(Level::Warn, format!("Amazon RVS answered HTTP {status}"), "Try the check again later"),
    }
}

/// Notifications of a store (`store_notify_health`). Times are in microseconds; 0 = never.
/// `last_test_at`: the last test notification (Google, Apple), `confirmation_token`: the last
/// subscription confirmation (Amazon RTN), which must be pasted into the store's console.
#[allow(clippy::too_many_arguments)]
pub fn classify_store_notifications(
    store_name: &str,
    last_at: i64,
    last_test_at: i64,
    rejected_at: i64,
    rejected_reason: &str,
    confirmation_at: i64,
    confirmation_token: &str,
    when: impl Fn(i64) -> String,
    fix: &str,
) -> Verdict {
    if rejected_at > last_at.max(confirmation_at) {
        v(
            Level::Fail,
            format!("{store_name} notifications are rejected ({rejected_reason}, last {})", when(rejected_at)),
            "Check that the endpoint URL is the one shown below",
        )
    } else if last_at > 0 && last_test_at == last_at {
        v(Level::Ok, format!("Test notification received {}", when(last_at)), "")
    } else if last_at > 0 {
        v(Level::Ok, format!("Last notification {}", when(last_at)), "")
    } else if confirmation_at > 0 {
        v(
            Level::Warn,
            format!("Endpoint confirmation received {}; confirmation token: {confirmation_token}", when(confirmation_at)),
            "Paste the confirmation token into the store console to verify the endpoint (it is valid for 48 hours)",
        )
    } else {
        v(Level::Warn, format!("No {store_name} notifications received yet"), fix)
    }
}

/// What the worker last did with the events of one integration of a build (`integration_health`).
/// Times are in microseconds; 0 = never.
#[derive(Clone, Debug, Default)]
pub struct IntegrationActivity<'a> {
    pub last_sent_at: i64,
    pub last_failed_at: i64,
    pub last_error: &'a str,
    /// The last failure was final (not retryable or out of attempts): that event was dropped.
    pub last_failure_final: bool,
    /// Last event skipped because the client did not send the device id the integration needs.
    pub last_skipped_at: i64,
    pub last_skip_reason: &'a str,
}

/// The setup check of an integration (`config`: its settings and key, or a live probe) combined
/// with what really happened to its events: a failure or a skip after the last sent event wins,
/// otherwise the last sent event is shown. A broken configuration is reported as it is.
pub fn integration_activity(config: Verdict, a: &IntegrationActivity, when: impl Fn(i64) -> String) -> Verdict {
    if config.0 == Level::Fail {
        return config;
    }
    if a.last_failed_at > 0 && a.last_failed_at >= a.last_sent_at.max(a.last_skipped_at) {
        return if a.last_failure_final {
            v(
                Level::Fail,
                format!("The last event was dropped {}: {}", when(a.last_failed_at), a.last_error),
                "Fix the cause (every failure is on the Alerts and Logs pages), then use Send test event",
            )
        } else {
            v(
                Level::Warn,
                format!("The last event failed {} and is retried: {}", when(a.last_failed_at), a.last_error),
                "Usually temporary; if it keeps failing, check the key and the service status",
            )
        };
    }
    if a.last_skipped_at > a.last_sent_at {
        return v(
            Level::Warn,
            format!("Events are skipped (last {}): {}", when(a.last_skipped_at), a.last_skip_reason),
            "The game must send this id with purchases (Unity: AttributionKeys); see docs/game-integration.md",
        );
    }
    if a.last_sent_at > 0 {
        return match config.0 {
            Level::Warn => (Level::Warn, format!("{}; last event sent {}", config.1, when(a.last_sent_at)), config.2),
            _ => v(Level::Ok, format!("Last event sent {}", when(a.last_sent_at)), ""),
        };
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integration_activity_shows_failures_skips_and_the_last_event() {
        let when = |t: i64| format!("t{t}");
        let ok = || v(Level::Ok, "configured", "Send a test event");
        let a = |sent, failed, fin, skipped| IntegrationActivity {
            last_sent_at: sent,
            last_failed_at: failed,
            last_error: "HTTP 401",
            last_failure_final: fin,
            last_skipped_at: skipped,
            last_skip_reason: "client did not send appsflyer_id",
        };
        assert_eq!(integration_activity(ok(), &a(0, 0, false, 0), when), ok(), "nothing happened yet");
        assert_eq!(integration_activity(ok(), &a(5, 0, false, 0), when), v(Level::Ok, "Last event sent t5", ""));
        let dropped = integration_activity(ok(), &a(5, 7, true, 0), when);
        assert_eq!((dropped.0, dropped.1.as_str()), (Level::Fail, "The last event was dropped t7: HTTP 401"));
        assert_eq!(integration_activity(ok(), &a(5, 7, false, 0), when).0, Level::Warn, "a retry");
        assert_eq!(integration_activity(ok(), &a(9, 7, true, 0), when).0, Level::Ok, "sent again after the failure");
        let skipped = integration_activity(ok(), &a(5, 0, false, 8), when);
        assert_eq!(skipped.0, Level::Warn);
        assert!(skipped.1.contains("appsflyer_id"));
        let broken = v(Level::Fail, "ga4_api_secret is missing", "Set it");
        assert_eq!(integration_activity(broken.clone(), &a(5, 7, true, 0), when), broken, "the configuration comes first");
        let warn = v(Level::Warn, "no event tokens: nothing is sent", "Create events");
        assert_eq!(integration_activity(warn, &a(5, 0, false, 0), when).1, "no event tokens: nothing is sent; last event sent t5");
    }

    #[test]
    fn amazon_secret_and_store_notifications() {
        assert_eq!(classify_amazon_secret(496).0, Level::Fail);
        assert_eq!(classify_amazon_secret(400).0, Level::Ok);
        assert_eq!(classify_amazon_secret(500).0, Level::Warn);
        let when = |t: i64| t.to_string();
        let n = |last, rej, conf| classify_store_notifications("Amazon", last, 0, rej, "bad signature", conf, "tok-1", when, "set it up");
        assert!(classify_store_notifications("Google Play", 5, 5, 0, "", 0, "", when, "").1.starts_with("Test notification"));
        assert!(classify_store_notifications("Google Play", 7, 5, 0, "", 0, "", when, "").1.starts_with("Last notification"));
        assert_eq!(n(0, 0, 0).0, Level::Warn);
        let (l, d, _) = n(0, 0, 5);
        assert!(l == Level::Warn && d.contains("tok-1"));
        assert_eq!(n(7, 0, 5).0, Level::Ok);
        assert_eq!(n(7, 9, 5).0, Level::Fail);
    }

    #[test]
    fn report_summary_is_the_worst_level() {
        let mut r = Report::default();
        assert_eq!(r.summary(), Level::Ok);
        r.add("a", "A", v(Level::Skip, "", ""));
        r.add("b", "B", v(Level::Ok, "", ""));
        assert_eq!(r.summary(), Level::Ok);
        r.add("c", "C", v(Level::Warn, "", ""));
        assert_eq!(r.summary(), Level::Warn);
        r.add("d", "D", v(Level::Fail, "", ""));
        let j = r.to_json("app", 1, json!({}));
        assert_eq!(j["summary"], "fail");
        assert_eq!(j["counts"], json!({ "ok": 1, "warn": 1, "fail": 1 }));
        assert_eq!(j["checks"][0]["level"], "skip");
    }

    #[test]
    fn required_secrets_depend_on_platform_and_delivery() {
        let a = required_secrets("android", "spacetimedb");
        assert!(a.contains(&("google_service_account_json", true)));
        assert!(a.contains(&("google_rtdn_secret", false)));
        assert!(a.contains(&("spacetime_token", true)));
        let i = required_secrets("ios", "webhook");
        assert!(i.contains(&("apple_private_key_p8", true)));
        assert!(i.contains(&("webhook_secret", true)));
        assert!(!required_secrets("ios", "none").iter().any(|(n, _)| n.starts_with("spacetime") || n.starts_with("webhook")));
        assert_eq!(required_secrets("apple", "none"), required_secrets("ios", "none"));
        assert_eq!(required_secrets("amazon", "none"), vec![("amazon_shared_secret", true)]);
    }

    #[test]
    fn google_access_classification() {
        assert_eq!(classify_google_access(200, "sa@x").0, Level::Ok);
        let (l, _, fix) = classify_google_access(403, "sa@proj.iam.gserviceaccount.com");
        assert_eq!(l, Level::Fail);
        assert!(fix.contains("sa@proj.iam.gserviceaccount.com"));
        assert_eq!(classify_google_access(401, "").0, Level::Fail);
        assert_eq!(classify_google_access(404, "").0, Level::Fail);
        assert_eq!(classify_google_access(503, "").0, Level::Warn);
    }

    #[test]
    fn apple_classification() {
        assert_eq!(classify_apple_test(200, "production").0, Level::Ok);
        assert_eq!(classify_apple_test(401, "production").0, Level::Fail);
        assert_eq!(classify_apple_test(404, "sandbox").0, Level::Warn);
        let ok = r#"{"signedPayload":"x","sendAttempts":[{"attemptDate":1,"sendAttemptResult":"SUCCESS"}]}"#;
        assert_eq!(classify_apple_test_status(200, ok).0, Level::Ok);
        let bad = r#"{"sendAttempts":[{"attemptDate":1,"sendAttemptResult":"NO_RESPONSE"}]}"#;
        let (l, d, _) = classify_apple_test_status(200, bad);
        assert_eq!(l, Level::Fail);
        assert!(d.contains("NO_RESPONSE"));
        assert_eq!(classify_apple_test_status(404, "").0, Level::Info);
    }

    #[test]
    fn identity_verify_classification() {
        assert_eq!(classify_identity_verify(204).0, Level::Ok);
        assert_eq!(classify_identity_verify(400).0, Level::Fail);
        assert_eq!(classify_identity_verify(401).0, Level::Fail);
        assert_eq!(classify_identity_verify(500).0, Level::Warn);
    }

    /// Trimmed from `GET /v1/database/<db>/schema?version=9` of a SpacetimeDB 2.14.1 module.
    const SCHEMA: &str = r#"{"typespace":{},"tables":[],"reducers":[
        {"name":"billing_grant","params":{"elements":[{"name":{"some":"ev"},"algebraic_type":{"Ref":0}}]},"lifecycle":{"none":[]}},
        {"name":"server_fulfill_purchase","params":{"elements":[
            {"name":{"some":"user"},"algebraic_type":{"String":[]}},{"name":{"some":"tx"},"algebraic_type":{"String":[]}},
            {"name":{"some":"product"},"algebraic_type":{"String":[]}},{"name":{"some":"exp"},"algebraic_type":{"I64":[]}}]},"lifecycle":{"none":[]}},
        {"name":"init","params":{"elements":[]},"lifecycle":{"some":{"Init":[]}}}
    ],"types":[],"misc_exports":[],"row_level_security":[]}"#;

    #[test]
    fn reducer_shape_matches_args_format() {
        assert_eq!(reducer_shape(SCHEMA, "billing_grant", "v1", true), Ok(()));
        assert_eq!(reducer_shape(SCHEMA, "server_fulfill_purchase", "legacy", true), Ok(()));
        let e = reducer_shape(SCHEMA, "billing_revoke", "v1", false).unwrap_err();
        assert!(e.contains("not found") && e.contains("billing_grant") && !e.contains("init"), "{e}");
        let e = reducer_shape(SCHEMA, "billing_grant", "legacy", true).unwrap_err();
        assert!(e.contains("takes 1"), "{e}");
        assert!(reducer_shape("<html>", "x", "v1", true).is_err());
    }

    #[test]
    fn token_identity_is_read_from_the_claims() {
        let payload = crate::jwt::b64url(br#"{"hex_identity":"c200ab","sub":"x","iss":"localhost","exp":null}"#);
        assert_eq!(spacetime_token_identity(&format!("h.{payload}.s")), Ok(("c200ab".to_string(), false)));
        let expiring = crate::jwt::b64url(br#"{"hex_identity":"c200ab","exp":1}"#);
        assert_eq!(spacetime_token_identity(&format!("h.{expiring}.s")), Ok(("c200ab".to_string(), true)));
        assert!(spacetime_token_identity("plain-token").is_err());
    }
}
