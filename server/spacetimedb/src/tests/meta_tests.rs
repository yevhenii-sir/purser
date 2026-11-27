//! Meta Horizon Store (Meta Quest).

use super::*;

pub(super) const META_APP: &str = "com.studio.game.quest";
pub(super) const META_USER: &str = "5550001";

/// A Quest build of the game (an Android build in the Meta Horizon Store) with its App ID and secret.
pub(super) fn add_meta_build(w: &World) {
    w.as_admin(false);
    let app = json!({
        "app_id": META_APP, "platform": "android", "store": "meta", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke", "allow_sandbox": false,
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": META_APP, "product_id": "gems", "kind": "consumable" },
        { "app_id": META_APP, "product_id": "no_ads", "kind": "non_consumable" },
        { "app_id": META_APP, "product_id": "vip", "kind": "subscription" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    w.set_secret(purser_core::meta::APP_ID, "1234");
    w.set_secret(purser_core::meta::APP_SECRET, "meta-app-secret");
    // A Meta build needs no Google key, for the consume step either.
    let google = config::credential_key(config::DEFAULT_SCOPE, secret_name::GOOGLE_SERVICE_ACCOUNT_JSON);
    w.write(|tx| {
        tx.db.credential().key().delete(&google);
    });
    w.script("user_nonce_validate", 200, r#"{"is_valid":true}"#);
}

/// `viewer_purchases` answers with these purchases.
pub(super) fn meta_owns(w: &World, purchases: Value) {
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "viewer_purchases");
    w.script("viewer_purchases", 200, &json!({ "data": purchases, "paging": { "cursors": {} } }).to_string());
}

pub(super) fn meta_verify(w: &World, user: &str, product_id: &str, proof: &str) -> Outcome {
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": META_APP, "user_id": user, "product_id": product_id, "store_user_id": META_USER, "store_user_proof": proof,
    }))
    .unwrap();
    verify(&mut w.ctx(), "meta", &r)
}

#[test]
fn meta_purchases_are_proven_verified_granted_and_consumed() {
    let w = World::new();
    add_meta_build(&w);
    w.script("consume_entitlement", 200, r#"{"success":true}"#);
    meta_owns(&w, json!([{ "id": "9001", "grant_time": 1_789_000_000, "expiration_time": 0, "item": { "sku": "gems", "id": "77" } }]));

    let o = meta_verify(&w, "player-1", "gems", "");
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("error")), "the user proof is required: {}", o.body);
    let o = meta_verify(&w, "player-1", "gems", "nonce-1");
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")), "{}", o.body);
    let p = find(&w, "meta:9001").expect("ledger row");
    assert_eq!((p.status.as_str(), p.purchase_token.as_str(), p.environment.as_str()), (status::COMPLETED, META_USER, "production"));
    assert_eq!(w.calls("/call/billing_grant"), 1);
    let calls = w.fake.borrow().calls.clone();
    let proof = calls.iter().find(|c| c.contains("user_nonce_validate")).unwrap();
    assert!(proof.starts_with("POST https://graph.oculus.com/user_nonce_validate"), "{proof}");
    assert!(proof.contains("nonce=nonce-1&user_id=5550001"), "{proof}");
    let consume = calls.iter().find(|c| c.contains("consume_entitlement")).unwrap();
    assert!(consume.starts_with("POST https://graph.oculus.com/1234/consume_entitlement"), "{consume}");
    assert!(consume.ends_with("&user_id=5550001&sku=gems"), "{consume}");
    assert!(calls.iter().any(|c| c.starts_with("GET https://graph.oculus.com/1234/viewer_purchases?")));
    assert_eq!(meta_verify(&w, "player-2", "gems", "nonce-2").http, 403, "another player cannot claim it");

    // A proof Meta refuses, and a SKU the user does not own, are invalid; nothing is granted.
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "user_nonce_validate");
    w.script("user_nonce_validate", 200, r#"{"is_valid":false}"#);
    let o = meta_verify(&w, "player-3", "no_ads", "stale");
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("invalid")), "{}", o.body);
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "user_nonce_validate");
    w.script("user_nonce_validate", 200, r#"{"is_valid":true}"#);
    let o = meta_verify(&w, "player-3", "no_ads", "nonce-3");
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("invalid")), "{}", o.body);

    // Rejected app credentials are a setup error, retried by the client, never `invalid`.
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "viewer_purchases");
    w.script("viewer_purchases", 400, r#"{"error":{"message":"Invalid OAuth access token","type":"OAuthException","code":190}}"#);
    let o = meta_verify(&w, "player-3", "no_ads", "nonce-4");
    assert_eq!((o.http, o.body["status"].as_str()), (500, Some("error")), "{}", o.body);
}

#[test]
fn a_meta_consume_that_was_done_already_counts_and_restore_proves_the_user_once() {
    let w = World::new();
    add_meta_build(&w);
    // Meta refuses the consume; the purchase decides whether it was done (by the client SDK) or not.
    w.script("consume_entitlement", 400, r#"{"error":{"message":"Nothing to consume","code":100}}"#);
    let gems = json!({ "id": "9002", "grant_time": 1_789_000_000, "item": { "sku": "gems" } });
    let no_ads = json!({ "id": "9003", "grant_time": 1_788_000_000, "item": { "sku": "no_ads" } });
    meta_owns(&w, json!([gems, no_ads.clone()]));
    let r: RestoreRequest = serde_json::from_value(json!({
        "app_id": META_APP, "user_id": "player-1", "store_user_id": META_USER, "store_user_proof": "nonce-1",
        "purchases": [{ "product_id": "gems", "order_id": "9002" }, { "product_id": "no_ads", "order_id": "9003" }],
    }))
    .unwrap();
    let o = restore_purchases(&mut w.ctx(), &r).unwrap_or_else(|o| o);
    assert_eq!(o.http, 200, "{}", o.body);
    assert_eq!(w.calls("user_nonce_validate"), 1, "one proof for every item: {:?}", o.body["results"]);
    assert_eq!(o.body["results"][1]["status"], "success", "{}", o.body);
    // gems: the consume was refused while still listed, so the step is retried later.
    assert_ne!(find(&w, "meta:9002").unwrap().status, status::COMPLETED);
    meta_owns(&w, json!([no_ads]));
    w.advance_clock(3600);
    w.run_worker();
    assert_eq!(find(&w, "meta:9002").unwrap().status, status::COMPLETED, "no longer listed: consumed");
    assert_eq!(find(&w, "meta:9003").unwrap().status, status::COMPLETED);
}

#[test]
fn meta_subscriptions_are_rechecked_when_their_period_ends() {
    use crate::poll::store_poll;
    let w = World::new();
    add_meta_build(&w);
    let now_s = NOW_MICROS / 1_000_000;
    let period = |start: i64, end: i64, active: bool| {
        json!({ "data": [{ "id": "S1", "sku": "vip", "owner": { "id": META_USER }, "is_active": active, "is_trial": false,
            "period_start_time": start, "period_end_time": end,
            "current_price_term": { "term": "MONTHLY", "price": "9.99", "currency": "USD" } }] })
        .to_string()
    };
    let end = now_s + 86_400;
    w.script("application/subscriptions", 200, &period(now_s - 29 * 86_400, end, true));
    let o = meta_verify(&w, "player-1", "vip", "nonce-1");
    assert_eq!(o.body["status"], "success", "{}", o.body);
    let first = format!("meta:S1.{}", now_s - 29 * 86_400);
    let p = find(&w, &first).expect("the period's row");
    assert_eq!((p.original_transaction_id.as_str(), p.expires_at_ms, p.price_micros, p.currency.as_str()), ("S1", end * 1000, 9_990_000, "USD"));
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("application/subscriptions")).cloned().unwrap();
    assert!(call.starts_with("GET https://graph.oculus.com/application/subscriptions?") && call.contains("owner_id=5550001&skus=vip"), "{call}");
    let poll = |w: &World, id: &str| {
        w.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.store_poll().purchase_id().find(id.to_string()))).unwrap()
    };
    assert!(poll(&w, &first).is_some(), "Meta sends no notifications: the worker re-checks it");

    // Renewed: a new period is a row of its own, granted again.
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "application/subscriptions");
    let next_end = end + 30 * 86_400;
    w.script("application/subscriptions", 200, &period(end, next_end, true));
    w.advance_clock(86_400 + 11 * 60);
    w.run_worker();
    let renewal = find(&w, &format!("meta:S1.{end}")).expect("the renewal");
    assert_eq!((renewal.status.as_str(), renewal.reason.as_str(), renewal.expires_at_ms), (status::COMPLETED, "renewal", next_end * 1000));
    assert_eq!(w.calls("user_nonce_validate"), 1, "re-checks need no user proof");

    // Ended: expired in the game.
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "application/subscriptions");
    w.script("application/subscriptions", 200, &period(end, next_end, false));
    w.advance_clock(30 * 86_400 + 11 * 60);
    w.run_worker();
    assert_eq!(find(&w, &format!("meta:S1.{end}")).unwrap().status, status::EXPIRED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 expire"));
}

#[test]
fn the_meta_setup_check_asks_for_the_apps_add_ons() {
    let w = World::new();
    add_meta_build(&w);
    let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": true })).unwrap();
    let level = |w: &World| {
        let report = crate::doctor::check_app(&mut w.ctx(), META_APP, &opts).unwrap();
        report["checks"].as_array().unwrap().iter().find(|c| c["id"] == "meta_api").map(|c| c["level"].as_str().unwrap().to_string())
    };
    w.script("available_purchases", 200, r#"{"data":[{"id":"1","sku":"gems"}]}"#);
    assert_eq!(level(&w).as_deref(), Some("ok"));
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "available_purchases");
    w.script("available_purchases", 400, r#"{"error":{"type":"OAuthException","code":190}}"#);
    assert_eq!(level(&w).as_deref(), Some("fail"));
    w.set_secret(purser_core::meta::APP_ID, "not-a-number");
    assert_eq!(level(&w).as_deref(), Some("fail"));
}
