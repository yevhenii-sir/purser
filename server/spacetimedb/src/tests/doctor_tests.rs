//! Setup check ("doctor") and app health.

use super::*;

pub(super) const SA_EMAIL: &str = "billing@proj.iam.gserviceaccount.com";
pub(super) const SA_JSON: &str = r#"{"client_email":"billing@proj.iam.gserviceaccount.com","private_key":"unused: a token is cached"}"#;
pub(super) const GAME_SCHEMA: &str = r#"{"reducers":[
    {"name":"billing_grant","params":{"elements":[{"name":{"some":"ev"},"algebraic_type":{"Ref":0}}]}},
    {"name":"billing_revoke","params":{"elements":[{"name":{"some":"ev"},"algebraic_type":{"Ref":0}}]}}]}"#;

pub(super) fn billing_token() -> String {
    let claims = purser_core::jwt::b64url(br#"{"hex_identity":"c200aa","sub":"billing","exp":null}"#);
    format!("header.{claims}.signature")
}

pub(super) fn level<'a>(checks: &'a std::collections::BTreeMap<String, (String, String)>, id: &str) -> &'a str {
    checks.get(id).map(|(l, _)| l.as_str()).unwrap_or_else(|| panic!("no check {id}: {checks:?}"))
}

#[test]
fn doctor_requires_an_admin() {
    let w = World::new();
    let e = w.doctor(false).unwrap_err();
    assert!(e.contains("ERR_ADMIN_UNAUTHORIZED"), "{e}");
}

#[test]
fn doctor_offline_reports_configuration_and_secret_scopes() {
    let w = World::new();
    w.as_admin(false);
    let c = w.doctor(false).unwrap();

    assert_eq!(level(&c, "app"), "ok");
    assert_eq!(level(&c, "products"), "warn", "Android products without a default price");
    assert_eq!(c["secret:google_service_account_json"], ("ok".into(), "set (scope default)".into()));
    assert_eq!(c["google_key"], ("ok".into(), SA_EMAIL.into()));
    assert_eq!(level(&c, "notifications"), "warn", "no RTDN yet");
    assert_eq!(level(&c, "delivery_identity"), "fail", "the seeded token is not a SpacetimeDB token");
    assert_eq!(level(&c, "delivery_game"), "skip");
    assert!(w.fake.borrow().calls.is_empty(), "offline: no HTTP");
    let check = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| {
            use crate::health::app_check;
            Ok::<_, ()>(tx.db.app_check().app_id().find(APP.to_string()))
        })
        .unwrap()
        .expect("report saved");
    assert_eq!(check.summary, "fail");
    assert!(check.report_json.contains("\"summary\":\"fail\""));
}

#[test]
fn doctor_flags_missing_play_permissions_with_the_account_to_invite() {
    let w = World::new();
    w.as_admin(false);
    w.set_secret(secret_name::GOOGLE_SERVICE_ACCOUNT_JSON, SA_JSON);
    w.script("voidedpurchases", 403, r#"{"error":{"code":403}}"#);

    let c = w.doctor(true).unwrap();
    assert_eq!(c["google_key"].1, "billing@proj.iam.gserviceaccount.com");
    assert_eq!(level(&c, "google_api"), "fail");

    w.fake.borrow_mut().overrides.clear();
    let c = w.doctor(true).unwrap();
    assert_eq!(level(&c, "google_api"), "ok");
}

#[test]
fn doctor_checks_the_game_reducers_without_calling_them() {
    let w = World::new();
    w.as_admin(false);
    w.set_secret(secret_name::SPACETIME_TOKEN, &billing_token());
    w.script("/v1/identity/c200aa/verify", 204, "");
    w.script("/v1/database/game-db/schema", 200, GAME_SCHEMA);

    let c = w.doctor(true).unwrap();
    assert_eq!(level(&c, "delivery_host"), "ok");
    assert_eq!(level(&c, "delivery_identity"), "ok");
    assert_eq!(level(&c, "delivery_token"), "ok");
    assert_eq!(level(&c, "delivery_game"), "ok");
    assert_eq!(w.calls("/call/"), 0, "the doctor never calls game reducers");

    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.grant_reducer = "fulfill".into();
        tx.db.game_app().app_id().update(app);
    });
    let c = w.doctor(true).unwrap();
    assert_eq!(level(&c, "delivery_game"), "fail");
    assert!(c["delivery_game"].1.contains("fulfill"), "{:?}", c["delivery_game"]);
}

#[test]
fn doctor_does_not_send_the_token_to_a_host_that_is_not_allowed() {
    let w = World::new();
    w.as_admin(false);
    w.set_secret(secret_name::SPACETIME_TOKEN, &billing_token());
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.spacetime_url = "https://evil.example.net".into();
        tx.db.game_app().app_id().update(app);
    });

    let c = w.doctor(true).unwrap();
    assert_eq!(level(&c, "delivery_host"), "fail");
    assert!(!c.contains_key("delivery_token"));
    assert_eq!(w.calls("/identity/"), 0);
}

#[test]
fn rtdn_push_tokens_must_name_the_build_audience_and_service_account() {
    let w = World::new();
    let test = json!({ "packageName": APP, "testNotification": { "version": "1.0" } });
    let push = |change: fn(&mut Value)| w.google_push(&format!("Bearer {}", w.push_token(change)), test.clone());

    let o = push(|c| c["aud"] = json!("purser-rtdn:com.other.game"));
    assert_eq!(o.http, 403);
    let h = w.notify_health();
    assert!(h.last_rejected_reason.contains("service account and audience") && h.last_rejected_reason.contains("audience"), "{}", h.last_rejected_reason);
    w.advance_clock(61);
    assert_eq!(push(|c| c["email"] = json!("someone@evil.example")).http, 403);
    assert!(w.notify_health().last_rejected_reason.contains(SA_EMAIL));
    assert_eq!(push(|c| c["email_verified"] = json!(false)).http, 403);
    assert_eq!(push(|c| c["exp"] = json!(1)).http, 403);
    assert_eq!(w.google_push("", test.clone()).http, 401, "no token: authentication is off in the subscription");
    assert_eq!(w.google_push("Bearer not-a-jwt", test.clone()).http, 403);
    assert_eq!(w.notify_health().last_at_micros, 0, "nothing accepted");

    let o = w.google_notification(test.clone());
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("ok")), "{}", o.body);
    assert!(w.notify_health().last_test_at_micros > 0);

    // The build's service account names the tokens: without its key nothing is accepted.
    w.write(|tx| {
        tx.db.credential().key().delete(config::credential_key(config::DEFAULT_SCOPE, secret_name::GOOGLE_SERVICE_ACCOUNT_JSON));
    });
    w.advance_clock(61);
    assert_eq!(w.google_notification(test).http, 403);
    assert!(w.notify_health().last_rejected_reason.contains("google_service_account_json"));
}

#[test]
fn google_keys_are_cached_and_fetched_for_an_unknown_key_at_most_once_a_minute() {
    use purser_core::google_oidc::{audience, testing};
    let w = World::new();
    let test = json!({ "packageName": APP, "testNotification": { "version": "1.0" } });
    let certs = || w.calls(purser_core::google_oidc::CERTS_URL);
    assert_eq!(w.google_notification(test.clone()).http, 200);
    assert_eq!(w.google_notification(test.clone()).http, 200);
    assert_eq!(certs(), 1, "cached");

    w.advance_clock(61);
    let now = || w.read(|tx| tx.timestamp.to_micros_since_unix_epoch()) / 1_000_000;
    let unknown = |kid: &str| format!("Bearer {}", testing::token(kid, &testing::claims(&audience(APP), SA_EMAIL, now())));
    assert_eq!(w.google_push(&unknown("made-up-1"), test.clone()).http, 403);
    assert_eq!(certs(), 2, "an unknown key is looked up");
    assert_eq!(w.google_push(&unknown("made-up-2"), test.clone()).http, 403);
    assert_eq!(certs(), 2, "at most once a minute");
    assert_eq!(w.google_notification(test.clone()).http, 200, "known keys still work");
    w.advance_clock(30);
    assert_eq!(w.google_push(&unknown("made-up-3"), test.clone()).http, 403);
    assert_eq!(certs(), 2);
    w.advance_clock(31);
    assert_eq!(w.google_push(&unknown("made-up-3"), test.clone()).http, 403);
    assert_eq!(certs(), 3);

    // Keys a day old are fetched again.
    w.advance_clock(24 * 3600);
    assert_eq!(w.google_notification(test).http, 200);
    assert_eq!(certs(), 4);
}

#[test]
fn a_failed_fetch_of_google_keys_is_retried_by_pub_sub() {
    let w = World::new();
    let test = json!({ "packageName": APP, "testNotification": { "version": "1.0" } });
    w.script(purser_core::google_oidc::CERTS_URL, 500, "backend error");
    assert_eq!(w.google_notification(test.clone()).http, 503);
    w.fake.borrow_mut().overrides.clear();
    assert_eq!(w.google_notification(test.clone()).http, 503, "no keys, and the last fetch was a moment ago");
    assert_eq!(w.calls(purser_core::google_oidc::CERTS_URL), 1);
    assert_eq!(w.notify_health().last_rejected_at_micros, 0, "our failure, not the store's");
    w.advance_clock(61);
    assert_eq!(w.google_notification(test).http, 200);
}

#[test]
fn rtdn_without_a_token_and_test_notifications_show_up_in_the_setup_check() {
    let w = World::new();
    w.as_admin(false);
    let test = json!({ "packageName": APP, "testNotification": { "version": "1.0" } });

    assert_eq!(w.google_push("", test.clone()).http, 401);
    let h = w.notify_health();
    assert!(h.last_rejected_at_micros > 0 && h.last_at_micros == 0);
    assert_eq!(level(&w.doctor(false).unwrap(), "notifications"), "fail");

    w.advance_clock(5);
    assert_eq!(w.google_notification(test).http, 200);
    let h = w.notify_health();
    assert!(h.last_test_at_micros > h.last_rejected_at_micros);
    let c = w.doctor(false).unwrap();
    assert_eq!(level(&c, "notifications"), "ok");
    assert!(c["notifications"].1.starts_with("Test notification received"));
}

#[test]
fn verify_records_client_activity() {
    let w = World::new();
    w.verify("player-1", "gems");
    let h = w.health();
    assert_eq!((h.last_verify_at_micros, h.last_verify_ok_at_micros), (NOW_MICROS, NOW_MICROS));
}

#[test]
fn store_catalog_is_listed_and_compared() {
    let w = World::new();
    w.as_admin(false);
    w.script(
        "/oneTimeProducts?pageSize=100&pageToken=p2",
        200,
        r#"{"oneTimeProducts":[{"productId":"no_ads","purchaseOptions":[{"regionalPricingAndAvailabilityConfigs":[
            {"regionCode":"US","price":{"currencyCode":"USD","units":"2","nanos":990000000}}]}]}]}"#,
    );
    w.script(
        "/oneTimeProducts?pageSize=100",
        200,
        r#"{"oneTimeProducts":[{"productId":"gems"},{"productId":"coins"}],"nextPageToken":"p2"}"#,
    );
    w.script(
        "/subscriptions?pageSize=100",
        200,
        r#"{"subscriptions":[{"productId":"vip","basePlans":[{"autoRenewingBasePlanType":{"billingPeriodDuration":"P1M"},
            "regionalConfigs":[{"regionCode":"US","price":{"currencyCode":"USD","units":"4","nanos":990000000}}]}]}]}"#,
    );

    let v = crate::catalog::list_store_products(&mut w.ctx(), APP).unwrap();
    let status = |id: &str| {
        let row = v["products"].as_array().unwrap().iter().find(|p| p["product_id"] == id).unwrap().clone();
        (row["status"].as_str().unwrap().to_string(), row["kind"].as_str().unwrap().to_string())
    };
    assert_eq!(status("gems"), ("same".into(), "consumable".into()));
    assert_eq!(status("coins"), ("new".into(), "consumable".into()));
    assert_eq!(status("no_ads"), ("changed".into(), "non_consumable".into()), "price differs, configured kind kept");
    assert_eq!(status("vip"), ("new".into(), "subscription".into()));
    assert_eq!(v["missing_in_store"], json!([]));
    assert_eq!(w.calls("/oneTimeProducts"), 2, "both pages");
}

#[test]
fn store_catalog_needs_an_admin() {
    let w = World::new();
    assert!(crate::catalog::list_store_products(&mut w.ctx(), APP).unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
}

#[test]
fn app_store_catalog_is_listed_and_compared() {
    let w = apple_offer_world(true);
    w.as_admin(false);
    let list = |w: &World| crate::catalog::list_store_products(&mut w.ctx(), IOS_APP);
    assert!(list(&w).unwrap_err().contains("Team key"), "the In-App Purchase key is not enough");

    w.set_secret(secret_name::APPLE_CONNECT_KEY_ID, "TEAMKEY123");
    w.set_secret(secret_name::APPLE_CONNECT_PRIVATE_KEY_P8, &test_p8(9));
    let next = "https://api.appstoreconnect.apple.com/v1/apps/1234567890/inAppPurchasesV2?cursor=Mg&limit=200";
    w.script("inAppPurchasesV2?cursor=Mg", 200, r#"{"data":[
        {"type":"inAppPurchases","id":"2","attributes":{"productId":"no_ads","inAppPurchaseType":"NON_CONSUMABLE"}}],"links":{}}"#);
    w.script(
        "inAppPurchasesV2?limit=200",
        200,
        &json!({ "data": [{ "type": "inAppPurchases", "id": "1",
                            "attributes": { "productId": "gems", "inAppPurchaseType": "CONSUMABLE" } }],
                 "links": { "next": next } })
        .to_string(),
    );
    w.script("/v1/apps?filter%5BbundleId%5D=com.studio.game.ios", 200, &json!({ "data": [
        { "type": "apps", "id": "1234567890", "attributes": { "bundleId": IOS_APP } }] }).to_string());
    w.script("/subscriptionGroups?", 200, r#"{"data":[{"type":"subscriptionGroups","id":"g"}],"included":[
        {"type":"subscriptions","id":"s1","attributes":{"productId":"premium","subscriptionPeriod":"ONE_MONTH"}},
        {"type":"subscriptions","id":"s2","attributes":{"productId":"vip_year","subscriptionPeriod":"ONE_YEAR"}}]}"#);

    let v = list(&w).unwrap();
    assert_eq!(v["store"], "apple");
    let row = |id: &str| v["products"].as_array().unwrap().iter().find(|r| r["product_id"] == id).cloned().unwrap();
    assert_eq!((row("gems")["status"].as_str(), row("gems")["kind"].as_str()), (Some("same"), Some("consumable")));
    assert_eq!((row("premium")["status"].as_str(), row("premium")["billing_period_days"].as_i64()), (Some("same"), Some(30)));
    assert_eq!((row("no_ads")["status"].as_str(), row("no_ads")["kind"].as_str()), (Some("new"), Some("non_consumable")));
    assert_eq!(row("vip_year")["status"], "new");
    assert_eq!(v["missing_in_store"], json!([]));
    assert_eq!(w.calls("api.appstoreconnect.apple.com"), 4);

    // The token is cached: a second import signs nothing new and lists again.
    list(&w).unwrap();
    assert_eq!(w.calls("api.appstoreconnect.apple.com"), 8);
}

#[test]
fn app_store_catalog_explains_a_refused_key() {
    let w = apple_offer_world(true);
    w.as_admin(false);
    w.set_secret(secret_name::APPLE_CONNECT_KEY_ID, "TEAMKEY123");
    w.set_secret(secret_name::APPLE_CONNECT_PRIVATE_KEY_P8, &test_p8(9));
    w.script("/v1/apps?filter", 403, r#"{"errors":[{"status":"403"}]}"#);
    let e = crate::catalog::list_store_products(&mut w.ctx(), IOS_APP).unwrap_err();
    assert!(e.contains("App Manager"), "{e}");
}
