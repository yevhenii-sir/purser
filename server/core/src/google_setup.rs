//! Google Cloud setup of a Google Play developer account, run once with an access token of the
//! project's owner (`gcloud auth print-access-token`): the APIs, the service account and its
//! key, the Pub/Sub topic of real-time developer notifications with the publisher grant for
//! Google Play, and one push subscription per build, authenticated with an OIDC token of the
//! service account ([`push_config`], checked by `google_oidc`). Request builders and answer parsers; the
//! module makes the calls (`store_setup::setup_google`).
//!
//! What has no API stays manual: inviting the service account in Play Console and naming the
//! topic in Monetization setup ([`manual_steps`]).

use base64::Engine;
use serde_json::{json, Value};

pub const SERVICE_USAGE: &str = "https://serviceusage.googleapis.com/v1";
pub const IAM: &str = "https://iam.googleapis.com/v1";
pub const PUBSUB: &str = "https://pubsub.googleapis.com/v1";

/// APIs the setup and the server use.
pub const SERVICES: &[&str] = &["androidpublisher.googleapis.com", "pubsub.googleapis.com", "iam.googleapis.com"];
/// Account id of the service account the setup creates.
pub const SERVICE_ACCOUNT_ID: &str = "purser-billing";
pub const DEFAULT_TOPIC: &str = "play-billing";
/// Google Play publishes real-time developer notifications as this account.
pub const PLAY_PUBLISHER: &str = "serviceAccount:google-play-developer-notifications@system.gserviceaccount.com";
pub const PUBLISHER_ROLE: &str = "roles/pubsub.publisher";

/// A Google Cloud project id: 6-30 lowercase letters, digits and hyphens, starting with a letter.
pub fn check_project(project: &str) -> Result<(), String> {
    let ok = (6..=30).contains(&project.len())
        && project.starts_with(|c: char| c.is_ascii_lowercase())
        && !project.ends_with('-')
        && project.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok { Ok(()) } else { Err(format!("'{project}' is not a Google Cloud project id")) }
}

/// A Pub/Sub topic or subscription id (the subset of Pub/Sub's rules that needs no escaping).
fn check_resource_id(what: &str, id: &str) -> Result<(), String> {
    let ok = (3..=255).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_alphabetic())
        && !id.starts_with("goog")
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok { Ok(()) } else { Err(format!("'{id}' is not a valid Pub/Sub {what} id")) }
}

pub fn check_topic(topic: &str) -> Result<(), String> {
    check_resource_id("topic", topic)
}

/// The access token is sent as a header only; it must look like one.
pub fn check_access_token(token: &str) -> Result<(), String> {
    if token.len() < 20 || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("the access token is missing or malformed: paste the output of `gcloud auth print-access-token`".into());
    }
    Ok(())
}

// --- APIs -----------------------------------------------------------------------------------

pub fn service_url(project: &str, service: &str) -> String {
    format!("{SERVICE_USAGE}/projects/{project}/services/{service}")
}

pub fn service_enabled(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|v| v["state"] == "ENABLED")
}

pub fn batch_enable_url(project: &str) -> String {
    format!("{SERVICE_USAGE}/projects/{project}/services:batchEnable")
}

pub fn batch_enable_body(services: &[&str]) -> String {
    json!({ "serviceIds": services }).to_string()
}

/// A long-running operation that has finished without an error.
pub fn operation_done(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|v| v["done"] == true && v["error"].is_null())
}

// --- Service account and key ----------------------------------------------------------------

pub fn service_account_email(project: &str) -> String {
    format!("{SERVICE_ACCOUNT_ID}@{project}.iam.gserviceaccount.com")
}

pub fn service_account_url(project: &str) -> String {
    format!("{IAM}/projects/{project}/serviceAccounts/{}", service_account_email(project))
}

pub fn create_service_account_url(project: &str) -> String {
    format!("{IAM}/projects/{project}/serviceAccounts")
}

pub fn create_service_account_body() -> String {
    json!({
        "accountId": SERVICE_ACCOUNT_ID,
        "serviceAccount": {
            "displayName": "Purser billing",
            "description": "Purser verifies Google Play purchases with this account (no Google Cloud roles needed)",
        },
    })
    .to_string()
}

pub fn keys_url(project: &str) -> String {
    format!("{}/keys", service_account_url(project))
}

pub fn create_key_body() -> String {
    json!({ "privateKeyType": "TYPE_GOOGLE_CREDENTIALS_FILE", "keyAlgorithm": "KEY_ALG_RSA_2048" }).to_string()
}

/// The JSON key file of a created key, checked to belong to `email`.
pub fn parse_key(body: &str, email: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad IAM key response: {e}"))?;
    let data = v["privateKeyData"].as_str().ok_or("IAM answered without the key")?;
    let file = base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| "IAM answered with a key that is not base64")?;
    let file = String::from_utf8(file).map_err(|_| "IAM answered with a key that is not text")?;
    let parsed: Value = serde_json::from_str(&file).map_err(|_| "IAM answered with a key file that is not JSON")?;
    if parsed["client_email"] != email || !parsed["private_key"].is_string() {
        return Err("IAM answered with a key of another service account".into());
    }
    Ok(file)
}

// --- Topic ----------------------------------------------------------------------------------

pub fn topic_path(project: &str, topic: &str) -> String {
    format!("projects/{project}/topics/{topic}")
}

/// `GET` / `PUT` (create, body `{}`).
pub fn topic_url(project: &str, topic: &str) -> String {
    format!("{PUBSUB}/{}", topic_path(project, topic))
}

pub fn get_policy_url(project: &str, topic: &str) -> String {
    format!("{}:getIamPolicy", topic_url(project, topic))
}

pub fn set_policy_url(project: &str, topic: &str) -> String {
    format!("{}:setIamPolicy", topic_url(project, topic))
}

/// The policy with `member` added to `role`, or `None` when it is there already. The rest of
/// the policy, `etag` included, is kept, so a concurrent change makes the write fail instead of
/// being overwritten.
pub fn with_binding(policy: &Value, role: &str, member: &str) -> Option<Value> {
    let mut policy = if policy.is_object() { policy.clone() } else { json!({}) };
    let bindings = policy["bindings"].as_array().cloned().unwrap_or_default();
    let has = |b: &Value| b["role"] == role && b["condition"].is_null();
    if bindings.iter().any(|b| has(b) && b["members"].as_array().is_some_and(|m| m.iter().any(|x| x == member))) {
        return None;
    }
    let mut bindings = bindings;
    match bindings.iter_mut().find(|b| has(b)) {
        Some(b) => {
            if !b["members"].is_array() {
                b["members"] = json!([]);
            }
            if let Some(members) = b["members"].as_array_mut() {
                members.push(json!(member));
            }
        }
        None => bindings.push(json!({ "role": role, "members": [member] })),
    }
    policy["bindings"] = Value::Array(bindings);
    Some(policy)
}

pub fn set_policy_body(policy: &Value) -> String {
    json!({ "policy": policy }).to_string()
}

// --- Subscriptions --------------------------------------------------------------------------

/// The push subscription of a build: `purser-<app id>`.
pub fn subscription_id(app_id: &str) -> Result<String, String> {
    let id = format!("purser-{app_id}");
    check_resource_id("subscription", &id)?;
    Ok(id)
}

/// `GET` / `PUT` (create).
pub fn subscription_url(project: &str, subscription: &str) -> String {
    format!("{PUBSUB}/projects/{project}/subscriptions/{subscription}")
}

/// Where a build's subscription pushes, and as whom: Pub/Sub signs every push with an OIDC token
/// of `service_account` for `audience` (`google_oidc::audience`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Push {
    pub endpoint: String,
    pub service_account: String,
    pub audience: String,
}

pub fn push_config(push: &Push) -> Value {
    json!({
        "pushEndpoint": push.endpoint,
        "oidcToken": { "serviceAccountEmail": push.service_account, "audience": push.audience },
    })
}

pub fn create_subscription_body(topic_path: &str, push: &Push) -> String {
    json!({ "topic": topic_path, "pushConfig": push_config(push), "ackDeadlineSeconds": 60 }).to_string()
}

pub fn modify_push_url(project: &str, subscription: &str) -> String {
    format!("{}:modifyPushConfig", subscription_url(project, subscription))
}

pub fn modify_push_body(push: &Push) -> String {
    json!({ "pushConfig": push_config(push) }).to_string()
}

/// `(topic path, push)` of a subscription; the service account and audience are empty when it
/// pushes without authentication.
pub fn parse_subscription(body: &str) -> Result<(String, Push), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Pub/Sub response: {e}"))?;
    let s = |x: &Value| x.as_str().unwrap_or("").to_string();
    let c = &v["pushConfig"];
    let push = Push {
        endpoint: s(&c["pushEndpoint"]),
        service_account: s(&c["oidcToken"]["serviceAccountEmail"]),
        audience: s(&c["oidcToken"]["audience"]),
    };
    Ok((s(&v["topic"]), push))
}

// --- Errors and manual steps ---------------------------------------------------------------------

/// `error.message` of a Google API error body, else a short prefix of the body.
pub fn error_message(status: u16, body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.chars().take(200).collect());
    let hint = if body.contains("disableServiceAccountKeyCreation") {
        " (the organization policy iam.managed.disableServiceAccountKeyCreation forbids keys: an organization admin can \
         turn it off for this project in IAM & Admin > Organization policies)"
    } else if body.contains("allowedPolicyMemberDomains") || body.contains("Domain Restricted Sharing") {
        " (the organization policy iam.allowedPolicyMemberDomains blocks Google Play's notification account: an \
         organization admin can allow it for this project)"
    } else if body.contains("iam.serviceAccounts.actAs") {
        " (the push subscription authenticates as the service account: the token's account needs the Service Account \
         User role (roles/iam.serviceAccountUser) on the project, or the Owner role; a new grant takes a minute or two)"
    } else if status == 401 {
        " (the access token expired: get a new one with `gcloud auth print-access-token`)"
    } else if status == 403 {
        " (the token's account needs the Owner role, or Service Usage Admin, Service Account Admin, Service Account \
         Key Admin, Service Account User and Pub/Sub Admin, on the project)"
    } else {
        ""
    };
    format!("HTTP {status}: {msg}{hint}")
}

/// What only a person can do afterwards.
pub fn manual_steps(email: &str, topic_path: &str) -> Vec<String> {
    vec![
        format!(
            "Play Console > Users and permissions > Invite new users: {email}, with View app information and download \
             bulk reports, View financial data, Manage orders and subscriptions (access can take up to 24 hours)"
        ),
        format!("Play Console > each app > Monetization setup > Real-time developer notifications: topic {topic_path}, then Save"),
        "Press \"Send test notification\" there, then run the setup check".into(),
        "Projects created before 8 April 2021 only: grant roles/iam.serviceAccountTokenCreator on the project to the \
         Pub/Sub service agent service-<PROJECT_NUMBER>@gcp-sa-pubsub.iam.gserviceaccount.com, or Pub/Sub cannot sign \
         its pushes"
            .into(),
    ]
}

/// IAM's answer to a key creation for the setup's service account (a fake key file).
#[cfg(any(test, feature = "testing"))]
pub fn testing_key_answer(project: &str) -> String {
    let file = json!({ "type": "service_account", "client_email": service_account_email(project), "private_key": "-----BEGIN PRIVATE KEY-----\n" });
    json!({ "privateKeyData": base64::engine::general_purpose::STANDARD.encode(file.to_string()) }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_checked() {
        assert!(check_project("my-game-123").is_ok());
        for bad in ["My-game", "abc", "game-", "1game1", "my_game", "game/../x"] {
            assert!(check_project(bad).is_err(), "{bad}");
        }
        assert!(check_topic("play-billing").is_ok());
        assert!(check_topic("goog-x").is_err() && check_topic("a/b").is_err() && check_topic("ab").is_err());
        assert_eq!(subscription_id("com.studio.game").unwrap(), "purser-com.studio.game");
        assert!(subscription_id("com.studio/game").is_err());
        assert!(check_access_token("ya29.a0AfB_byC1234567890abcdef").is_ok());
        assert!(check_access_token("ya29 with spaces in the token").is_err() && check_access_token("").is_err());
    }

    #[test]
    fn urls() {
        assert_eq!(service_account_email("my-game"), "purser-billing@my-game.iam.gserviceaccount.com");
        assert_eq!(keys_url("my-game"), "https://iam.googleapis.com/v1/projects/my-game/serviceAccounts/purser-billing@my-game.iam.gserviceaccount.com/keys");
        assert_eq!(get_policy_url("p-game", "play-billing"), "https://pubsub.googleapis.com/v1/projects/p-game/topics/play-billing:getIamPolicy");
        assert_eq!(modify_push_url("p-game", "purser-a.b"), "https://pubsub.googleapis.com/v1/projects/p-game/subscriptions/purser-a.b:modifyPushConfig");
        assert!(service_enabled(r#"{"name":"x","state":"ENABLED"}"#) && !service_enabled(r#"{"state":"DISABLED"}"#));
        assert!(operation_done(r#"{"name":"operations/1","done":true,"response":{}}"#));
        assert!(!operation_done(r#"{"name":"operations/1"}"#) && !operation_done(r#"{"done":true,"error":{"code":7}}"#));
    }

    #[test]
    fn key_file_must_belong_to_the_account() {
        let email = service_account_email("my-game");
        let body = testing_key_answer("my-game");
        assert!(parse_key(&body, &email).unwrap().contains("\"client_email\":\"purser-billing@my-game.iam.gserviceaccount.com\""));
        assert!(parse_key(&body, "other@my-game.iam.gserviceaccount.com").is_err());
        assert!(parse_key(r#"{"privateKeyData":"%%%"}"#, &email).is_err());
    }

    #[test]
    fn publisher_binding_is_merged() {
        let policy = json!({"etag": "BwX", "bindings": [{"role": "roles/viewer", "members": ["user:a@example.com"]}]});
        let added = with_binding(&policy, PUBLISHER_ROLE, PLAY_PUBLISHER).unwrap();
        assert_eq!(added["etag"], "BwX");
        assert_eq!(added["bindings"].as_array().unwrap().len(), 2);
        assert_eq!(with_binding(&added, PUBLISHER_ROLE, PLAY_PUBLISHER), None);
        let other = json!({"bindings": [{"role": PUBLISHER_ROLE, "members": ["user:a@example.com"]}]});
        let merged = with_binding(&other, PUBLISHER_ROLE, PLAY_PUBLISHER).unwrap();
        assert_eq!(merged["bindings"][0]["members"].as_array().unwrap().len(), 2);
        assert!(with_binding(&json!({"etag": "e"}), PUBLISHER_ROLE, PLAY_PUBLISHER).is_some());
    }

    #[test]
    fn errors_and_subscriptions() {
        let blocked = r#"{"error":{"code":400,"message":"Key creation is not allowed","status":"FAILED_PRECONDITION","details":[{"violations":[{"type":"constraints/iam.disableServiceAccountKeyCreation"}]}]}}"#;
        assert!(error_message(400, blocked).contains("Organization policies"));
        assert!(error_message(401, r#"{"error":{"message":"bad"}}"#).contains("expired"));
        let act_as = r#"{"error":{"message":"Principal initiating the request does not have iam.serviceAccounts.actAs permission on the requested push authentication service account."}}"#;
        assert!(error_message(403, act_as).contains("roles/iam.serviceAccountUser"));
        let (topic, push) = parse_subscription(r#"{"topic":"projects/p/topics/t","pushConfig":{"pushEndpoint":"https://h/x"}}"#).unwrap();
        assert_eq!((topic.as_str(), push.endpoint.as_str(), push.service_account.as_str()), ("projects/p/topics/t", "https://h/x", ""));
        let want = Push { endpoint: "https://h/notify/google".into(), service_account: "sa@p.iam.gserviceaccount.com".into(), audience: "purser-rtdn:com.a".into() };
        let created: Value = serde_json::from_str(&create_subscription_body("projects/p/topics/t", &want)).unwrap();
        assert_eq!(created["pushConfig"]["oidcToken"]["audience"], "purser-rtdn:com.a");
        let answer = json!({ "topic": "projects/p/topics/t", "pushConfig": push_config(&want) }).to_string();
        assert_eq!(parse_subscription(&answer).unwrap().1, want, "what the setup writes reads back the same");
        let modified: Value = serde_json::from_str(&modify_push_body(&want)).unwrap();
        assert_eq!(modified["pushConfig"]["oidcToken"]["serviceAccountEmail"], "sa@p.iam.gserviceaccount.com");
        assert!(manual_steps("sa@p", "projects/p/topics/t").iter().any(|s| s.contains("gcp-sa-pubsub")));
    }
}
