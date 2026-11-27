//! Integrations.

use super::*;

#[test]
fn integration_settings_are_validated() {
    let w = World::new();
    w.as_admin(false);
    let upsert = |v: Value| w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_integration(ctx, v.to_string()));
    assert!(upsert(json!({ "app_id": APP, "kind": "webhook", "settings": { "url": "http://insecure.example" } })).unwrap_err().contains("https"));
    assert!(upsert(json!({ "app_id": APP, "kind": "segment", "settings": {} })).unwrap_err().contains("unknown"));
    assert!(upsert(json!({ "app_id": APP, "kind": "mixpanel", "settings": {} })).unwrap_err().contains("project_id"));
    assert!(upsert(json!({ "app_id": APP, "kind": "meta", "settings": { "dataset_id": "1", "graph_version": "latest" } })).unwrap_err().contains("graph_version"));
    assert!(upsert(json!({ "app_id": APP, "kind": "amplitude", "settings": { "region": "mars" } })).unwrap_err().contains("region"));
    assert!(upsert(json!({ "app_id": APP, "kind": "s3", "settings": { "endpoint": "http://s3.example.com", "bucket": "events" } }))
        .unwrap_err()
        .contains("https"));
    assert!(upsert(json!({ "app_id": APP, "kind": "braze", "settings": { "endpoint": "https://braze.example.com" } })).unwrap_err().contains("REST endpoint"));
    assert!(upsert(json!({ "app_id": APP, "kind": "onesignal", "settings": { "app_id": "x" } })).unwrap_err().contains("UUID"));
    assert!(upsert(json!({ "app_id": APP, "kind": "singular", "settings": { "event_names": { "pause": "p" } } })).unwrap_err().contains("pause"));
    assert_eq!(upsert(json!({ "app_id": APP, "kind": "branch", "settings": {} })), Ok(()));
    assert_eq!(upsert(json!({ "app_id": APP, "kind": "kochava", "settings": {} })), Ok(()));
    assert_eq!(upsert(json!({ "app_id": APP, "kind": "tenjin", "settings": {} })), Ok(()));
    assert_eq!(upsert(json!({ "app_id": APP, "kind": "ga4", "settings": { "firebase_app_id": "1:1:android:x" } })), Ok(()));
    assert!(upsert(json!({ "app_id": APP, "kind": "tenjin", "settings": { "revenue": "profit" } })).unwrap_err().contains("gross, proceeds or net"));
    assert_eq!(upsert(json!({ "app_id": APP, "kind": "tenjin", "settings": { "revenue": "net" } })), Ok(()));
}

#[test]
fn integrations_report_the_revenue_mode_they_are_set_to() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "adjust", json!({ "app_token": "abc123def456", "event_tokens": { "purchase": "pur111" }, "revenue": "proceeds" }));
    w.set_secret(secret_name::ADJUST_S2S_TOKEN, "adjust-token");
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.price_micros = 1_000_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "adjust_id": "2e9c3b6f8a1d4e7c9b0a1f2e3d4c5b6a" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let rows = outbox(&w);
    let queued: Value = serde_json::from_str(&rows[0].event_json).unwrap();
    assert_eq!(queued["commission_percent"], 15.0, "the default commission, stored with the event");
    assert_eq!(queued["price_micros"], 1_000_000, "the queue keeps the gross price");
    crate::integrations::send_integration_event(&mut w.ctx(), rows.into_iter().next().unwrap());
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("s2s.adjust.com")).cloned().expect("Adjust call");
    assert!(call.contains("revenue=0.85"), "proceeds after the 15% commission: {call}");
}

#[test]
fn purchases_are_sent_to_tenjin_with_the_device_ids_from_the_client() {
    use crate::integrations::integration_outbox;
    let w = World::new();
    w.as_admin(false);
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_upsert_integration(ctx, json!({ "app_id": APP, "kind": "tenjin", "settings": {} }).to_string())
        })
        .unwrap();
    w.set_secret(secret_name::TENJIN_SDK_KEY, "tenjin-key");
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "tenjin_id": "b1c5d7c4-1c4f-4d6e-9e2a-3a2f1f0a9b10", "os_version": "14", "advertising_id": "gaid-1" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");

    let rows: Vec<_> = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.integration_outbox().iter().collect::<Vec<_>>()))
        .unwrap();
    assert_eq!(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), ["tenjin"]);
    w.script("track.tenjin.com/v0/purchase", 200, r#"{"code":200}"#);
    crate::integrations::send_integration_event(&mut w.ctx(), rows.into_iter().next().unwrap());
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("tenjin")).cloned().expect("Tenjin call");
    assert!(call.starts_with("POST https://track.tenjin.com/v0/purchase"), "{call}");
}

#[test]
fn first_purchases_the_game_reports_itself_are_not_sent_twice() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "tenjin", json!({ "skip_client_purchases": true }));
    w.set_secret(secret_name::TENJIN_SDK_KEY, "tenjin-key");
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "tenjin_id": "b1c5d7c4-1c4f-4d6e-9e2a-3a2f1f0a9b10" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    for row in outbox(&w) {
        crate::integrations::send_integration_event(&mut w.ctx(), row);
    }
    assert_eq!(w.calls("tenjin.com"), 0, "the game's Tenjin SDK reported the purchase");

    // Unknown kinds are refused when the integration is saved.
    let bad = w.test.with_reducer_tx(TestAuth::internal(), |ctx| {
        crate::admin_upsert_integration(ctx, json!({ "app_id": APP, "kind": "tenjin", "settings": { "events": ["bought"] } }).to_string())
    });
    assert!(bad.unwrap_err().contains("unknown event kind"));
}

#[test]
fn child_directed_apps_keep_no_advertising_ids() {
    let w = World::new();
    let game_key = w.read(|tx| tx.db.game_app().app_id().find(APP.to_string()).unwrap().game_key);
    w.as_admin(true);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_set_app_policy(ctx, game_key.clone(), true)).unwrap();
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "tenjin_id": "b1c5d7c4-1c4f-4d6e-9e2a-3a2f1f0a9b10", "advertising_id": "gaid-1", "idfa": "idfa-1" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let ids = w.read(|tx| crate::integrations::load_attribution(tx, APP, "player-1"));
    assert_eq!(ids.keys().map(String::as_str).collect::<Vec<_>>(), ["tenjin_id"]);

    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_set_app_policy(ctx, game_key.clone(), false)).unwrap();
    assert!(w.read(|tx| tx.db.app_policy().game_key().find(&game_key)).is_none());
    assert!(w
        .test
        .with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_set_app_policy(ctx, "no-such-app".into(), true))
        .unwrap_err()
        .contains("ERR_UNKNOWN_APP"));
}

pub(super) fn outbox(w: &World) -> Vec<crate::integrations::IntegrationOutbox> {
    use crate::integrations::integration_outbox;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.integration_outbox().iter().collect::<Vec<_>>()))
        .unwrap()
}

pub(super) fn connect(w: &World, kind: &str, settings: Value) {
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_upsert_integration(ctx, json!({ "app_id": APP, "kind": kind, "settings": settings }).to_string())
        })
        .unwrap();
}

#[test]
fn test_event_is_exported_to_cloud_storage_with_its_own_token() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "gcs", json!({ "bucket": "my-billing-events", "folder": "billing" }));
    w.set_secret(secret_name::GCS_SERVICE_ACCOUNT_JSON, SA_JSON);
    w.write(|tx| {
        tx.db.access_token_cache().insert(AccessTokenCache {
            key: "gcs/default".into(),
            token: "gcs-token".into(),
            expires_at_micros: i64::MAX,
        });
    });
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_test_integration(ctx, json!({ "app_id": APP }).to_string()))
        .unwrap();
    let rows = outbox(&w);
    assert_eq!(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), ["gcs"]);
    crate::integrations::send_integration_event(&mut w.ctx(), rows.into_iter().next().unwrap());
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("storage.googleapis.com")).cloned().expect("upload");
    assert!(
        call.starts_with("POST https://storage.googleapis.com/upload/storage/v1/b/my-billing-events/o?uploadType=media&name=billing%2Fdt%3D"),
        "{call}"
    );
    assert!(!w.fake.borrow().calls.iter().any(|c| c.contains("oauth2")), "the cached token is reused");
}

#[test]
fn purchases_are_sent_to_adjust_with_the_event_token_of_their_kind() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "adjust", json!({ "app_token": "abc123def456", "event_tokens": { "purchase": "pur111" } }));
    w.set_secret(secret_name::ADJUST_S2S_TOKEN, "adjust-token");
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "adjust_id": "2e9c3b6f8a1d4e7c9b0a1f2e3d4c5b6a" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let rows = outbox(&w);
    assert_eq!(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), ["adjust"]);
    crate::integrations::send_integration_event(&mut w.ctx(), rows.into_iter().next().unwrap());
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("s2s.adjust.com")).cloned().expect("Adjust call");
    assert!(call.starts_with("POST https://s2s.adjust.com/event"), "{call}");
}

/// A verified purchase of `gems` (0.99 USD) by `player-1`, with the device ids its client sent; the
/// one integration connected gets it in the outbox and sends it. Returns (url, auth header, body).
pub(super) fn send_gems_purchase(w: &World, kind: &str, host: &str, attribution: Value) -> (String, String, Value) {
    let (method, url, auth, body) = send_gems_purchase_as(w, kind, host, attribution, "{}");
    assert_eq!(method, "POST");
    (url, auth, serde_json::from_str(&body).unwrap())
}

/// [`send_gems_purchase`] with the integration answering `answer`: (method, url, auth header, raw body).
pub(super) fn send_gems_purchase_as(w: &World, kind: &str, host: &str, attribution: Value, answer: &str) -> (String, String, String, String) {
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN, "attribution": attribution,
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let rows = outbox(w);
    assert_eq!(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), [kind]);
    w.script(host, 200, answer);
    crate::integrations::send_integration_event(&mut w.ctx(), rows.into_iter().next().unwrap());
    let call = w.fake.borrow().calls.iter().find(|c| c.contains(host)).cloned().unwrap_or_else(|| panic!("{kind} call"));
    let (head, body) = call.split_once("] ").expect("auth and body recorded");
    let (request, auth) = head.split_once(" [auth=").unwrap();
    let (method, url) = request.split_once(' ').unwrap();
    (method.to_string(), url.to_string(), auth.to_string(), body.to_string())
}

#[test]
fn purchases_are_exported_to_an_s3_bucket_with_a_signed_put() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "s3", json!({ "endpoint": "https://s3.example.com", "region": "eu-west-1", "bucket": "billing-events", "prefix": "billing" }));
    w.set_secret(secret_name::S3_ACCESS_KEY, "AKIDEXAMPLE:s3-secret/key+1");
    let (method, url, auth, body) = send_gems_purchase_as(&w, "s3", "s3.example.com", json!({ "advertising_id": "gaid-1" }), "");
    assert_eq!(method, "PUT");
    assert!(url.starts_with("https://s3.example.com/billing-events/billing/dt%3D"), "path style: {url}");
    assert!(url.ends_with(&format!("-purchase-{ORDER}.json")), "{url}");
    let date = purser_core::sigv4::amz_date(NOW_MICROS / 1000);
    assert!(
        auth.starts_with(&format!("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/{}/eu-west-1/s3/aws4_request, ", &date[..8])),
        "{auth}"
    );
    assert!(!auth.contains("s3-secret") && !url.contains("s3-secret") && !body.contains("s3-secret"));
    let ev: Value = serde_json::from_str(body.trim_end()).unwrap();
    assert!(body.ends_with('\n'), "one NDJSON line");
    assert_eq!((ev["gross_micros"].as_i64(), ev["transaction_id"].as_str()), (Some(990_000), Some(ORDER)));
    assert_eq!(ev["attribution"]["advertising_id"], "gaid-1");
    assert_eq!(w.doctor(false).unwrap()["integration:s3"].0, "ok");

    // A key without its secret half is refused before anything is sent.
    w.set_secret(secret_name::S3_ACCESS_KEY, "AKIDEXAMPLE");
    let doctor = w.doctor(false).unwrap();
    assert_eq!(doctor["integration:s3"].0, "fail");
    assert!(!doctor["integration:s3"].1.contains("AKIDEXAMPLE"));
}

#[test]
fn purchases_are_sent_to_braze_for_the_external_id() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "braze", json!({ "endpoint": "https://rest.iad-01.braze.com" }));
    w.set_secret(secret_name::BRAZE_REST_API_KEY, "braze-key-1");
    let (url, auth, body) = send_gems_purchase(&w, "braze", "braze.com", json!({}));
    assert_eq!(url, "https://rest.iad-01.braze.com/users/track");
    assert_eq!(auth, "Bearer braze-key-1");
    let p = &body["purchases"][0];
    assert_eq!((p["external_id"].as_str(), p["product_id"].as_str()), (Some("player-1"), Some("gems")));
    assert_eq!((p["price"].as_f64(), p["currency"].as_str(), p["quantity"].as_u64()), (Some(0.99), Some("USD"), Some(1)));
    assert_eq!(p["properties"]["transaction_id"], ORDER);
    assert_eq!(w.doctor(false).unwrap()["integration:braze"].0, "ok");
}

#[test]
fn events_onesignal_refuses_in_a_202_answer_are_dropped() {
    use crate::integrations::integration_dead;
    let w = World::new();
    w.as_admin(false);
    connect(&w, "onesignal", json!({ "app_id": "8250eaf6-1a4b-4de3-9e5a-2b4d63d1e7ab" }));
    w.set_secret(secret_name::ONESIGNAL_API_KEY, "os-key-1");
    let refused = r#"{"errors":[{"event_user_id":"player-1","event_id":"x","error":{"code":"too many event names","title":"limit reached"}}]}"#;
    let (_, url, auth, body) = send_gems_purchase_as(&w, "onesignal", "api.onesignal.com", json!({}), refused);
    assert_eq!(url, "https://api.onesignal.com/apps/8250eaf6-1a4b-4de3-9e5a-2b4d63d1e7ab/custom_events");
    assert_eq!(auth, "Key os-key-1");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((body["events"][0]["name"].as_str(), body["events"][0]["external_id"].as_str()), (Some("purser_purchase"), Some("player-1")));
    let dead: Vec<(String, String)> = w.read(|tx| tx.db.integration_dead().iter().map(|d| (d.kind, d.last_error)).collect());
    assert_eq!(dead.len(), 1, "a 202 that names the event as refused is a failure, not a delivery");
    assert!(dead[0].1.contains("too many event names"), "{dead:?}");
    // The test leaves the attempt's own row in place (the host deletes it before the call): no second attempt was queued.
    assert!(outbox(&w).iter().all(|r| r.attempts == 0), "not retried");
}

#[test]
fn purchases_reach_ga4_in_the_players_session_and_with_their_user_id() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "ga4", json!({ "firebase_app_id": "1:1:android:abc", "send_user_id": true }));
    w.set_secret(secret_name::GA4_API_SECRET, "ga4-secret");
    let (url, _, body) = send_gems_purchase(&w, "ga4", "google-analytics.com", json!({ "ga_session_id": "1795810339" }));
    assert!(url.starts_with("https://www.google-analytics.com/mp/collect?"), "{url}");
    assert_eq!(body["user_id"], "player-1");
    assert_eq!(body["events"][0]["params"]["session_id"], 1795810339i64);

    // An hour later (a refund, a renewal) the session is over: the event goes without it.
    w.advance_clock(3600);
    let p = w.purchase();
    let ev = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| {
            let app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
            Ok::<_, ()>(crate::integrations::out_event(tx, &app, &p, "refund"))
        })
        .unwrap();
    assert_eq!(ev.attribution.get("ga_session_id").map(String::as_str), Some("1795810339"));
    assert_eq!(ev.session("ga_session_id"), None);
}

#[test]
fn purchases_are_sent_to_amplitude_with_the_key_in_the_body() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "amplitude", json!({}));
    w.set_secret(secret_name::AMPLITUDE_API_KEY, "amp-key-123");
    let (url, auth, body) = send_gems_purchase(&w, "amplitude", "api2.amplitude.com", json!({ "amplitude_device_id": "device-42" }));
    assert_eq!(url, "https://api2.amplitude.com/2/httpapi");
    assert!(auth.is_empty() && !url.contains("amp-key-123"));
    assert_eq!(body["api_key"], "amp-key-123");
    let e = &body["events"][0];
    assert_eq!((e["event_type"].as_str(), e["user_id"].as_str(), e["device_id"].as_str()), (Some("purser_purchase"), Some("player-1"), Some("device-42")));
    assert_eq!((e["revenue"].as_f64(), e["currency"].as_str(), e["productId"].as_str()), (Some(0.99), Some("USD"), Some("gems")));
    assert_eq!(e["insert_id"], format!("{LEDGER_ID}#0:purchase"), "the event id: the same on retries, another for every other event");
    assert_eq!(e["time"].as_i64(), Some(NOW_MICROS / 1000));
}

#[test]
fn purchases_are_sent_to_mixpanel_with_the_service_account() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "mixpanel", json!({ "project_id": "3456789", "region": "eu" }));
    w.set_secret(secret_name::MIXPANEL_SERVICE_ACCOUNT, "sa-user.mp-service-account:sa-secret");
    let (url, auth, body) = send_gems_purchase(&w, "mixpanel", "mixpanel.com", json!({}));
    assert_eq!(url, "https://api-eu.mixpanel.com/import?strict=1&project_id=3456789");
    assert_eq!(auth, purser_core::encoding::basic_auth("sa-user.mp-service-account", "sa-secret"));
    assert!(!url.contains("sa-secret"));
    let p = &body[0]["properties"];
    assert_eq!((body[0]["event"].as_str(), p["distinct_id"].as_str()), (Some("purser_purchase"), Some("player-1")));
    assert_eq!((p["revenue"].as_f64(), p["currency"].as_str(), p["transaction_id"].as_str()), (Some(0.99), Some("USD"), Some(ORDER)));
    assert_eq!(p["$insert_id"].as_str().map(str::len), Some(32));
}

#[test]
fn amplitude_and_mixpanel_get_the_ids_of_their_own_sdks() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "mixpanel", json!({ "project_id": "3456789" }));
    w.set_secret(secret_name::MIXPANEL_SERVICE_ACCOUNT, "sa:secret");
    let (_, _, body) = send_gems_purchase(&w, "mixpanel", "mixpanel.com", json!({ "mixpanel_distinct_id": "$device:3F2A-11", "amplitude_session_id": "1795810339000" }));
    assert_eq!((body[0]["properties"]["distinct_id"].as_str(), body[0]["properties"]["purser_user_id"].as_str()), (Some("$device:3F2A-11"), Some("player-1")));

    let w = World::new();
    w.as_admin(false);
    connect(&w, "amplitude", json!({}));
    w.set_secret(secret_name::AMPLITUDE_API_KEY, "amp-key");
    let (_, _, body) = send_gems_purchase(&w, "amplitude", "api2.amplitude.com", json!({ "amplitude_session_id": "1795810339000" }));
    assert_eq!(body["events"][0]["session_id"], 1795810339000i64);
}

#[test]
fn purchases_are_sent_to_meta_with_the_token_in_the_header() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "meta", json!({ "dataset_id": "123456789012345" }));
    w.set_secret(secret_name::META_ACCESS_TOKEN, "EAAmeta-token");
    let ids = json!({ "advertising_id": "gaid-1", "meta_anon_id": "XZanon-1", "os_version": "14", "app_version": "1.2.3" });
    let (url, auth, body) = send_gems_purchase(&w, "meta", "graph.facebook.com", ids);
    assert_eq!(url, "https://graph.facebook.com/v23.0/123456789012345/events");
    assert_eq!(auth, "Bearer EAAmeta-token");
    assert!(!url.contains("EAAmeta-token") && !body.to_string().contains("EAAmeta-token"));
    let e = &body["data"][0];
    assert_eq!((e["event_name"].as_str(), e["action_source"].as_str()), (Some("Purchase"), Some("app")));
    assert_eq!((e["user_data"]["madid"].as_str(), e["user_data"]["anon_id"].as_str()), (Some("gaid-1"), Some("XZanon-1")));
    assert_eq!((e["custom_data"]["value"].as_f64(), e["custom_data"]["currency"].as_str()), (Some(0.99), Some("USD")));
    assert_eq!(e["app_data"]["extinfo"].as_array().map(|a| a.len()), Some(16));
    assert_eq!(e["app_data"]["extinfo"][1], PACKAGE, "the package, not the build key");
    assert_eq!(e["event_time"].as_i64(), Some(NOW_MICROS / 1_000_000));
    assert_eq!(w.doctor(false).unwrap()["integration:meta"].0, "ok");
}

#[test]
fn integration_health_shows_sent_skipped_and_failed_events_in_the_setup_check() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "adjust", json!({ "app_token": "abc123def456", "event_tokens": { "purchase": "pur111" } }));
    w.set_secret(secret_name::ADJUST_S2S_TOKEN, "adjust-token");
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "adjust_id": "2e9c3b6f8a1d4e7c9b0a1f2e3d4c5b6a" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let row = outbox(&w).into_iter().next().unwrap();
    let health = |w: &World| {
        w.test
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(crate::integrations::health_of(tx, APP)))
            .unwrap()
            .into_iter()
            .next()
    };
    let check = |w: &World| w.doctor(false).unwrap()["integration:adjust"].clone();
    // The fake answers with the first matching script: replace it.
    let adjust_answers = |w: &World, status: u16, body: &str| {
        w.fake.borrow_mut().overrides.retain(|(needle, _, _)| needle != "s2s.adjust.com");
        w.script("s2s.adjust.com", status, body);
    };
    assert!(check(&w).1.contains("event token"), "nothing sent yet: the configuration: {:?}", check(&w));

    adjust_answers(&w, 200, "{}");
    crate::integrations::send_integration_event(&mut w.ctx(), row.clone());
    let h = health(&w).expect("a health row");
    assert!(h.last_sent_at_micros > 0 && h.last_failed_at_micros == 0);
    assert_eq!(check(&w).0, "ok");
    assert!(check(&w).1.starts_with("Last event sent"), "{:?}", check(&w));

    // The next player's client did not send a device id: nothing reaches Adjust.
    w.advance_clock(120);
    let mut no_id = row.clone();
    let mut ev: Value = serde_json::from_str(&no_id.event_json).unwrap();
    ev["attribution"] = json!({});
    no_id.event_json = ev.to_string();
    crate::integrations::send_integration_event(&mut w.ctx(), no_id);
    assert!(health(&w).unwrap().last_skip_reason.contains("adjust_id"));
    assert_eq!(check(&w).0, "warn");
    assert!(check(&w).1.starts_with("Events are skipped"), "{:?}", check(&w));

    // A temporary failure is retried; a rejected key drops the event.
    w.advance_clock(120);
    adjust_answers(&w, 503, "busy");
    crate::integrations::send_integration_event(&mut w.ctx(), row.clone());
    let h = health(&w).unwrap();
    assert!(!h.last_failure_final && h.last_error.contains("503"), "{}", h.last_error);
    assert_eq!(check(&w).0, "warn");
    w.advance_clock(120);
    adjust_answers(&w, 401, "bad token");
    crate::integrations::send_integration_event(&mut w.ctx(), row.clone());
    assert!(health(&w).unwrap().last_failure_final);
    assert_eq!(check(&w).0, "fail");
    assert!(check(&w).1.starts_with("The last event was dropped"), "{:?}", check(&w));
    // The dropped event is kept for a replay.
    let dead = |w: &World| {
        use crate::integrations::integration_dead;
        w.read(|tx| tx.db.integration_dead().iter().map(|d| (d.kind, d.purchase_id, d.last_error)).collect::<Vec<_>>())
    };
    assert_eq!(dead(&w).len(), 1);
    assert!(dead(&w)[0].2.contains("401"), "{:?}", dead(&w));

    // Working again; an admin sends the dropped event again.
    w.advance_clock(120);
    adjust_answers(&w, 200, "{}");
    crate::integrations::send_integration_event(&mut w.ctx(), row);
    assert_eq!(check(&w).0, "ok");
    w.as_admin(false);
    let before = outbox(&w).len();
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| crate::integrations::admin_replay_integration_events(ctx, APP.into(), "adjust".into()))
        .unwrap();
    assert!(dead(&w).is_empty());
    assert_eq!(outbox(&w).len(), before + 1);
}

#[test]
fn trial_events_carry_the_trial_and_take_the_names_of_the_game() {
    use super::revenue_reports_tests::{add_subscription_product, keep_google_token_valid, subscription_period};
    use purser_core::integrations::OutEvent;
    use crate::integrations::integration_outbox;
    let w = World::new();
    w.as_admin(false);
    keep_google_token_valid(&w, 60);
    add_subscription_product(&w);
    connect(&w, "amplitude", json!({ "event_names": { "trial_start": "trial_started", "trial_converted": "trial_converted", "renewal": "subscription_renewed" } }));
    w.set_secret(secret_name::AMPLITUDE_API_KEY, "amp-key");
    w.script("api2.amplitude.com", 200, r#"{"code":200}"#);
    let send_all = |w: &World| -> Vec<OutEvent> {
        // The scheduler takes a row out of the table when it runs it.
        let rows: Vec<_> = w
            .test
            .with_reducer_tx(TestAuth::internal(), |tx| {
                let rows: Vec<_> = tx.db.integration_outbox().iter().collect();
                for r in &rows {
                    tx.db.integration_outbox().id().delete(r.id);
                }
                Ok::<_, ()>(rows)
            })
            .unwrap();
        let events = rows.iter().map(|r| serde_json::from_str::<OutEvent>(&r.event_json).unwrap()).collect();
        for r in rows {
            crate::integrations::send_integration_event(&mut w.ctx(), r);
        }
        events
    };
    let sent_names = |w: &World| -> Vec<String> {
        w.fake.borrow().calls.iter().filter(|c| c.contains("amplitude")).map(|c| c.split("\"event_type\":\"").nth(1).unwrap().split('"').next().unwrap().to_string()).collect()
    };

    // A 7-day free trial.
    subscription_period(&w, "GPA.7777-0000-0000-00002", "freeTrial", "2026-09-28T00:00:00Z");
    assert_eq!(w.verify("player-1", "vip").body["status"], "success");
    let events = send_all(&w);
    assert_eq!(events.iter().map(|e| (e.kind.as_str(), e.trial, e.trial_conversion)).collect::<Vec<_>>(), [("subscription_start", true, false)]);
    // The first paid period, then a renewal.
    w.advance_clock(7 * 86_400);
    subscription_period(&w, "GPA.7777-0000-0000-00002..0", "basePrice", "2026-10-28T00:00:00Z");
    assert_eq!(w.verify("player-1", "vip").body["status"], "success");
    let events = send_all(&w);
    assert_eq!(events.iter().map(|e| (e.kind.as_str(), e.trial, e.trial_conversion)).collect::<Vec<_>>(), [("renewal", false, true)]);
    w.advance_clock(30 * 86_400);
    subscription_period(&w, "GPA.7777-0000-0000-00002..1", "basePrice", "2026-11-28T00:00:00Z");
    assert_eq!(w.verify("player-1", "vip").body["status"], "success");
    send_all(&w);
    assert_eq!(sent_names(&w), ["trial_started", "trial_converted", "subscription_renewed"]);
}
